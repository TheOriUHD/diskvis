//! Integrated text editor for diskvis.
//!
//! Owns its own document state, undo/redo, cursor logic and syntax-highlight
//! cache. The TUI layer is responsible only for invoking [`Editor`] keystroke
//! handlers and drawing its current state via [`Editor::view_lines`] etc.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, Style as SynStyle, ThemeSet,
};
use syntect::parsing::{SyntaxReference, SyntaxSet};

const TAB_WIDTH: usize = 4;
const UNDO_BATCH_MS: u128 = 600;
const MAX_UNDO: usize = 200;

/// Largest file the editor will load without warning.
pub const SOFT_FILE_LIMIT: u64 = 10 * 1024 * 1024;

/// Result of a key event handled by the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorOutcome {
    /// Stay in the editor.
    Continue,
    /// Close the editor (return to main view).
    Close,
}

/// Modal prompt overlays inside the editor.
#[derive(Debug, Clone)]
pub enum EditorPrompt {
    /// Prompt the user to confirm closing with unsaved changes.
    /// Buttons: Save & Close / Discard / Cancel.
    UnsavedClose { selection: u8 },
    /// Goto-line input at the bottom.
    GotoLine { input: String },
    /// Status / error message shown briefly at the bottom.
    Message { text: String },
    /// Confirmation that the file is large or appears binary.
    LargeFile { selection: u8, size: u64 },
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
}

pub struct Editor {
    pub path: PathBuf,
    pub lines: Vec<String>,
    /// (row, col) — col is byte index into the row's string.
    pub cursor: (usize, usize),
    /// Top row of the visible window.
    pub scroll_row: usize,
    /// Left column of the visible window (in display columns).
    pub scroll_col: usize,
    pub modified: bool,
    pub language: String,
    #[allow(dead_code)]
    pub syntax_name: String,
    pub prompt: Option<EditorPrompt>,
    /// Whether the load was forced past a binary/large warning.
    pub force_loaded: bool,

    syntax_set: SyntaxSet,
    theme: syntect::highlighting::Theme,
    syntax: SyntaxReference,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    last_edit_at: Option<Instant>,
    last_size: (u16, u16),
}

impl Editor {
    /// Try to open `path`. Returns `Err(message)` for unreadable paths.
    /// Files exceeding [`SOFT_FILE_LIMIT`] or that look binary set the
    /// [`EditorPrompt::LargeFile`] prompt; the caller can then either honour
    /// it (by leaving `force_loaded=false` and showing the prompt) or call
    /// [`Editor::confirm_load`] to keep editing anyway.
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let meta = fs::metadata(&path)
            .map_err(|e| format!("cannot stat {}: {}", path.display(), e))?;
        let size = meta.len();

        let bytes = fs::read(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;

        let looks_binary = bytes.iter().take(8192).any(|b| *b == 0);

        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
        };

        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.trim_end_matches('\r').to_string()).collect()
        };
        // split('\n') leaves a trailing empty line for files ending in \n;
        // keep it so the user sees the final newline as an empty bottom line.

        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_else(|| theme_set.themes.values().next().cloned().unwrap());

        let syntax = pick_syntax(&syntax_set, &path);
        let language = syntax.name.clone();
        let syntax_name = syntax.name.clone();

        let mut ed = Editor {
            path,
            lines,
            cursor: (0, 0),
            scroll_row: 0,
            scroll_col: 0,
            modified: false,
            language,
            syntax_name,
            prompt: None,
            force_loaded: false,
            syntax_set,
            theme,
            syntax: syntax.clone(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_edit_at: None,
            last_size: (80, 24),
        };

        let needs_warn = looks_binary || size > SOFT_FILE_LIMIT;
        if needs_warn {
            ed.prompt = Some(EditorPrompt::LargeFile {
                selection: 1, // default: Cancel
                size,
            });
        }
        Ok(ed)
    }

    pub fn confirm_load(&mut self) {
        self.force_loaded = true;
        self.prompt = None;
    }

    pub fn report_size(&mut self, w: u16, h: u16) {
        self.last_size = (w, h);
    }

    /// Single-line title bar text: `path  ·  language  ·  L:C  [modified]`.
    pub fn title(&self) -> String {
        let m = if self.modified { "  [modified]" } else { "" };
        format!(
            "{}  ·  {}  ·  L{}:C{}{}",
            self.path.display(),
            self.language,
            self.cursor.0 + 1,
            self.cursor.1 + 1,
            m,
        )
    }

    /// Hint bar shown at the bottom of the editor.
    pub fn hint(&self) -> &'static str {
        "Ctrl-S save  Ctrl-W save & close  Ctrl-Q close  Ctrl-Z/Y undo/redo  Ctrl-D dup  Ctrl-K kill  Ctrl-G goto"
    }

    // ------------------------------------------------------------------
    // Persistence
    // ------------------------------------------------------------------

    /// Atomic save: write to `<path>.diskvis.tmp` then rename over the target.
    pub fn save(&mut self) -> Result<(), String> {
        let body = self.lines.join("\n");
        let tmp_name = format!(
            ".{}.diskvis-{}.tmp",
            self.path.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
            std::process::id()
        );
        let tmp_path = self
            .path
            .parent()
            .map(|p| p.join(&tmp_name))
            .unwrap_or_else(|| PathBuf::from(&tmp_name));
        fs::write(&tmp_path, body.as_bytes()).map_err(|e| {
            format!("write {}: {}", tmp_path.display(), e)
        })?;
        fs::rename(&tmp_path, &self.path).map_err(|e| {
            // Best effort: drop the temp file if rename failed.
            let _ = fs::remove_file(&tmp_path);
            format!("rename: {}", e)
        })?;
        self.modified = false;
        self.flash(format!("saved {}", self.path.display()));
        Ok(())
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.prompt = Some(EditorPrompt::Message {
            text: text.into(),
        });
    }

    // ------------------------------------------------------------------
    // Undo / redo
    // ------------------------------------------------------------------

    fn push_snapshot(&mut self) {
        let now = Instant::now();
        // Batch rapid edits into a single undo step.
        if let Some(last) = self.last_edit_at {
            if now.duration_since(last).as_millis() < UNDO_BATCH_MS
                && !self.undo_stack.is_empty()
            {
                self.last_edit_at = Some(now);
                return;
            }
        }
        self.last_edit_at = Some(now);
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
        if self.undo_stack.len() > MAX_UNDO {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        let Some(snap) = self.undo_stack.pop() else {
            self.flash("nothing to undo");
            return;
        };
        self.redo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
        self.lines = snap.lines;
        self.cursor = snap.cursor;
        self.modified = true;
        self.last_edit_at = None;
    }

    fn redo(&mut self) {
        let Some(snap) = self.redo_stack.pop() else {
            self.flash("nothing to redo");
            return;
        };
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        });
        self.lines = snap.lines;
        self.cursor = snap.cursor;
        self.modified = true;
        self.last_edit_at = None;
    }

    // ------------------------------------------------------------------
    // Cursor movement helpers
    // ------------------------------------------------------------------

    #[allow(dead_code)]
    fn clamp_cursor(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        if self.cursor.0 >= self.lines.len() {
            self.cursor.0 = self.lines.len() - 1;
        }
        let line_len = self.lines[self.cursor.0].len();
        if self.cursor.1 > line_len {
            self.cursor.1 = line_len;
        }
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map(|s| s.len()).unwrap_or(0)
    }

    fn cur_line(&self) -> &str {
        self.lines
            .get(self.cursor.0)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    fn move_word(&mut self, forward: bool) {
        let (mut r, mut c) = self.cursor;
        if forward {
            loop {
                let line: &[u8] = self.lines[r].as_bytes();
                if c >= line.len() {
                    if r + 1 < self.lines.len() {
                        r += 1;
                        c = 0;
                        continue;
                    }
                    break;
                }
                // Skip whitespace then run of word chars.
                while c < line.len() && (line[c] as char).is_whitespace() {
                    c += 1;
                }
                let start = c;
                while c < line.len() && !(line[c] as char).is_whitespace() {
                    c += 1;
                }
                if c != start {
                    break;
                }
            }
        } else {
            loop {
                if c == 0 {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                    c = self.line_len(r);
                    continue;
                }
                let line = self.lines[r].as_bytes();
                // Step left over whitespace.
                while c > 0 && (line[c - 1] as char).is_whitespace() {
                    c -= 1;
                }
                let end = c;
                while c > 0 && !(line[c - 1] as char).is_whitespace() {
                    c -= 1;
                }
                if c != end {
                    break;
                }
            }
        }
        self.cursor = (r, c);
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        let h = self.last_size.1.saturating_sub(3) as usize; // title + hint + status
        let h = h.max(1);
        if self.cursor.0 < self.scroll_row {
            self.scroll_row = self.cursor.0;
        } else if self.cursor.0 >= self.scroll_row + h {
            self.scroll_row = self.cursor.0 + 1 - h;
        }

        let gutter = self.gutter_w();
        let w = self.last_size.0.saturating_sub(gutter as u16) as usize;
        let w = w.max(1);
        let display_col = display_col_of(self.cur_line(), self.cursor.1);
        if display_col < self.scroll_col {
            self.scroll_col = display_col;
        } else if display_col >= self.scroll_col + w {
            self.scroll_col = display_col + 1 - w;
        }
    }

    // ------------------------------------------------------------------
    // Editing primitives
    // ------------------------------------------------------------------

    fn insert_char(&mut self, c: char) {
        self.push_snapshot();
        let (r, col) = self.cursor;
        self.lines[r].insert(col, c);
        self.cursor.1 = col + c.len_utf8();
        self.modified = true;
        self.ensure_visible();
    }

    fn insert_str(&mut self, s: &str) {
        self.push_snapshot();
        let (r, col) = self.cursor;
        self.lines[r].insert_str(col, s);
        self.cursor.1 = col + s.len();
        self.modified = true;
        self.ensure_visible();
    }

    fn split_line(&mut self) {
        self.push_snapshot();
        let (r, col) = self.cursor;
        let rest = self.lines[r].split_off(col);
        // Determine indentation of the current (now-truncated) line for auto-indent.
        let indent: String = self.lines[r]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let mut new_line = indent.clone();
        new_line.push_str(&rest);
        self.lines.insert(r + 1, new_line);
        self.cursor = (r + 1, indent.len());
        self.modified = true;
        self.ensure_visible();
    }

    fn backspace(&mut self) {
        let (r, col) = self.cursor;
        if col == 0 {
            if r == 0 {
                return;
            }
            self.push_snapshot();
            let line = self.lines.remove(r);
            let prev_len = self.lines[r - 1].len();
            self.lines[r - 1].push_str(&line);
            self.cursor = (r - 1, prev_len);
            self.modified = true;
            return;
        }
        self.push_snapshot();
        // Step back one char (handle multi-byte).
        let line = &self.lines[r];
        let mut new_col = col;
        while new_col > 0 && !line.is_char_boundary(new_col - 1) {
            new_col -= 1;
        }
        new_col = new_col.saturating_sub(1);
        self.lines[r].replace_range(new_col..col, "");
        self.cursor.1 = new_col;
        self.modified = true;
        self.ensure_visible();
    }

    fn delete_forward(&mut self) {
        let (r, col) = self.cursor;
        let line_len = self.line_len(r);
        if col == line_len {
            if r + 1 >= self.lines.len() {
                return;
            }
            self.push_snapshot();
            let next = self.lines.remove(r + 1);
            self.lines[r].push_str(&next);
            self.modified = true;
            return;
        }
        self.push_snapshot();
        let line = &self.lines[r];
        let mut end = col + 1;
        while end < line.len() && !line.is_char_boundary(end) {
            end += 1;
        }
        self.lines[r].replace_range(col..end, "");
        self.modified = true;
    }

    fn duplicate_line(&mut self) {
        self.push_snapshot();
        let r = self.cursor.0;
        let copy = self.lines[r].clone();
        self.lines.insert(r + 1, copy);
        self.cursor.0 = r + 1;
        self.modified = true;
        self.ensure_visible();
    }

    fn kill_line(&mut self) {
        self.push_snapshot();
        let r = self.cursor.0;
        if self.lines.len() == 1 {
            self.lines[0].clear();
            self.cursor = (0, 0);
        } else {
            self.lines.remove(r);
            if self.cursor.0 >= self.lines.len() {
                self.cursor.0 = self.lines.len() - 1;
            }
            self.cursor.1 = 0;
        }
        self.modified = true;
        self.ensure_visible();
    }

    fn dedent_line(&mut self) {
        self.push_snapshot();
        let r = self.cursor.0;
        let line = &mut self.lines[r];
        let mut removed = 0;
        while removed < TAB_WIDTH && line.starts_with(' ') {
            line.remove(0);
            removed += 1;
        }
        if removed == 0 && line.starts_with('\t') {
            line.remove(0);
            removed = 1;
        }
        if self.cursor.1 >= removed {
            self.cursor.1 -= removed;
        } else {
            self.cursor.1 = 0;
        }
        if removed > 0 {
            self.modified = true;
        }
    }

    // ------------------------------------------------------------------
    // Key handling
    // ------------------------------------------------------------------

    /// Returns [`EditorOutcome::Close`] when the editor should be torn down.
    pub fn handle_key(&mut self, key: KeyEvent) -> EditorOutcome {
        if key.kind != KeyEventKind::Press {
            return EditorOutcome::Continue;
        }

        // --- Prompts intercept input ---
        if let Some(prompt) = self.prompt.clone() {
            return self.handle_prompt_key(prompt, key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match (key.code, ctrl) {
            (KeyCode::Char('q'), true) => return self.request_close(),
            (KeyCode::Char('s'), true) => {
                if let Err(e) = self.save() {
                    self.flash(format!("save failed: {}", e));
                }
                return EditorOutcome::Continue;
            }
            (KeyCode::Char('w'), true) => {
                if let Err(e) = self.save() {
                    self.flash(format!("save failed: {}", e));
                    return EditorOutcome::Continue;
                }
                return EditorOutcome::Close;
            }
            (KeyCode::Char('z'), true) => {
                self.undo();
                return EditorOutcome::Continue;
            }
            (KeyCode::Char('y'), true) => {
                self.redo();
                return EditorOutcome::Continue;
            }
            (KeyCode::Char('d'), true) => {
                self.duplicate_line();
                return EditorOutcome::Continue;
            }
            (KeyCode::Char('k'), true) => {
                self.kill_line();
                return EditorOutcome::Continue;
            }
            (KeyCode::Char('g'), true) => {
                self.prompt = Some(EditorPrompt::GotoLine { input: String::new() });
                return EditorOutcome::Continue;
            }
            _ => {}
        }

        match key.code {
            KeyCode::Esc => return self.request_close(),
            KeyCode::Left if ctrl => self.move_word(false),
            KeyCode::Right if ctrl => self.move_word(true),
            KeyCode::Left => {
                if self.cursor.1 > 0 {
                    let line = &self.lines[self.cursor.0];
                    let mut c = self.cursor.1 - 1;
                    while c > 0 && !line.is_char_boundary(c) {
                        c -= 1;
                    }
                    self.cursor.1 = c;
                } else if self.cursor.0 > 0 {
                    self.cursor.0 -= 1;
                    self.cursor.1 = self.line_len(self.cursor.0);
                }
                self.ensure_visible();
            }
            KeyCode::Right => {
                let len = self.line_len(self.cursor.0);
                if self.cursor.1 < len {
                    let line = &self.lines[self.cursor.0];
                    let mut c = self.cursor.1 + 1;
                    while c < line.len() && !line.is_char_boundary(c) {
                        c += 1;
                    }
                    self.cursor.1 = c;
                } else if self.cursor.0 + 1 < self.lines.len() {
                    self.cursor.0 += 1;
                    self.cursor.1 = 0;
                }
                self.ensure_visible();
            }
            KeyCode::Up => {
                if self.cursor.0 > 0 {
                    self.cursor.0 -= 1;
                    self.cursor.1 = self.cursor.1.min(self.line_len(self.cursor.0));
                }
                self.ensure_visible();
            }
            KeyCode::Down => {
                if self.cursor.0 + 1 < self.lines.len() {
                    self.cursor.0 += 1;
                    self.cursor.1 = self.cursor.1.min(self.line_len(self.cursor.0));
                }
                self.ensure_visible();
            }
            KeyCode::Home => {
                self.cursor.1 = 0;
                self.ensure_visible();
            }
            KeyCode::End => {
                self.cursor.1 = self.line_len(self.cursor.0);
                self.ensure_visible();
            }
            KeyCode::PageUp => {
                let h = (self.last_size.1.saturating_sub(3) as usize).max(1);
                self.cursor.0 = self.cursor.0.saturating_sub(h);
                self.cursor.1 = self.cursor.1.min(self.line_len(self.cursor.0));
                self.ensure_visible();
            }
            KeyCode::PageDown => {
                let h = (self.last_size.1.saturating_sub(3) as usize).max(1);
                self.cursor.0 = (self.cursor.0 + h).min(self.lines.len().saturating_sub(1));
                self.cursor.1 = self.cursor.1.min(self.line_len(self.cursor.0));
                self.ensure_visible();
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Enter => self.split_line(),
            KeyCode::Tab => self.insert_str(&" ".repeat(TAB_WIDTH)),
            KeyCode::BackTab => self.dedent_line(),
            KeyCode::Char(c) => {
                if shift && c.is_ascii_alphabetic() && !ctrl {
                    // Shift+letter is already a capital from the terminal.
                    self.insert_char(c);
                } else if ctrl {
                    // Unhandled control key — ignore.
                } else {
                    self.insert_char(c);
                }
            }
            _ => {}
        }
        EditorOutcome::Continue
    }

    fn request_close(&mut self) -> EditorOutcome {
        if self.modified {
            self.prompt = Some(EditorPrompt::UnsavedClose { selection: 0 });
            EditorOutcome::Continue
        } else {
            EditorOutcome::Close
        }
    }

    fn handle_prompt_key(&mut self, prompt: EditorPrompt, key: KeyEvent) -> EditorOutcome {
        match prompt {
            EditorPrompt::UnsavedClose { mut selection } => match key.code {
                KeyCode::Esc => {
                    self.prompt = None;
                    EditorOutcome::Continue
                }
                KeyCode::Left => {
                    selection = selection.saturating_sub(1);
                    self.prompt = Some(EditorPrompt::UnsavedClose { selection });
                    EditorOutcome::Continue
                }
                KeyCode::Right | KeyCode::Tab => {
                    selection = (selection + 1).min(2);
                    self.prompt = Some(EditorPrompt::UnsavedClose { selection });
                    EditorOutcome::Continue
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.prompt = None;
                    match selection {
                        0 => {
                            // Save & Close
                            if let Err(e) = self.save() {
                                self.flash(format!("save failed: {}", e));
                                return EditorOutcome::Continue;
                            }
                            EditorOutcome::Close
                        }
                        1 => EditorOutcome::Close,
                        _ => EditorOutcome::Continue,
                    }
                }
                _ => {
                    self.prompt = Some(EditorPrompt::UnsavedClose { selection });
                    EditorOutcome::Continue
                }
            },
            EditorPrompt::GotoLine { mut input } => match key.code {
                KeyCode::Esc => {
                    self.prompt = None;
                    EditorOutcome::Continue
                }
                KeyCode::Enter => {
                    if let Ok(n) = input.trim().parse::<usize>() {
                        let idx = n.saturating_sub(1).min(self.lines.len().saturating_sub(1));
                        self.cursor = (idx, 0);
                        self.ensure_visible();
                    } else {
                        self.flash("not a number");
                    }
                    self.prompt = None;
                    EditorOutcome::Continue
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.prompt = Some(EditorPrompt::GotoLine { input });
                    EditorOutcome::Continue
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    input.push(c);
                    self.prompt = Some(EditorPrompt::GotoLine { input });
                    EditorOutcome::Continue
                }
                _ => {
                    self.prompt = Some(EditorPrompt::GotoLine { input });
                    EditorOutcome::Continue
                }
            },
            EditorPrompt::LargeFile { mut selection, size } => match key.code {
                KeyCode::Esc => EditorOutcome::Close,
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    selection ^= 1;
                    self.prompt = Some(EditorPrompt::LargeFile { selection, size });
                    EditorOutcome::Continue
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if selection == 0 {
                        self.confirm_load();
                        EditorOutcome::Continue
                    } else {
                        EditorOutcome::Close
                    }
                }
                _ => {
                    self.prompt = Some(EditorPrompt::LargeFile { selection, size });
                    EditorOutcome::Continue
                }
            },
            EditorPrompt::Message { .. } => {
                // Any keypress dismisses the flash message. The keystroke
                // itself is consumed (no recursion into `handle_key` — that
                // produced an infinite loop because the prompt was still
                // observed by the recursive call before being cleared).
                self.prompt = None;
                EditorOutcome::Continue
            }
        }
    }

    /// Width of the line-number gutter in display columns.
    pub fn gutter_w(&self) -> usize {
        let max = self.lines.len().max(1);
        let digits = max.to_string().len();
        digits + 2
    }

    /// Build the visible lines as styled `ratatui::text::Line` values for the
    /// caller to render directly.
    pub fn view_lines(&self, height: u16, width: u16) -> Vec<Line<'static>> {
        let h = height as usize;
        let gutter = self.gutter_w();
        let body_w = (width as usize).saturating_sub(gutter);
        let mut out = Vec::with_capacity(h);

        let total = self.lines.len();
        if total == 0 {
            return out;
        }

        // Pre-build a HighlightLines per-call: the highlight state is reset
        // at the top of the file, and we need to feed every line up to the
        // window so multi-line highlights stay correct. For very large files
        // we still re-highlight from scroll_row to keep things bounded.
        let mut hl = HighlightLines::new(&self.syntax, &self.theme);
        let from = 0; // start from top so syntax is correct
        let to = (self.scroll_row + h).min(total);

        // Highlight all lines from 0..to; cache only the ones we need to display.
        let mut highlighted: Vec<Vec<(SynStyle, String)>> = Vec::with_capacity(to);
        for i in from..to {
            let mut line = self.lines[i].clone();
            line.push('\n');
            let regions = hl
                .highlight_line(&line, &self.syntax_set)
                .map(|v| {
                    v.into_iter()
                        .map(|(s, t)| (s, t.trim_end_matches('\n').to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|_| vec![(default_syn_style(), self.lines[i].clone())]);
            highlighted.push(regions);
        }

        for visible_idx in 0..h {
            let row = self.scroll_row + visible_idx;
            if row >= total {
                out.push(Line::from(""));
                continue;
            }
            let regions = &highlighted[row];
            let is_cursor_row = row == self.cursor.0;

            let line_no = format!("{:>width$}  ", row + 1, width = gutter - 2);
            let gutter_style = if is_cursor_row {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled(line_no, gutter_style));

            // Render highlighted regions, applying the horizontal scroll.
            // We work in display columns since tabs may expand.
            let mut col_disp = 0usize;
            let body_end = self.scroll_col + body_w;

            for (sty, text) in regions {
                if text.is_empty() {
                    continue;
                }
                let style = syn_to_ratatui(sty, is_cursor_row);
                // Walk the text char-by-char to slice the visible window.
                let mut buf = String::new();
                for ch in text.chars() {
                    let ch_w = if ch == '\t' { TAB_WIDTH - col_disp % TAB_WIDTH } else { 1 };
                    let next = col_disp + ch_w;
                    if next <= self.scroll_col {
                        col_disp = next;
                        continue;
                    }
                    if col_disp >= body_end {
                        break;
                    }
                    if ch == '\t' {
                        let visible = ch_w.min(body_end - col_disp);
                        for _ in 0..visible {
                            buf.push(' ');
                        }
                    } else {
                        buf.push(ch);
                    }
                    col_disp = next;
                }
                if !buf.is_empty() {
                    spans.push(Span::styled(buf, style));
                }
                if col_disp >= body_end {
                    break;
                }
            }

            // Pad current line with a subtle highlight to span full width.
            if is_cursor_row {
                let used_disp = col_disp.saturating_sub(self.scroll_col);
                if used_disp < body_w {
                    spans.push(Span::styled(
                        " ".repeat(body_w - used_disp),
                        Style::default().bg(Color::Rgb(30, 30, 45)),
                    ));
                }
                // Inject the cursor block at the correct visible column.
                let cursor_disp = display_col_of(self.cur_line(), self.cursor.1);
                let cursor_visible = cursor_disp.saturating_sub(self.scroll_col);
                let cursor_visible = cursor_visible.min(body_w.saturating_sub(1));
                let bg = Color::Rgb(30, 30, 45);
                let cursor_block = Span::styled(
                    "█".to_string(),
                    Style::default()
                        .fg(Color::Rgb(255, 200, 0))
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                );

                // Walk the existing spans (gutter at index 0 is preserved
                // verbatim) counting display columns within the body, and
                // splice the cursor block in at `cursor_visible`. The
                // underlying char under the cursor is overwritten by the
                // block.
                let mut new_spans: Vec<Span<'static>> = Vec::with_capacity(spans.len() + 2);
                if let Some(g) = spans.first() {
                    new_spans.push(Span::styled(g.content.clone().into_owned(), g.style));
                }
                let mut col = 0usize;
                let mut inserted = false;
                for span in spans.iter().skip(1) {
                    let style = span.style.bg(bg);
                    if inserted {
                        new_spans.push(Span::styled(span.content.clone().into_owned(), style));
                        continue;
                    }
                    let text: &str = &span.content;
                    let span_chars: Vec<char> = text.chars().collect();
                    let mut before = String::new();
                    let mut consumed = 0usize;
                    let mut hit = false;
                    for (i, ch) in span_chars.iter().enumerate() {
                        if col == cursor_visible {
                            if !before.is_empty() {
                                new_spans.push(Span::styled(before.clone(), style));
                            }
                            new_spans.push(cursor_block.clone());
                            // Skip the character under the cursor — the block
                            // visually replaces it.
                            consumed = i + 1;
                            col += 1;
                            hit = true;
                            break;
                        }
                        before.push(*ch);
                        consumed += 1;
                        col += 1;
                    }
                    if hit {
                        let rest: String = span_chars[consumed..].iter().collect();
                        if !rest.is_empty() {
                            new_spans.push(Span::styled(rest, style));
                        }
                        inserted = true;
                    } else {
                        if !before.is_empty() {
                            new_spans.push(Span::styled(before, style));
                        }
                    }
                }
                if !inserted {
                    // Cursor sits past the end of rendered content (empty
                    // line or past EOL): pad with spaces and emit the block.
                    let pad = cursor_visible.saturating_sub(col);
                    if pad > 0 {
                        new_spans.push(Span::styled(
                            " ".repeat(pad),
                            Style::default().bg(bg),
                        ));
                    }
                    new_spans.push(cursor_block);
                }
                out.push(Line::from(new_spans));
            } else {
                out.push(Line::from(spans));
            }
        }

        out
    }
}

fn pick_syntax(set: &SyntaxSet, path: &Path) -> SyntaxReference {
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        if let Some(s) = set.find_syntax_by_extension(ext) {
            return s.clone();
        }
    }
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if let Some(s) = set.find_syntax_by_extension(name) {
            return s.clone();
        }
    }
    set.find_syntax_plain_text().clone()
}

fn default_syn_style() -> SynStyle {
    SynStyle {
        foreground: SynColor {
            r: 220,
            g: 220,
            b: 220,
            a: 0xff,
        },
        background: SynColor {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        },
        font_style: FontStyle::empty(),
    }
}

fn syn_to_ratatui(s: &SynStyle, _cursor_row: bool) -> Style {
    let mut style = Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// Translate a byte offset into a display column, expanding tabs.
fn display_col_of(line: &str, byte_col: usize) -> usize {
    let mut disp = 0usize;
    for (i, ch) in line.char_indices() {
        if i >= byte_col {
            break;
        }
        if ch == '\t' {
            disp += TAB_WIDTH - disp % TAB_WIDTH;
        } else {
            disp += 1;
        }
    }
    disp
}

// Drop temp files if instance is forgotten mid-save (best-effort).
impl Drop for Editor {
    fn drop(&mut self) {
        // Nothing currently — temp files are cleaned in `save`.
    }
}

// Suppress unused warnings on internal struct fields when only a few code
// paths exercise them.
#[allow(dead_code)]
fn _touch(_: &Snapshot) {}
