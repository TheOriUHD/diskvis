use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;

use crate::cli::{Mode, SortBy, SortOrder};
use crate::config::{Config, Theme};
use crate::display::{self, RenderOptions, Row};
use crate::editor::{Editor, EditorOutcome, EditorPrompt};
use crate::walker::{self, Node, WalkOptions};

/// Display a path as a string. Unix paths are returned verbatim.
fn display_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Render the navigation breadcrumb for the current root.
fn breadcrumb(p: &Path) -> String {
    display_path(p)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum View {
    Main,
    Settings,
    Help,
    Warnings,
    Permissions,
    Editor,
}

#[derive(Clone, Default)]
pub struct FilterState {
    pub query: String,
    pub active: bool,
    /// True while the input bar is open and capturing keystrokes.
    pub editing: bool,
}

#[derive(Clone, Debug)]
pub struct SpotlightEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    #[allow(dead_code)]
    pub size_hint: Option<u64>,
}

pub struct SpotlightState {
    pub input: String,
    pub candidates: Vec<SpotlightEntry>,
    pub selected: usize,
    pub last_input_change: Instant,
    pub pending_refresh: bool,
    pub current_generation: u64,
    /// Cache of the last directory listing, keyed by the parent path.
    pub cache: Option<(PathBuf, Vec<SpotlightEntry>)>,
}

impl SpotlightState {
    fn new(initial: String) -> Self {
        SpotlightState {
            input: initial,
            candidates: Vec::new(),
            selected: 0,
            last_input_change: Instant::now(),
            pending_refresh: true,
            current_generation: 0,
            cache: None,
        }
    }
}

#[derive(Debug)]
struct SpotlightReq {
    generation: u64,
    parent: PathBuf,
    partial: String,
}

#[derive(Debug)]
struct SpotlightResp {
    generation: u64,
    parent: PathBuf,
    entries: Vec<SpotlightEntry>,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    SetMode(Mode),
    OpenHelp,
    OpenSettings,
    CloseOverlay,
    Quit,
    OpenWarnings,
    SelectRow(usize),
    DrillRow(usize),
    CycleSort,
    ToggleHidden,
    ToggleEmpty,
    TogglePercent,
    ToggleModified,
    ClearFilter,
    SettingsRow(usize),
    SettingsResetDefaults,
    SettingsClearFilter,
}

#[derive(Clone, Copy)]
struct HitRegion {
    rect: Rect,
    action: Action,
}

pub struct App {
    pub root: Node,
    #[allow(dead_code)]
    pub initial_path: PathBuf,
    pub cursor: usize,
    pub scroll: usize,
    pub config: Config,
    pub min_size: u64,
    view: View,
    settings_cursor: usize,
    settings_scroll: usize,
    nav_stack: Vec<PathBuf>,
    status_msg: Option<(String, Instant)>,
    last_size: (u16, u16),
    rescanning: bool,
    pub warnings: Vec<String>,
    warnings_scroll: usize,
    filter: FilterState,
    /// Hit regions registered by the renderer for the most-recent frame.
    hits: Vec<HitRegion>,
    /// Body rect of the most recent frame (for scrollbar / row hit math).
    body_rect: Rect,
    body_scroll: usize,
    body_visible_rows: usize,
    last_click: Option<(Instant, u16, u16)>,
    /// Per-session extra excludes added via 'x'.
    session_excludes: Vec<String>,
    pub spotlight: Option<SpotlightState>,
    spot_tx: Option<Sender<SpotlightReq>>,
    spot_rx: Option<Receiver<SpotlightResp>>,
    /// Permissions inspector state (when active).
    pub perms: Option<PermsState>,
    /// Embedded text editor (when active).
    pub editor: Option<Editor>,
    /// Set when the user requests application exit (e.g. clicks the [✕]
    /// title-bar button). Polled by the run loop.
    pub should_quit: bool,
}

#[derive(Clone)]
struct SettingItem {
    label: String,
    kind: SettingKind,
}

#[derive(Clone, Copy)]
enum SettingKind {
    ExcludeToggle(usize),
    DepthAdjust,
    ToggleModified,
    ToggleHidden,
    ToggleEmpty,
    TogglePercent,
    CycleTheme,
    ToggleShowFiles,
    ClearFilter,
    ResetDefaults,
}

/// State for the permissions inspector overlay.
pub struct PermsState {
    pub path: PathBuf,
    pub is_dir: bool,
    pub uid: u32,
    pub gid: u32,
    pub owner_name: String,
    pub group_name: String,
    /// Lower 12 bits: rwx triplets and special bits.
    pub mode: u32,
    /// Cursor across the toggle/button grid. See [`PermsState::CELLS`].
    pub cursor: usize,
    /// Confirmation overlay for "Apply Recursively".
    pub confirm_recursive: bool,
    /// Confirm-cursor: 0=Yes, 1=No.
    pub confirm_cursor: u8,
}

impl PermsState {
    /// 9 perm toggles (rwx for owner/group/other) + 3 special bits + 3 buttons = 15.
    #[allow(dead_code)]
    pub const CELLS: usize = 15;

    pub fn from_path(path: &Path) -> Result<Self, String> {
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| format!("stat {}: {}", path.display(), e))?;
        let mode = meta.permissions().mode();
        let uid = meta.uid();
        let gid = meta.gid();
        let owner_name = users::get_user_by_uid(uid)
            .map(|u| u.name().to_string_lossy().into_owned())
            .unwrap_or_else(|| uid.to_string());
        let group_name = users::get_group_by_gid(gid)
            .map(|g| g.name().to_string_lossy().into_owned())
            .unwrap_or_else(|| gid.to_string());
        Ok(PermsState {
            path: path.to_path_buf(),
            is_dir: meta.is_dir(),
            uid,
            gid,
            owner_name,
            group_name,
            mode: mode & 0o7777,
            cursor: 12, // default to [Apply]
            confirm_recursive: false,
            confirm_cursor: 1,
        })
    }

    /// Apply the current `mode` to `path` (non-recursive).
    pub fn apply(&self) -> std::io::Result<()> {
        std::fs::set_permissions(
            &self.path,
            std::fs::Permissions::from_mode(self.mode),
        )
    }

    /// Walk the tree, applying `mode` to every entry.
    pub fn apply_recursive(&self) -> Result<usize, String> {
        let mut count = 0usize;
        for entry in walkdir::WalkDir::new(&self.path) {
            let entry =
                entry.map_err(|e| format!("walk error: {}", e))?;
            let p = entry.path();
            if let Err(e) = std::fs::set_permissions(
                p,
                std::fs::Permissions::from_mode(self.mode),
            ) {
                return Err(format!("{}: {}", p.display(), e));
            }
            count += 1;
        }
        Ok(count)
    }

    /// Toggle the cell under the cursor (perms grid only).
    pub fn toggle_cursor(&mut self) {
        if self.cursor < 9 {
            // owner/group/other rwx
            let bit = 1u32 << (8 - self.cursor);
            self.mode ^= bit;
        } else if self.cursor < 12 {
            // SUID / SGID / Sticky
            let bit = match self.cursor {
                9 => 0o4000,
                10 => 0o2000,
                11 => 0o1000,
                _ => 0,
            };
            self.mode ^= bit;
        }
    }

    pub fn move_cursor(&mut self, dx: i32, dy: i32) {
        let idx = self.cursor as i32;
        // Layout: 3 rows of 3 perm toggles (0..9), 1 row of 3 special bits
        // (9..12), 1 row of 3 buttons (12..15).
        let row = idx / 3;
        let col = idx % 3;
        let new_row = (row + dy).clamp(0, 4);
        let new_col = (col + dx).clamp(0, 2);
        let new_idx = (new_row * 3 + new_col).clamp(0, 14);
        self.cursor = new_idx as usize;
    }
}

pub fn is_root() -> bool {
    users::get_current_uid() == 0
}

impl App {
    pub fn new(
        root: Node,
        initial_path: PathBuf,
        config: Config,
        min_size: u64,
        warnings: Vec<String>,
    ) -> Self {
        let nav_stack: Vec<PathBuf> = Vec::new();

        Self {
            root,
            initial_path,
            cursor: 0,
            scroll: 0,
            config,
            min_size,
            view: View::Main,
            settings_cursor: 0,
            settings_scroll: 0,
            nav_stack,
            status_msg: None,
            last_size: (80, 24),
            rescanning: false,
            warnings,
            warnings_scroll: 0,
            filter: FilterState::default(),
            hits: Vec::new(),
            body_rect: Rect::new(0, 0, 0, 0),
            body_scroll: 0,
            body_visible_rows: 0,
            last_click: None,
            session_excludes: Vec::new(),
            spotlight: None,
            spot_tx: None,
            spot_rx: None,
            perms: None,
            editor: None,
            should_quit: false,
        }
    }

    fn render_opts(&self) -> RenderOptions {
        RenderOptions {
            theme: self.config.theme,
            depth: self.config.depth,
            min_size: self.min_size,
            sort: self.config.sort,
            sort_order: self.config.sort_order,
            show_files: self.config.show_files,
            show_modified: self.config.view.show_modified || self.config.show_modified,
            width: self.last_size.0,
            height: self.last_size.1.saturating_sub(5).max(8),
            hidden_summary: display::compute_hidden_summary(
                &self.root,
                self.config.view.show_hidden,
            ),
        }
    }

    fn build_rows(&self) -> Vec<Row> {
        let opts = self.render_opts();
        let raw = match self.config.mode {
            Mode::Tree => display::tree::render(&self.root, &opts),
            Mode::Bars => display::bars::render(&self.root, &opts),
            Mode::Treemap => display::treemap::render(&self.root, &opts),
            Mode::Flat => display::flat::render(&self.root, &opts),
        };
        self.filter_rows(raw)
    }

    /// Number of selectable rows in the currently rendered view. The
    /// synthetic hidden-items summary row may appear anywhere in the list
    /// (Bars mode sorts it among real entries), and is excluded from
    /// navigation/click hit-testing.
    fn selectable_len(rows: &[Row]) -> usize {
        rows.iter().filter(|r| !r.is_hidden_summary).count()
    }

    /// Index of the last selectable (non-summary) row, or 0 if none exist.
    fn last_selectable_idx(rows: &[Row]) -> usize {
        rows.iter()
            .rposition(|r| !r.is_hidden_summary)
            .unwrap_or(0)
    }

    /// Move forward from `from` by up to `steps` selectable rows, skipping
    /// hidden-summary entries.
    fn step_forward(rows: &[Row], from: usize, steps: usize) -> usize {
        let mut i = from;
        let mut left = steps;
        while left > 0 {
            let mut next = i + 1;
            while next < rows.len() && rows[next].is_hidden_summary {
                next += 1;
            }
            if next >= rows.len() {
                break;
            }
            i = next;
            left -= 1;
        }
        i
    }

    /// Move backward from `from` by up to `steps` selectable rows, skipping
    /// hidden-summary entries.
    fn step_backward(rows: &[Row], from: usize, steps: usize) -> usize {
        let mut i = from;
        let mut left = steps;
        while left > 0 && i > 0 {
            let mut prev = i - 1;
            while prev > 0 && rows[prev].is_hidden_summary {
                prev -= 1;
            }
            if rows[prev].is_hidden_summary {
                break;
            }
            i = prev;
            left -= 1;
        }
        i
    }

    /// If the cursor currently points at a hidden-summary row, snap it to the
    /// nearest selectable neighbour (preferring forward, falling back back).
    fn snap_cursor_to_selectable(rows: &[Row], cursor: usize) -> usize {
        if rows.is_empty() {
            return 0;
        }
        let mut c = cursor.min(rows.len() - 1);
        if !rows[c].is_hidden_summary {
            return c;
        }
        let mut fwd = c + 1;
        while fwd < rows.len() && rows[fwd].is_hidden_summary {
            fwd += 1;
        }
        if fwd < rows.len() {
            return fwd;
        }
        if c == 0 {
            return 0;
        }
        let mut back = c - 1;
        loop {
            if !rows[back].is_hidden_summary {
                c = back;
                break;
            }
            if back == 0 {
                break;
            }
            back -= 1;
        }
        c
    }

    fn filter_rows(&self, rows: Vec<Row>) -> Vec<Row> {
        let q = if self.filter.active && !self.filter.query.is_empty() {
            Some(self.filter.query.to_lowercase())
        } else {
            None
        };
        let view = self.config.view;
        rows.into_iter()
            .filter(|r| {
                // Always keep header row (the root row referencing self.root.path).
                if let Some(p) = &r.path {
                    if p == &self.root.path {
                        return true;
                    }
                }
                // Hidden file/dir
                if !view.show_hidden {
                    if r.name.starts_with('.') {
                        return false;
                    }
                }
                // Empty dirs
                if !view.show_empty && r.is_dir && r.size == 0 {
                    return false;
                }
                // Filter query
                if let Some(q) = &q {
                    if !r.name.is_empty() && !r.name.to_lowercase().contains(q) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    fn settings_items(&self) -> Vec<SettingItem> {
        let mut items = Vec::new();
        items.push(SettingItem {
            label: format!("Depth: {}  (+/- to change)", self.config.depth),
            kind: SettingKind::DepthAdjust,
        });
        items.push(SettingItem {
            label: format!(
                "Show modified date: {}",
                onoff(self.config.view.show_modified || self.config.show_modified)
            ),
            kind: SettingKind::ToggleModified,
        });
        items.push(SettingItem {
            label: format!("Show hidden files: {}", onoff(self.config.view.show_hidden)),
            kind: SettingKind::ToggleHidden,
        });
        items.push(SettingItem {
            label: format!("Show empty dirs: {}", onoff(self.config.view.show_empty)),
            kind: SettingKind::ToggleEmpty,
        });
        items.push(SettingItem {
            label: format!(
                "Show percentage column: {}",
                onoff(self.config.view.show_percent)
            ),
            kind: SettingKind::TogglePercent,
        });
        items.push(SettingItem {
            label: format!("Show files: {}", onoff(self.config.show_files)),
            kind: SettingKind::ToggleShowFiles,
        });
        items.push(SettingItem {
            label: format!("Theme: {:?}  (Enter to cycle)", self.config.theme),
            kind: SettingKind::CycleTheme,
        });
        if self.filter.active && !self.filter.query.is_empty() {
            items.push(SettingItem {
                label: format!("Filter: \"{}\"   [clear]", self.filter.query),
                kind: SettingKind::ClearFilter,
            });
        }
        for (i, pat) in self.config.excludes.iter().enumerate() {
            let on = self.config.excludes_enabled.get(i).copied().unwrap_or(true);
            items.push(SettingItem {
                label: format!("Exclude  [{}]  {}", if on { "x" } else { " " }, pat),
                kind: SettingKind::ExcludeToggle(i),
            });
        }
        items.push(SettingItem {
            label: "[Reset to defaults]".to_string(),
            kind: SettingKind::ResetDefaults,
        });
        items
    }

    fn cycle_theme(&mut self) {
        self.config.theme = match self.config.theme {
            Theme::Default => Theme::HighContrast,
            Theme::HighContrast => Theme::Monochrome,
            Theme::Monochrome => Theme::Default,
        };
    }

    fn rescan_at(&mut self, path: PathBuf) {
        // Guard against vanished / inaccessible paths so we never panic on a
        // stale nav stack entry or a directory that was removed mid-session.
        if !path.exists() {
            self.flash(format!("path no longer exists: {}", path.display()));
            return;
        }
        if !path.is_dir() {
            self.flash(format!("not a directory: {}", path.display()));
            return;
        }
        self.rescanning = true;
        let mut excludes = self.config.active_excludes();
        excludes.extend(self.session_excludes.iter().cloned());
        let opts = WalkOptions {
            excludes: &excludes,
            on_progress: None,
        };
        match walker::build_tree(&path, &opts) {
            Ok(scan) => {
                self.root = scan.root;
                self.warnings = scan.warnings;
                self.warnings_scroll = 0;
                self.cursor = 0;
                self.scroll = 0;
            }
            Err(e) => {
                self.status_msg = Some((format!("scan error: {}", e), Instant::now()));
            }
        }
        self.rescanning = false;
    }

    fn drill_into(&mut self, path: PathBuf) {
        self.nav_stack.push(self.root.path.clone());
        self.rescan_at(path);
    }

    fn go_up(&mut self) {
        // Decide where "up" goes without mutating the nav stack yet so a
        // failure leaves us in a consistent state.
        let (target, pop) = if let Some(prev) = self.nav_stack.last().cloned() {
            (prev, true)
        } else {
            match self.root.path.parent().map(|p| p.to_path_buf()) {
                Some(parent) => (parent, false),
                None => {
                    self.flash("already at filesystem root");
                    return;
                }
            }
        };
        if !target.is_dir() {
            // Stale nav stack entry or a parent we cannot read — drop the bad
            // entry (if any) but do not navigate.
            if pop {
                self.nav_stack.pop();
            }
            self.flash(format!("cannot go up: {} is not accessible", target.display()));
            return;
        }
        if pop {
            self.nav_stack.pop();
        }
        self.rescan_at(target);
    }

    fn cycle_sort(&mut self) {
        let next = match (self.config.sort, self.config.sort_order) {
            (SortBy::Size, SortOrder::Desc) => (SortBy::Size, SortOrder::Asc),
            (SortBy::Size, SortOrder::Asc) => (SortBy::Name, SortOrder::Asc),
            (SortBy::Name, SortOrder::Asc) => (SortBy::Name, SortOrder::Desc),
            (SortBy::Name, SortOrder::Desc) => (SortBy::Size, SortOrder::Desc),
        };
        self.config.sort = next.0;
        self.config.sort_order = next.1;
    }

    fn sort_label(&self) -> String {
        let by = match self.config.sort {
            SortBy::Size => "size",
            SortBy::Name => "name",
        };
        format!("{} {}", by, self.config.sort_order.arrow())
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status_msg = Some((msg.into(), Instant::now()));
    }

    fn selected_row_path(&self) -> Option<PathBuf> {
        let rows = self.build_rows();
        let r = rows.get(self.cursor)?;
        if r.is_hidden_summary {
            return None;
        }
        r.path.clone()
    }

    fn yank_selected(&mut self) {
        let Some(p) = self.selected_row_path() else {
            self.flash("nothing selected");
            return;
        };
        let s = p.display().to_string();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(s.clone())) {
            Ok(()) => self.flash("yanked path to clipboard"),
            Err(e) => self.flash(format!("clipboard error: {}", e)),
        }
    }

    fn open_selected(&mut self) {
        let Some(p) = self.selected_row_path() else {
            self.flash("nothing selected");
            return;
        };
        #[cfg(target_os = "linux")]
        let opener: &str = "xdg-open";
        #[cfg(target_os = "macos")]
        let opener: &str = "open";
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let opener: &str = "xdg-open";

        match Command::new(opener)
            .arg(&p)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => self.flash(format!("opened with {}", opener)),
            Err(_) => self.flash(format!("failed to launch {}", opener)),
        }
    }

    fn exclude_selected(&mut self) {
        let Some(p) = self.selected_row_path() else {
            self.flash("nothing selected");
            return;
        };
        if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
            let pat = name.to_string();
            if !self.session_excludes.iter().any(|e| e == &pat) {
                self.session_excludes.push(pat.clone());
            }
            self.flash(format!("excluded \"{}\" (session)", pat));
            let cur = self.root.path.clone();
            self.rescan_at(cur);
        }
    }

    fn jump_home(&mut self) {
        if let Some(home) = dirs::home_dir() {
            self.nav_stack.push(self.root.path.clone());
            self.rescan_at(home);
        }
    }

    fn open_perms_inspector(&mut self) {
        let Some(p) = self.selected_row_path() else {
            self.flash("nothing selected");
            return;
        };
        match PermsState::from_path(&p) {
            Ok(state) => {
                self.perms = Some(state);
                self.view = View::Permissions;
            }
            Err(e) => self.flash(format!("perms: {}", e)),
        }
    }

    fn open_editor_for_selected(&mut self) {
        let Some(p) = self.selected_row_path() else {
            self.flash("nothing selected");
            return;
        };
        if p.is_dir() {
            self.flash("editor: cannot open a directory");
            return;
        }
        match Editor::open(p) {
            Ok(ed) => {
                self.editor = Some(ed);
                self.view = View::Editor;
            }
            Err(e) => self.flash(format!("editor: {}", e)),
        }
    }

    fn save_settings(&mut self) {
        match self.config.save() {
            Ok(()) => self.flash("Settings saved to ~/.config/diskvis/config.toml"),
            Err(e) => self.flash(format!("Failed to save settings: {}", e)),
        }
    }

    // -- Spotlight ---------------------------------------------------------

    fn ensure_spotlight_worker(&mut self) {
        if self.spot_tx.is_some() {
            return;
        }
        let (req_tx, req_rx) = mpsc::channel::<SpotlightReq>();
        let (resp_tx, resp_rx) = mpsc::channel::<SpotlightResp>();
        thread::spawn(move || {
            // Coalesce: if multiple requests are queued, only honour the latest.
            while let Ok(mut req) = req_rx.recv() {
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let entries = list_dir_entries(&req.parent, &req.partial);
                let _ = resp_tx.send(SpotlightResp {
                    generation: req.generation,
                    parent: req.parent,
                    entries,
                });
            }
        });
        self.spot_tx = Some(req_tx);
        self.spot_rx = Some(resp_rx);
    }

    fn open_spotlight(&mut self) {
        self.ensure_spotlight_worker();
        let init = {
            let s = format!("{}/", self.root.path.display());
            s.replace("//", "/")
        };
        let mut s = SpotlightState::new(init);
        s.pending_refresh = true;
        s.last_input_change = Instant::now();
        self.spotlight = Some(s);
        // Trigger immediate refresh.
        self.spotlight_refresh(true);
    }

    fn close_spotlight(&mut self) {
        self.spotlight = None;
    }

    fn spotlight_input_changed(&mut self) {
        if let Some(s) = self.spotlight.as_mut() {
            s.pending_refresh = true;
            s.last_input_change = Instant::now();
            s.selected = 0;
        }
    }

    fn spotlight_refresh(&mut self, force: bool) {
        let Some(s) = self.spotlight.as_mut() else {
            return;
        };
        if !s.pending_refresh && !force {
            return;
        }
        // Debounce 50 ms unless forced.
        if !force && s.last_input_change.elapsed() < Duration::from_millis(50) {
            return;
        }
        s.pending_refresh = false;
        s.current_generation = s.current_generation.wrapping_add(1);
        let gen = s.current_generation;
        let (parent, partial) = split_input(&s.input);

        // Cache hit?
        if let Some((p, ents)) = &s.cache {
            if p == &parent {
                s.candidates = filter_entries(ents, &partial);
                if s.selected >= s.candidates.len() {
                    s.selected = 0;
                }
                // Still send a request to refresh, but the cached view shows now.
            }
        }

        if let Some(tx) = self.spot_tx.as_ref() {
            let _ = tx.send(SpotlightReq {
                generation: gen,
                parent,
                partial,
            });
        }
    }

    fn drain_spotlight_responses(&mut self) {
        let Some(rx) = self.spot_rx.as_ref() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(resp) => {
                    let Some(s) = self.spotlight.as_mut() else {
                        continue;
                    };
                    if resp.generation != s.current_generation {
                        continue;
                    }
                    let (_parent, partial) = split_input(&s.input);
                    s.cache = Some((resp.parent.clone(), resp.entries.clone()));
                    s.candidates = filter_entries(&resp.entries, &partial);
                    if s.selected >= s.candidates.len() {
                        s.selected = 0;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }
}

fn split_input(input: &str) -> (PathBuf, String) {
    let expanded = expand_tilde(input);
    if expanded.is_empty() {
        return (PathBuf::from("/"), String::new());
    }
    if expanded.ends_with('/') {
        let trimmed = expanded.trim_end_matches('/');
        let parent = if trimmed.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(trimmed)
        };
        return (parent, String::new());
    }
    let p = Path::new(&expanded);
    let parent = p
        .parent()
        .map(|x| {
            if x.as_os_str().is_empty() {
                PathBuf::from("/")
            } else {
                x.to_path_buf()
            }
        })
        .unwrap_or_else(|| PathBuf::from("/"));
    let partial = p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    (parent, partial)
}

fn expand_tilde(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    if input == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    input.to_string()
}

fn list_dir_entries(parent: &Path, _partial: &str) -> Vec<SpotlightEntry> {
    let mut out: Vec<SpotlightEntry> = Vec::new();
    let dir = if parent.as_os_str().is_empty() {
        Path::new("/")
    } else {
        parent
    };
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(SpotlightEntry {
            path,
            name,
            is_dir,
            size_hint: None,
        });
    }
    // Dirs first, then files; both alpha.
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    out
}

fn filter_entries(entries: &[SpotlightEntry], partial: &str) -> Vec<SpotlightEntry> {
    let needle = partial.to_lowercase();
    if needle.is_empty() {
        return entries.iter().take(64).cloned().collect();
    }
    entries
        .iter()
        .filter(|e| e.name.to_lowercase().starts_with(&needle))
        .take(64)
        .cloned()
        .collect()
}

fn longest_common_prefix(strs: &[String]) -> String {
    let Some(first) = strs.first() else {
        return String::new();
    };
    let mut end = first.chars().count();
    for s in &strs[1..] {
        let mut a = first.chars();
        let mut b = s.chars();
        let mut i = 0;
        while i < end {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x.eq_ignore_ascii_case(&y) => {
                    i += 1;
                }
                _ => break,
            }
        }
        end = i;
        if end == 0 {
            break;
        }
    }
    first.chars().take(end).collect()
}

fn onoff(b: bool) -> &'static str {
    if b {
        "ON"
    } else {
        "OFF"
    }
}

pub fn run(mut app: App) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_loop(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    let _ = app.config.save();
    res
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        // Drain any worker responses + run debounced refresh before drawing.
        app.drain_spotlight_responses();
        if app.spotlight.is_some() {
            app.spotlight_refresh(false);
        }

        terminal.draw(|f| {
            let size = f.area();
            app.last_size = (size.width, size.height);
            draw(f, app, size);
        })?;

        let poll_ms = if app.spotlight.is_some() { 30 } else { 200 };
        if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                Event::Key(key) => {
                    if handle_key_event(app, key) {
                        return Ok(());
                    }
                }
                Event::Mouse(m) => {
                    handle_mouse_event(app, m);
                    if app.should_quit {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

// ============================================================================
// Event handling
// ============================================================================

pub fn handle_key_event(app: &mut App, key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }

    // Editor swallows all keys when open.
    if app.editor.is_some() {
        let outcome = {
            let editor = app.editor.as_mut().unwrap();
            editor.report_size(app.last_size.0, app.last_size.1);
            editor.handle_key(key)
        };
        if outcome == EditorOutcome::Close {
            app.editor = None;
        }
        return false;
    }

    // Permissions inspector swallows keys when open.
    if app.perms.is_some() {
        handle_perms_key(app, key);
        return false;
    }

    // Spotlight overlay swallows all keys when open.
    if app.spotlight.is_some() {
        handle_spotlight_key(app, key);
        return false;
    }

    // Filter-edit mode swallows almost everything.
    if app.filter.editing && app.view == View::Main {
        match key.code {
            KeyCode::Esc => {
                app.filter.query.clear();
                app.filter.active = false;
                app.filter.editing = false;
            }
            KeyCode::Enter => {
                app.filter.editing = false;
                if app.filter.query.is_empty() {
                    app.filter.active = false;
                }
            }
            KeyCode::Backspace => {
                app.filter.query.pop();
            }
            KeyCode::Char(c) => {
                app.filter.query.push(c);
            }
            _ => {}
        }
        return false;
    }

    match app.view {
        View::Help => {
            // Any key closes help.
            app.view = View::Main;
            return false;
        }
        View::Permissions | View::Editor => {
            // Handled above via app.perms / app.editor branches; the View enum
            // is purely a draw-state marker for these overlays.
            return false;
        }
        View::Warnings => {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('w') => {
                    app.view = View::Main;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if app.warnings_scroll + 1 < app.warnings.len() {
                        app.warnings_scroll += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.warnings_scroll > 0 {
                        app.warnings_scroll -= 1;
                    }
                }
                KeyCode::PageDown => {
                    app.warnings_scroll = (app.warnings_scroll + 10)
                        .min(app.warnings.len().saturating_sub(1));
                }
                KeyCode::PageUp => {
                    app.warnings_scroll = app.warnings_scroll.saturating_sub(10);
                }
                _ => {}
            }
            return false;
        }
        View::Settings => {
            handle_settings_key(app, key);
            return false;
        }
        View::Main => {}
    }

    // Main view keybinds.
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Char('?') => app.view = View::Help,
        KeyCode::Char('w') => {
            app.warnings_scroll = 0;
            app.view = View::Warnings;
        }
        KeyCode::Char('e') => app.open_editor_for_selected(),
        KeyCode::Char('S') => {
            app.settings_cursor = 0;
            app.settings_scroll = 0;
            app.view = View::Settings;
        }
        KeyCode::Char('i') => app.open_perms_inspector(),
        KeyCode::Char(' ') => app.open_spotlight(),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_spotlight();
        }
        KeyCode::Char('1') => app.config.mode = Mode::Tree,
        KeyCode::Char('2') => app.config.mode = Mode::Bars,
        KeyCode::Char('3') => app.config.mode = Mode::Treemap,
        KeyCode::Char('4') => app.config.mode = Mode::Flat,
        KeyCode::Char('f') => app.config.show_files = !app.config.show_files,
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('.') | KeyCode::Char('h') => {
            app.config.view.show_hidden = !app.config.view.show_hidden;
            app.flash(format!(
                "hidden files: {}",
                onoff(app.config.view.show_hidden)
            ));
        }
        KeyCode::Char('d') => {
            app.config.view.show_empty = !app.config.view.show_empty;
            app.flash(format!("empty dirs: {}", onoff(app.config.view.show_empty)));
        }
        KeyCode::Char('p') => {
            app.config.view.show_percent = !app.config.view.show_percent;
            app.flash(format!(
                "percent column: {}",
                onoff(app.config.view.show_percent)
            ));
        }
        KeyCode::Char('m') => {
            app.config.view.show_modified = !app.config.view.show_modified;
            app.config.show_modified = app.config.view.show_modified;
            app.flash(format!(
                "modified column: {}",
                onoff(app.config.view.show_modified)
            ));
        }
        KeyCode::Char('c') => {
            app.cycle_theme();
            app.flash(format!("theme: {:?}", app.config.theme));
        }
        KeyCode::Char('~') => app.jump_home(),
        KeyCode::Char('r') => {
            let cur = app.root.path.clone();
            app.rescan_at(cur);
            app.flash("rescanned");
        }
        KeyCode::Char('g') => {
            let rows = app.build_rows();
            app.cursor = App::snap_cursor_to_selectable(&rows, 0);
        }
        KeyCode::Char('G') => {
            let rows = app.build_rows();
            app.cursor = App::last_selectable_idx(&rows);
        }
        KeyCode::Char('u') => app.go_up(),
        KeyCode::Char('x') => app.exclude_selected(),
        KeyCode::Char('o') => app.open_selected(),
        KeyCode::Char('y') => app.yank_selected(),
        KeyCode::Char('/') => {
            app.filter.editing = true;
            app.filter.active = true;
        }
        KeyCode::Esc => {
            if app.filter.active {
                app.filter.query.clear();
                app.filter.active = false;
                app.filter.editing = false;
                app.flash("filter cleared");
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            let rows = app.build_rows();
            app.cursor = App::step_forward(&rows, app.cursor, 1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let rows = app.build_rows();
            app.cursor = App::step_backward(&rows, app.cursor, 1);
        }
        KeyCode::Home => {
            let rows = app.build_rows();
            app.cursor = App::snap_cursor_to_selectable(&rows, 0);
        }
        KeyCode::End => {
            let rows = app.build_rows();
            app.cursor = App::last_selectable_idx(&rows);
        }
        KeyCode::PageDown => {
            let rows = app.build_rows();
            let half = (app.body_visible_rows / 2).max(1);
            app.cursor = App::step_forward(&rows, app.cursor, half);
        }
        KeyCode::PageUp => {
            let rows = app.build_rows();
            let half = (app.body_visible_rows / 2).max(1);
            app.cursor = App::step_backward(&rows, app.cursor, half);
        }
        KeyCode::Enter => {
            let rows = app.build_rows();
            if let Some(row) = rows.get(app.cursor) {
                if row.is_dir {
                    if let Some(p) = row.path.clone() {
                        if p != app.root.path {
                            app.drill_into(p);
                        }
                    }
                }
            }
        }
        KeyCode::Backspace => app.go_up(),
        _ => {}
    }
    false
}

fn handle_spotlight_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            app.close_spotlight();
            return;
        }
        KeyCode::Enter => {
            // Navigate to the selected candidate, or to the typed path if none.
            let target: Option<PathBuf> = {
                let s = app.spotlight.as_ref().unwrap();
                if let Some(c) = s.candidates.get(s.selected) {
                    Some(c.path.clone())
                } else {
                    let expanded = expand_tilde(&s.input);
                    let p = PathBuf::from(expanded);
                    if p.exists() && p.is_dir() {
                        Some(p)
                    } else {
                        None
                    }
                }
            };
            if let Some(p) = target {
                app.close_spotlight();
                app.nav_stack.push(app.root.path.clone());
                app.rescan_at(p);
            } else {
                app.flash("spotlight: no valid path");
            }
            return;
        }
        KeyCode::Up => {
            if let Some(s) = app.spotlight.as_mut() {
                if s.selected > 0 {
                    s.selected -= 1;
                }
            }
            return;
        }
        KeyCode::Down => {
            if let Some(s) = app.spotlight.as_mut() {
                if s.selected + 1 < s.candidates.len() {
                    s.selected += 1;
                }
            }
            return;
        }
        KeyCode::Tab | KeyCode::Right => {
            spotlight_complete(app);
            return;
        }
        KeyCode::Backspace => {
            if let Some(s) = app.spotlight.as_mut() {
                s.input.pop();
            }
            app.spotlight_input_changed();
            app.spotlight_refresh(true);
            return;
        }
        KeyCode::Char('w') if ctrl => {
            if let Some(s) = app.spotlight.as_mut() {
                let mut text = s.input.clone();
                if text.ends_with('/') {
                    text.pop();
                }
                while let Some(c) = text.pop() {
                    if c == '/' {
                        text.push('/');
                        break;
                    }
                }
                if text.is_empty() {
                    text.push('/');
                }
                s.input = text;
            }
            app.spotlight_input_changed();
            app.spotlight_refresh(true);
            return;
        }
        KeyCode::Char('u') if ctrl => {
            if let Some(s) = app.spotlight.as_mut() {
                s.input.clear();
            }
            app.spotlight_input_changed();
            app.spotlight_refresh(true);
            return;
        }
        KeyCode::Char(c) => {
            if let Some(s) = app.spotlight.as_mut() {
                s.input.push(c);
            }
            app.spotlight_input_changed();
            // Typing '/' on a valid dir → expand into it (force immediate refresh).
            let force = c == '/';
            app.spotlight_refresh(force);
            return;
        }
        _ => {}
    }
}

fn spotlight_complete(app: &mut App) {
    let Some(s) = app.spotlight.as_mut() else {
        return;
    };
    if s.candidates.is_empty() {
        return;
    }
    let (parent, _) = split_input(&s.input);
    if s.candidates.len() == 1 {
        // Accept the single match.
        let c = &s.candidates[0];
        let mut new_input = parent.to_string_lossy().to_string();
        if !new_input.ends_with('/') {
            new_input.push('/');
        }
        new_input.push_str(&c.name);
        if c.is_dir {
            new_input.push('/');
        }
        s.input = new_input;
        s.selected = 0;
        app.spotlight_input_changed();
        app.spotlight_refresh(true);
        return;
    }
    // Multiple matches → extend to longest common prefix of names.
    let names: Vec<String> = s.candidates.iter().map(|c| c.name.clone()).collect();
    let lcp = longest_common_prefix(&names);
    let mut new_input = parent.to_string_lossy().to_string();
    if !new_input.ends_with('/') {
        new_input.push('/');
    }
    new_input.push_str(&lcp);
    if !lcp.is_empty() {
        s.input = new_input;
        app.spotlight_input_changed();
        app.spotlight_refresh(true);
    } else {
        // No common prefix — accept currently-selected entry instead.
        let c = &s.candidates[s.selected];
        let mut new_input = parent.to_string_lossy().to_string();
        if !new_input.ends_with('/') {
            new_input.push('/');
        }
        new_input.push_str(&c.name);
        if c.is_dir {
            new_input.push('/');
        }
        s.input = new_input;
        s.selected = 0;
        app.spotlight_input_changed();
        app.spotlight_refresh(true);
    }
}

fn handle_settings_key(app: &mut App, key: KeyEvent) {    match key.code {
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('S') => {
            app.save_settings();
            app.view = View::Main;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.settings_cursor > 0 {
                app.settings_cursor -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let n = app.settings_items().len();
            if app.settings_cursor + 1 < n {
                app.settings_cursor += 1;
            }
        }
        KeyCode::PageDown => {
            let n = app.settings_items().len();
            app.settings_cursor = (app.settings_cursor + 10).min(n.saturating_sub(1));
        }
        KeyCode::PageUp => {
            app.settings_cursor = app.settings_cursor.saturating_sub(10);
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            let items = app.settings_items();
            if let Some(SettingKind::DepthAdjust) =
                items.get(app.settings_cursor).map(|i| i.kind)
            {
                app.config.depth += 1;
                let cur = app.root.path.clone();
                app.rescan_at(cur);
            }
        }
        KeyCode::Char('-') => {
            let items = app.settings_items();
            if let Some(SettingKind::DepthAdjust) =
                items.get(app.settings_cursor).map(|i| i.kind)
            {
                if app.config.depth > 0 {
                    app.config.depth -= 1;
                }
            }
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let items = app.settings_items();
            if let Some(item) = items.get(app.settings_cursor).cloned() {
                activate_setting(app, item.kind);
            }
        }
        _ => {}
    }
}

fn activate_setting(app: &mut App, kind: SettingKind) {
    match kind {
        SettingKind::ExcludeToggle(i) => {
            if let Some(en) = app.config.excludes_enabled.get_mut(i) {
                *en = !*en;
            }
            let cur = app.root.path.clone();
            app.rescan_at(cur);
        }
        SettingKind::ToggleModified => {
            app.config.view.show_modified = !app.config.view.show_modified;
            app.config.show_modified = app.config.view.show_modified;
        }
        SettingKind::ToggleHidden => {
            app.config.view.show_hidden = !app.config.view.show_hidden;
        }
        SettingKind::ToggleEmpty => {
            app.config.view.show_empty = !app.config.view.show_empty;
        }
        SettingKind::TogglePercent => {
            app.config.view.show_percent = !app.config.view.show_percent;
        }
        SettingKind::ToggleShowFiles => {
            app.config.show_files = !app.config.show_files;
        }
        SettingKind::CycleTheme => app.cycle_theme(),
        SettingKind::ClearFilter => {
            app.filter.query.clear();
            app.filter.active = false;
            app.filter.editing = false;
        }
        SettingKind::ResetDefaults => {
            let saved_excludes = app.config.excludes.clone();
            let saved_enabled = app.config.excludes_enabled.clone();
            app.config = Config::default();
            app.config.excludes = saved_excludes;
            app.config.excludes_enabled = saved_enabled;
            app.flash("Settings reset to defaults");
        }
        SettingKind::DepthAdjust => {}
    }
}

pub fn handle_mouse_event(app: &mut App, m: MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollDown => {
            match app.view {
                View::Main => {
                    let rows = app.build_rows();
                    app.cursor = App::step_forward(&rows, app.cursor, 3);
                }
                View::Settings => {
                    let n = app.settings_items().len();
                    app.settings_cursor = (app.settings_cursor + 3).min(n.saturating_sub(1));
                }
                View::Warnings => {
                    app.warnings_scroll = (app.warnings_scroll + 3)
                        .min(app.warnings.len().saturating_sub(1));
                }
                View::Help | View::Permissions | View::Editor => {}
            }
            return;
        }
        MouseEventKind::ScrollUp => {
            match app.view {
                View::Main => {
                    let rows = app.build_rows();
                    app.cursor = App::step_backward(&rows, app.cursor, 3);
                }
                View::Settings => {
                    app.settings_cursor = app.settings_cursor.saturating_sub(3);
                }
                View::Warnings => {
                    app.warnings_scroll = app.warnings_scroll.saturating_sub(3);
                }
                View::Help | View::Permissions | View::Editor => {}
            }
            return;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return,
    }

    let (col, row) = (m.column, m.row);

    // Hit-testing against registered regions (last frame).
    let action = app
        .hits
        .iter()
        .find(|h| point_in(h.rect, col, row))
        .map(|h| h.action);

    // Detect double-click on row/area.
    let now = Instant::now();
    let is_double = match app.last_click {
        Some((t, c, r)) => {
            now.duration_since(t) < Duration::from_millis(450) && (c, r) == (col, row)
        }
        None => false,
    };
    app.last_click = Some((now, col, row));

    if let Some(act) = action {
        apply_action(app, act, is_double);
        return;
    }

    // No registered hit: maybe click landed in body area on a row.
    if app.view == View::Main && point_in(app.body_rect, col, row) {
        let local_y = row.saturating_sub(app.body_rect.y) as usize;
        let row_idx = app.body_scroll + local_y;
        let rows = app.build_rows();
        let n = App::selectable_len(&rows);
        if row_idx < rows.len()
            && !rows[row_idx].is_hidden_summary
            && n > 0
        {
            if is_double {
                apply_action(app, Action::DrillRow(row_idx), true);
            } else {
                apply_action(app, Action::SelectRow(row_idx), false);
            }
        }
    }
}

fn apply_action(app: &mut App, action: Action, is_double: bool) {
    match action {
        Action::SetMode(m) => app.config.mode = m,
        Action::OpenHelp => app.view = View::Help,
        Action::OpenSettings => {
            app.settings_cursor = 0;
            app.settings_scroll = 0;
            app.view = View::Settings;
        }
        Action::CloseOverlay => {
            if app.view == View::Settings {
                app.save_settings();
            }
            app.view = View::Main;
        }
        Action::Quit => {
            if app.view == View::Settings {
                app.save_settings();
            }
            app.should_quit = true;
        }
        Action::OpenWarnings => {
            app.warnings_scroll = 0;
            app.view = View::Warnings;
        }
        Action::SelectRow(i) => {
            app.cursor = i;
            if is_double {
                let rows = app.build_rows();
                if let Some(row) = rows.get(i) {
                    if row.is_dir {
                        if let Some(p) = row.path.clone() {
                            if p != app.root.path {
                                app.drill_into(p);
                            }
                        }
                    }
                }
            }
        }
        Action::DrillRow(i) => {
            app.cursor = i;
            let rows = app.build_rows();
            if let Some(row) = rows.get(i) {
                if row.is_dir {
                    if let Some(p) = row.path.clone() {
                        if p != app.root.path {
                            app.drill_into(p);
                        }
                    }
                }
            }
        }
        Action::CycleSort => app.cycle_sort(),
        Action::ToggleHidden => {
            app.config.view.show_hidden = !app.config.view.show_hidden;
        }
        Action::ToggleEmpty => {
            app.config.view.show_empty = !app.config.view.show_empty;
        }
        Action::TogglePercent => {
            app.config.view.show_percent = !app.config.view.show_percent;
        }
        Action::ToggleModified => {
            app.config.view.show_modified = !app.config.view.show_modified;
            app.config.show_modified = app.config.view.show_modified;
        }
        Action::ClearFilter => {
            app.filter.query.clear();
            app.filter.active = false;
            app.filter.editing = false;
        }
        Action::SettingsRow(i) => {
            app.settings_cursor = i;
            let items = app.settings_items();
            if let Some(item) = items.get(i).cloned() {
                activate_setting(app, item.kind);
            }
        }
        Action::SettingsResetDefaults => {
            activate_setting(app, SettingKind::ResetDefaults);
        }
        Action::SettingsClearFilter => {
            activate_setting(app, SettingKind::ClearFilter);
        }
    }
}

fn point_in(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x.saturating_add(rect.width) && y >= rect.y && y < rect.y.saturating_add(rect.height)
}

// ============================================================================
// Drawing
// ============================================================================

fn draw(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    app.hits.clear();

    // Layout: title (1) | body (min) | sel-path (1) | filter-bar (0/1) | status (2)
    let filter_h: u16 = if app.filter.active { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(filter_h),
            Constraint::Length(2),
        ])
        .split(area);

    let title_area = chunks[0];
    let body_area = chunks[1];
    let selpath_area = chunks[2];
    let filter_area = chunks[3];
    let status_area = chunks[4];

    draw_title(f, title_area, app);

    app.body_rect = body_area;
    draw_body(f, body_area, app);

    draw_selected_path(f, selpath_area, app);

    if filter_h > 0 {
        draw_filter_bar(f, filter_area, app);
    }

    draw_status(f, status_area, app);

    // Overlays
    match app.view {
        View::Help => draw_help(f, area, app),
        View::Settings => draw_settings(f, area, app),
        View::Warnings => draw_warnings(f, area, app),
        View::Permissions => draw_permissions(f, area, app),
        View::Editor => draw_editor(f, area, app),
        View::Main => {}
    }

    // Spotlight is independent of View — drawn last so it overlays everything.
    if app.spotlight.is_some() {
        draw_spotlight(f, area, app);
    }
}

fn draw_title(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    // Build mode pills.
    let modes = [
        (Mode::Tree, "Tree"),
        (Mode::Bars, "Bars"),
        (Mode::Treemap, "Treemap"),
        (Mode::Flat, "Flat"),
    ];

    let mut x = area.x;
    let y = area.y;

    // Brand
    let brand = " diskvis ";
    let brand_span = Span::styled(
        brand,
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(Paragraph::new(Line::from(brand_span)), Rect::new(x, y, brand.chars().count() as u16, 1));
    x = x.saturating_add(brand.chars().count() as u16 + 1);

    // Mode pills
    for (mode, label) in modes.iter() {
        let active = app.config.mode == *mode;
        let pill_text = format!(" {} ", label);
        let pill_w = pill_text.chars().count() as u16;
        let style = if active {
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let rect = Rect::new(x, y, pill_w.min(area.width.saturating_sub(x - area.x)), 1);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(pill_text.clone(), style))),
            rect,
        );
        app.hits.push(HitRegion {
            rect,
            action: Action::SetMode(*mode),
        });
        x = x.saturating_add(pill_w + 1);
    }

    // Right side: stylised [?] [⚙] [✕] action buttons.
    let right_btns: [(&str, Action); 3] = [
        ("[?]", Action::OpenHelp),
        ("[⚙]", Action::OpenSettings),
        ("[✕]", Action::Quit),
    ];
    // One space between each button => total = sum(widths) + (n-1).
    let right_total: u16 = right_btns
        .iter()
        .map(|(s, _)| s.chars().count() as u16)
        .sum::<u16>()
        + (right_btns.len() as u16).saturating_sub(1);
    if area.width > right_total + 2 {
        let mut rx = area.x + area.width.saturating_sub(right_total);
        for (i, (label, act)) in right_btns.iter().enumerate() {
            let w = label.chars().count() as u16;
            let rect = Rect::new(rx, y, w, 1);
            let base = match act {
                Action::Quit => Color::Red,
                Action::OpenSettings => Color::Cyan,
                _ => Color::Yellow,
            };
            let style = Style::default().fg(base).add_modifier(Modifier::BOLD);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(*label, style))),
                rect,
            );
            app.hits.push(HitRegion { rect, action: *act });
            rx = rx.saturating_add(w);
            // One-space gutter between buttons (but not after the last).
            if i + 1 < right_btns.len() {
                rx = rx.saturating_add(1);
            }
        }
    }

    // Path in middle (truncated). Render as a breadcrumb so the user always
    // sees where they are; on Windows this includes the synthetic "This PC"
    // root prefix.
    let avail_start = x + 1;
    let avail_end = area.x + area.width.saturating_sub(right_total + 2);
    if avail_end > avail_start {
        let avail = (avail_end - avail_start) as usize;
        let mut path_str = breadcrumb(&app.root.path);
        if path_str.chars().count() > avail {
            let take_tail = avail.saturating_sub(1);
            let tail: String = path_str
                .chars()
                .rev()
                .take(take_tail)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            path_str = format!("…{}", tail);
        }
        let rect = Rect::new(avail_start, y, avail as u16, 1);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                path_str,
                Style::default().add_modifier(Modifier::BOLD),
            ))),
            rect,
        );
    }
}

fn draw_body(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    // Treemap mode draws directly into the frame buffer.
    if app.config.mode == Mode::Treemap {
        app.body_visible_rows = area.height as usize;
        app.body_scroll = 0;
        let opts = app.render_opts();
        display::treemap::render_treemap(&app.root, &opts, area, f.buffer_mut());
        return;
    }

    let rows = app.build_rows();
    let total = rows.len();

    let view_h = area.height as usize;
    app.body_visible_rows = view_h;
    if total == 0 {
        return;
    }

    if app.cursor >= total {
        app.cursor = total - 1;
    }
    // Never let the cursor rest on the synthetic hidden-summary row, which
    // can appear anywhere in the list (Bars sorts it among real entries).
    app.cursor = App::snap_cursor_to_selectable(&rows, app.cursor);
    if app.cursor < app.scroll {
        app.scroll = app.cursor;
    } else if view_h > 0 && app.cursor >= app.scroll + view_h {
        app.scroll = app.cursor + 1 - view_h;
    }
    app.body_scroll = app.scroll;

    let show_percent = app.config.view.show_percent;
    let show_modified = app.config.view.show_modified;
    let root_size = app.root.size.max(1);

    // Reserve right column for scrollbar.
    let scrollbar_w: u16 = if total > view_h { 1 } else { 0 };
    let content_w = area.width.saturating_sub(scrollbar_w);

    let mut lines: Vec<Line> = Vec::with_capacity(view_h);
    for (idx, r) in rows.iter().enumerate().skip(app.scroll).take(view_h) {
        let mut spans: Vec<Span> = Vec::new();

        // Selection arrow / spacing
        if idx == app.cursor {
            spans.push(Span::styled(
                "▶ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw("  "));
        }

        // Dim hidden / zero entries
        let dim = (r.size == 0 && !r.name.is_empty()) || r.name.starts_with('.');
        if dim {
            for s in r.line.spans.iter() {
                spans.push(Span::styled(
                    s.content.clone().into_owned(),
                    s.style.add_modifier(Modifier::DIM),
                ));
            }
        } else {
            spans.extend(r.line.spans.iter().cloned());
        }

        // Optional right-aligned columns for % and modified
        if show_percent && !r.name.is_empty() {
            let pct = (r.size as f64 / root_size as f64) * 100.0;
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{:>5.1}%", pct),
                Style::default().fg(Color::Magenta),
            ));
        }
        if show_modified {
            // (best-effort: we don't have node here, but the renderers already
            // include modified date when show_modified is set in opts; this is
            // intentional fallback styling only)
        }

        let style = if idx == app.cursor {
            Style::default().bg(Color::Rgb(40, 40, 60))
        } else {
            Style::default()
        };

        // Pad/clip to content_w so background fills full row when selected.
        let line = Line::from(spans).style(style);
        lines.push(line);
    }

    let body = Paragraph::new(lines).wrap(Wrap { trim: false });
    let body_rect = Rect::new(area.x, area.y, content_w, area.height);
    f.render_widget(body, body_rect);

    // Scrollbar
    if scrollbar_w > 0 {
        let bar_rect = Rect::new(area.x + content_w, area.y, 1, area.height);
        draw_scrollbar(f, bar_rect, total, view_h, app.scroll);
    }
}

fn draw_scrollbar(f: &mut ratatui::Frame, area: Rect, total: usize, view: usize, scroll: usize) {
    if area.height == 0 || total == 0 {
        return;
    }
    let h = area.height as usize;
    let thumb_h = ((view as f64 / total as f64) * h as f64).max(1.0).round() as usize;
    let thumb_h = thumb_h.min(h);
    let max_scroll = total.saturating_sub(view).max(1);
    let thumb_top = ((scroll as f64 / max_scroll as f64) * (h - thumb_h) as f64).round() as usize;
    let thumb_top = thumb_top.min(h - thumb_h);

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for i in 0..h {
        if i >= thumb_top && i < thumb_top + thumb_h {
            lines.push(Line::from(Span::styled(
                "█",
                Style::default().fg(Color::Cyan),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "│",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_selected_path(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let rows = app.build_rows();
    let p = rows
        .get(app.cursor)
        .and_then(|r| r.path.clone())
        .unwrap_or_else(|| app.root.path.clone());
    let mut s = display_path(&p);
    let max_w = area.width as usize;
    if s.chars().count() > max_w {
        let take_tail = max_w.saturating_sub(1);
        let tail: String = s
            .chars()
            .rev()
            .take(take_tail)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        s = format!("…{}", tail);
    }
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            s,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ))),
        area,
    );
}

fn draw_filter_bar(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let rows = app.build_rows();
    // Match count: count rows that have a non-empty name (skip root header).
    let matches = rows
        .iter()
        .filter(|r| !r.name.is_empty() && Some(&r.path) != Some(&Some(app.root.path.clone())))
        .count();
    let cursor = if app.filter.editing { "█" } else { "" };
    let line = Line::from(vec![
        Span::styled(
            " Filter: ",
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            app.filter.query.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(cursor, Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled(
            format!("({} matches)", matches),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_status(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    // Row 1: path + totals + warnings.
    let path_str = breadcrumb(&app.root.path);
    let warn = if app.warnings.is_empty() {
        String::new()
    } else {
        format!("  ⚠ {} (w)", app.warnings.len())
    };

    let msg_override = if app.rescanning {
        Some("Scanning…".to_string())
    } else if let Some((msg, t)) = &app.status_msg {
        if t.elapsed() < Duration::from_secs(3) {
            Some(msg.clone())
        } else {
            None
        }
    } else {
        None
    };

    if let Some(msg) = msg_override {
        let line = Line::from(Span::styled(
            format!(" {}", msg),
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line), chunks[0]);
    } else {
        let line = Line::from(vec![
            Span::styled(
                " path: ",
                Style::default().bg(Color::DarkGray).fg(Color::Gray),
            ),
            Span::styled(
                path_str,
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
            Span::styled(
                "  |  total: ",
                Style::default().bg(Color::DarkGray).fg(Color::Gray),
            ),
            Span::styled(
                display::human_size(app.root.size),
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  |  files: {}", app.root.file_count()),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
            Span::styled(
                format!("  |  dirs: {}", app.root.dir_count()),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
            Span::styled(
                warn,
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        let para = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
        f.render_widget(para, chunks[0]);
        // Whole row is a click zone for warnings if any.
        if !app.warnings.is_empty() {
            app.hits.push(HitRegion {
                rect: chunks[0],
                action: Action::OpenWarnings,
            });
        }
    }

    // Row 2: clickable toggles.
    let row2 = chunks[1];
    let mut x = row2.x;
    let y = row2.y;

    // Background fill
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(Color::Black)),
        row2,
    );

    let segs: Vec<(String, Action, Color)> = vec![
        (
            format!(" sort: {} ", app.sort_label()),
            Action::CycleSort,
            Color::Magenta,
        ),
        (
            format!(" mode: {:?} ", app.config.mode),
            Action::SetMode(app.config.mode),
            Color::Cyan,
        ),
        (
            format!(" hidden: {} ", onoff(app.config.view.show_hidden)),
            Action::ToggleHidden,
            Color::Yellow,
        ),
        (
            format!(
                " empty: {} ",
                if app.config.view.show_empty {
                    "shown"
                } else {
                    "hidden"
                }
            ),
            Action::ToggleEmpty,
            Color::Yellow,
        ),
        (
            format!(" %: {} ", onoff(app.config.view.show_percent)),
            Action::TogglePercent,
            Color::Yellow,
        ),
        (
            format!(" mod: {} ", onoff(app.config.view.show_modified)),
            Action::ToggleModified,
            Color::Yellow,
        ),
    ];

    for (label, action, fg) in segs {
        let w = label.chars().count() as u16;
        if x + w > row2.x + row2.width {
            break;
        }
        let rect = Rect::new(x, y, w, 1);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                label,
                Style::default().fg(fg).bg(Color::Black),
            ))),
            rect,
        );
        app.hits.push(HitRegion { rect, action });
        x = x.saturating_add(w + 1);
    }

    // Filter status segment (clickable to clear)
    if app.filter.active {
        let label = format!(" filter: \"{}\" [x] ", app.filter.query);
        let w = label.chars().count() as u16;
        if x + w <= row2.x + row2.width {
            let rect = Rect::new(x, y, w, 1);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))),
                rect,
            );
            app.hits.push(HitRegion {
                rect,
                action: Action::ClearFilter,
            });
        }
    }
}

fn draw_warnings(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);

    let inner_h = popup.height.saturating_sub(2) as usize;
    let header = vec![
        Line::from(Span::styled(
            format!(" Warnings  ({} skipped) ", app.warnings.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "j/k or ↑↓: scroll   PgUp/PgDn   q/Esc/w: close",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];
    let body_h = inner_h.saturating_sub(header.len()).max(1);

    let mut lines: Vec<Line> = header;
    if app.warnings.is_empty() {
        lines.push(Line::from(Span::styled(
            "No warnings — nothing was skipped.",
            Style::default().fg(Color::Green),
        )));
    } else {
        for w in app.warnings.iter().skip(app.warnings_scroll).take(body_h) {
            lines.push(Line::from(Span::styled(
                w.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }
    }

    let block = Block::default().borders(Borders::ALL).title(" Warnings ");
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

fn draw_help(f: &mut ratatui::Frame, area: Rect, _app: &App) {
    let popup = centered_rect(70, 80, area);
    f.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "diskvis — keybinds",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Navigation"),
        Line::from("  j / k / ↑↓     navigate one row"),
        Line::from("  PgUp / PgDn    half page"),
        Line::from("  g / Home       jump to top"),
        Line::from("  G / End        jump to bottom"),
        Line::from("  Enter          drill into directory"),
        Line::from("  u / Backspace  go up"),
        Line::from("  ~              jump to home"),
        Line::from("  r              rescan current directory"),
        Line::from(""),
        Line::from("Display"),
        Line::from("  1 2 3 4        switch mode (tree/bars/treemap/flat)"),
        Line::from("  s              cycle sort (size↓ size↑ name↑ name↓)"),
        Line::from("  c              cycle theme"),
        Line::from("  f              toggle showing files"),
        Line::from("  . or h         toggle hidden files"),
        Line::from("  d              toggle empty dirs"),
        Line::from("  p              toggle percentage column"),
        Line::from("  m              toggle modified column"),
        Line::from(""),
        Line::from("Filter"),
        Line::from("  /              open filter bar"),
        Line::from("  Esc            clear filter"),
        Line::from(""),
        Line::from("Actions"),
        Line::from("  x              exclude selected entry (session)"),
        Line::from("  o              open in system file manager (xdg-open)"),
        Line::from("  y              yank path to clipboard"),
        Line::from("  e              open file in editor"),
        Line::from("  i              permission inspector"),
        Line::from("  S              settings panel"),
        Line::from("  w              warnings overlay"),
        Line::from("  Space / Ctrl-P open path spotlight"),
        Line::from("  ?              this help"),
        Line::from("  q / Ctrl-C     quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Mouse: click rows, double-click drills, scroll wheel navigates.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default().borders(Borders::ALL).title(" Help ");
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

fn draw_settings(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let popup = centered_rect(70, 80, area);
    f.render_widget(Clear, popup);

    let items = app.settings_items();
    let inner_h = popup.height.saturating_sub(2) as usize;
    let header_lines = 3;
    let visible = inner_h.saturating_sub(header_lines).max(1);

    // Adjust scroll so cursor stays visible.
    if app.settings_cursor < app.settings_scroll {
        app.settings_scroll = app.settings_cursor;
    } else if app.settings_cursor >= app.settings_scroll + visible {
        app.settings_scroll = app.settings_cursor + 1 - visible;
    }

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "Settings",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Enter/Space: toggle  +/-: depth  PgUp/PgDn: scroll  q/Esc: close (saves)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];

    let inner_x = popup.x + 1;
    let inner_y = popup.y + 1;

    // Hit regions for each visible item.
    for (vis_idx, (i, item)) in items
        .iter()
        .enumerate()
        .skip(app.settings_scroll)
        .take(visible)
        .enumerate()
    {
        let prefix = if i == app.settings_cursor { "▶ " } else { "  " };
        let style = if i == app.settings_cursor {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        } else {
            Style::default()
        };
        let extra_style = match item.kind {
            SettingKind::ResetDefaults => Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::BOLD),
            SettingKind::ClearFilter => Style::default().fg(Color::Yellow),
            _ => style,
        };
        lines.push(Line::from(vec![
            Span::raw(prefix.to_string()),
            Span::styled(item.label.clone(), extra_style),
        ]));
        let row_y = inner_y + (header_lines as u16) + vis_idx as u16;
        let rect = Rect::new(inner_x, row_y, popup.width.saturating_sub(2), 1);
        let action = match item.kind {
            SettingKind::ResetDefaults => Action::SettingsResetDefaults,
            SettingKind::ClearFilter => Action::SettingsClearFilter,
            _ => Action::SettingsRow(i),
        };
        app.hits.push(HitRegion { rect, action });
    }

    let block = Block::default().borders(Borders::ALL).title(" Settings ");
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, popup);

    // Also register a close-overlay hit on the [✕] in the corner of the popup
    // (already in title bar). And clicks outside popup close as well.
    // Anywhere outside the popup → close.
    // (added as a low-priority backstop region: full-screen close, but checked
    // last since hit-test stops at first match)
    let _ = (Action::CloseOverlay, area); // unused
}

fn draw_spotlight(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(s) = app.spotlight.as_ref() else {
        return;
    };

    // 60% wide, fixed height (12 rows), positioned in the upper third.
    let popup_w = (area.width as u32 * 60 / 100) as u16;
    let popup_w = popup_w.max(40).min(area.width.saturating_sub(2));
    let popup_h: u16 = 12;
    let popup_h = popup_h.min(area.height.saturating_sub(2));
    let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let popup_y = area.y + area.height / 6;
    let popup = Rect::new(popup_x, popup_y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Spotlight ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    if inner.height < 4 {
        return;
    }

    // Layout inside: input(1) | divider(1) | results(rest-2) | hint(1)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Input row.
    let input_line = Line::from(vec![
        Span::styled(
            "  󰍉 ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            s.input.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled("█", Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(input_line), chunks[0]);

    // Divider.
    let divider: String = "─".repeat(chunks[1].width as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            divider,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );

    // Results.
    let results_area = chunks[2];
    let max_visible = results_area.height as usize;
    let take = max_visible.min(8);
    if !s.candidates.is_empty() {
        // Window the candidates around `selected`.
        let start = if s.selected >= take {
            s.selected + 1 - take
        } else {
            0
        };
        for (vis_i, (idx, cand)) in s
            .candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(take)
            .enumerate()
        {
            let row_y = results_area.y + vis_i as u16;
            let row_rect = Rect::new(results_area.x, row_y, results_area.width, 1);
            let selected = idx == s.selected;
            let bg = if selected {
                Color::Rgb(40, 40, 70)
            } else {
                Color::Reset
            };
            let arrow = if selected { "  ▸ " } else { "    " };
            let name_color = if cand.is_dir {
                Color::Blue
            } else {
                Color::White
            };
            let mut spans = vec![
                Span::styled(
                    arrow,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                        .bg(bg),
                ),
                Span::styled(
                    display_path(&cand.path),
                    Style::default()
                        .fg(name_color)
                        .bg(bg)
                        .add_modifier(if cand.is_dir {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if cand.is_dir {
                spans.push(Span::styled(
                    "/",
                    Style::default().fg(Color::Blue).bg(bg),
                ));
            }
            let para =
                Paragraph::new(Line::from(spans)).style(Style::default().bg(bg));
            f.render_widget(para, row_rect);
        }
    } else {
        let para = Paragraph::new(Line::from(Span::styled(
            "    (no matches)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
        f.render_widget(para, results_area);
    }

    // Hint row.
    let hint = Line::from(Span::styled(
        " ↑↓ navigate   Tab complete   →/Tab accept   Enter jump   Ctrl-W del-component   Esc cancel ",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ));
    f.render_widget(Paragraph::new(hint), chunks[3]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[allow(dead_code)]
fn path_eq(a: Option<&Path>, b: Option<&Path>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

// ============================================================================
// Permissions inspector
// ============================================================================

fn handle_perms_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    // Confirm-recursive sub-modal first.
    if let Some(state) = app.perms.as_mut() {
        if state.confirm_recursive {
            match key.code {
                KeyCode::Esc => {
                    state.confirm_recursive = false;
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    state.confirm_cursor ^= 1;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    let cursor = state.confirm_cursor;
                    state.confirm_recursive = false;
                    if cursor == 0 {
                        // Yes -> apply recursively (root only).
                        if !is_root() {
                            app.flash("recursive apply requires sudo/root");
                            return;
                        }
                        let s = app.perms.as_ref().unwrap().clone_apply_target();
                        match s {
                            Ok(_) => {
                                let res = app.perms.as_ref().unwrap().apply_recursive();
                                match res {
                                    Ok(n) => app.flash(format!("applied to {} entries", n)),
                                    Err(e) => app.flash(format!("apply failed: {}", e)),
                                }
                            }
                            Err(e) => app.flash(e),
                        }
                    }
                }
                _ => {}
            }
            return;
        }
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => {
            app.perms = None;
            app.view = View::Main;
        }
        KeyCode::Left => {
            if let Some(s) = app.perms.as_mut() {
                s.move_cursor(-1, 0);
            }
        }
        KeyCode::Right => {
            if let Some(s) = app.perms.as_mut() {
                s.move_cursor(1, 0);
            }
        }
        KeyCode::Up => {
            if let Some(s) = app.perms.as_mut() {
                s.move_cursor(0, -1);
            }
        }
        KeyCode::Down => {
            if let Some(s) = app.perms.as_mut() {
                s.move_cursor(0, 1);
            }
        }
        KeyCode::Char(' ') => {
            if let Some(s) = app.perms.as_mut() {
                if s.cursor < 12 {
                    s.toggle_cursor();
                } else {
                    activate_perms_button(app);
                }
            }
        }
        KeyCode::Enter => {
            if let Some(s) = app.perms.as_mut() {
                if s.cursor < 12 {
                    s.toggle_cursor();
                } else {
                    activate_perms_button(app);
                }
            }
        }
        _ => {}
    }
}

fn activate_perms_button(app: &mut App) {
    let Some(state) = app.perms.as_ref() else {
        return;
    };
    match state.cursor {
        12 => {
            // Apply
            if !is_root() {
                app.flash("apply requires sudo/root");
                return;
            }
            match state.apply() {
                Ok(()) => app.flash("permissions applied"),
                Err(e) => app.flash(format!("apply failed: {}", e)),
            }
        }
        13 => {
            // Apply Recursively → confirm modal
            if let Some(s) = app.perms.as_mut() {
                s.confirm_recursive = true;
                s.confirm_cursor = 1;
            }
        }
        14 => {
            // Cancel
            app.perms = None;
            app.view = View::Main;
        }
        _ => {}
    }
}

impl PermsState {
    /// Compatibility shim used by the recursive-apply path so callers can
    /// uniformly produce a `Result` while we keep the `apply()` signature
    /// stable.
    pub fn clone_apply_target(&self) -> Result<(), String> {
        if !self.path.exists() {
            return Err(format!("path no longer exists: {}", self.path.display()));
        }
        Ok(())
    }
}

fn draw_permissions(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(s) = app.perms.as_ref() else {
        return;
    };

    let popup = centered_rect(64, 60, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            format!(" Permissions: {} ", s.path.display()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("  Owner: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} ({})", s.owner_name, s.uid),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled("Group: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} ({})", s.group_name, s.gid),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Mode:  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}  ({:o})", mode_string(s.mode, s.is_dir), s.mode),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(""));

    // 3x3 grid of rwx toggles for owner / group / other.
    let labels = ["Owner ", "Group ", "Other "];
    for (row_idx, label) in labels.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(*label, Style::default().fg(Color::Cyan)),
        ];
        for col_idx in 0..3 {
            let cell = row_idx * 3 + col_idx;
            let bit = 1u32 << (8 - cell);
            let on = s.mode & bit != 0;
            let ch = match col_idx {
                0 => 'r',
                1 => 'w',
                _ => 'x',
            };
            let label_text = if on { format!("[{}]", ch) } else { "[ ]".to_string() };
            let mut style = Style::default().fg(if on { Color::Green } else { Color::DarkGray });
            if cell == s.cursor {
                style = style.bg(Color::Rgb(40, 40, 70)).add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(label_text, style));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));

    // Special bits row.
    let specials = [
        (0o4000u32, "SUID"),
        (0o2000u32, "SGID"),
        (0o1000u32, "Sticky"),
    ];
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  Special  ")];
    for (i, (bit, label)) in specials.iter().enumerate() {
        let cell = 9 + i;
        let on = s.mode & *bit != 0;
        let text = if on {
            format!("[x] {}  ", label)
        } else {
            format!("[ ] {}  ", label)
        };
        let mut style = Style::default().fg(if on { Color::Green } else { Color::DarkGray });
        if cell == s.cursor {
            style = style.bg(Color::Rgb(40, 40, 70)).add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(text, style));
    }
    lines.push(Line::from(spans));
    lines.push(Line::from(""));

    // Buttons row.
    let root = is_root();
    let buttons = [
        (12usize, "[ Apply ]"),
        (13usize, "[ Apply Recursively ]"),
        (14usize, "[ Cancel ]"),
    ];
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    for (cell, label) in buttons.iter() {
        let active = *cell == s.cursor;
        let disabled = !root && (*cell == 12 || *cell == 13);
        let mut style = if disabled {
            Style::default().fg(Color::DarkGray)
        } else if *cell == 14 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        };
        if active {
            style = style.bg(Color::Rgb(40, 40, 70)).add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled((*label).to_string(), style));
        spans.push(Span::raw("  "));
    }
    lines.push(Line::from(spans));
    if !root {
        lines.push(Line::from(Span::styled(
            "  (Apply requires sudo/root — only Cancel is available)",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Arrows: navigate   Space: toggle   Enter: activate   Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);

    // Confirmation dialog overlay.
    if s.confirm_recursive {
        let conf = centered_rect(50, 25, area);
        f.render_widget(Clear, conf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title(Span::styled(
                " Confirm recursive chmod ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(conf);
        f.render_widget(block, conf);
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(format!(
            "  Apply mode {:o} to ALL entries under:",
            s.mode
        )));
        lines.push(Line::from(format!("    {}", s.path.display())));
        lines.push(Line::from(""));
        let yes_style = if s.confirm_cursor == 0 {
            Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red)
        };
        let no_style = if s.confirm_cursor == 1 {
            Style::default().bg(Color::Green).fg(Color::Black).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(" Yes, apply ", yes_style),
            Span::raw("    "),
            Span::styled(" Cancel ", no_style),
        ]));
        let para = Paragraph::new(lines);
        f.render_widget(para, inner);
    }
}

fn mode_string(mode: u32, is_dir: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_dir { 'd' } else { '-' });
    let triplet = |bits: u32, ext_bit: u32, ext_lower: char, ext_upper: char| -> String {
        let r = if bits & 0o4 != 0 { 'r' } else { '-' };
        let w = if bits & 0o2 != 0 { 'w' } else { '-' };
        let x_on = bits & 0o1 != 0;
        let x = if mode & ext_bit != 0 {
            if x_on { ext_lower } else { ext_upper }
        } else if x_on {
            'x'
        } else {
            '-'
        };
        format!("{}{}{}", r, w, x)
    };
    s.push_str(&triplet((mode >> 6) & 0o7, 0o4000, 's', 'S'));
    s.push_str(&triplet((mode >> 3) & 0o7, 0o2000, 's', 'S'));
    s.push_str(&triplet(mode & 0o7, 0o1000, 't', 'T'));
    s
}

// ============================================================================
// Editor view
// ============================================================================

fn draw_editor(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let Some(editor) = app.editor.as_mut() else {
        return;
    };
    editor.report_size(area.width, area.height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Title bar
    let title = editor.title();
    let title_style = Style::default()
        .bg(Color::Rgb(20, 30, 50))
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {:width$}", title, width = chunks[0].width as usize),
            title_style,
        ))),
        chunks[0],
    );

    // Body (highlighted lines)
    let lines = editor.view_lines(chunks[1].height, chunks[1].width);
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Rgb(15, 15, 25))),
        chunks[1],
    );

    // Hint bar
    let hint = editor.hint();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[2],
    );

    // Prompts overlay
    if let Some(prompt) = editor.prompt.clone() {
        match prompt {
            EditorPrompt::Message { text, .. } => {
                let bar = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!(" {} ", text),
                        Style::default()
                            .bg(Color::DarkGray)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ))),
                    bar,
                );
            }
            EditorPrompt::GotoLine { input } => {
                let bar = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(
                            " Goto line: ",
                            Style::default().bg(Color::Yellow).fg(Color::Black),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            input,
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ])),
                    bar,
                );
            }
            EditorPrompt::UnsavedClose { selection } => {
                let popup = centered_rect(50, 25, area);
                f.render_widget(Clear, popup);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" Unsaved changes ");
                let inner = block.inner(popup);
                f.render_widget(block, popup);
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(""));
                lines.push(Line::from(
                    "  This file has unsaved changes.".to_string(),
                ));
                lines.push(Line::from(""));
                let opt_style = |i: u8| -> Style {
                    if i == selection {
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Yellow)
                    }
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(" Save & Close ", opt_style(0)),
                    Span::raw("  "),
                    Span::styled(" Discard ", opt_style(1)),
                    Span::raw("  "),
                    Span::styled(" Cancel ", opt_style(2)),
                ]));
                f.render_widget(Paragraph::new(lines), inner);
            }
            EditorPrompt::LargeFile { selection, size } => {
                let popup = centered_rect(60, 30, area);
                f.render_widget(Clear, popup);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red))
                    .title(" Large or binary file ");
                let inner = block.inner(popup);
                f.render_widget(block, popup);
                let mb = size as f64 / (1024.0 * 1024.0);
                let mut lines: Vec<Line<'static>> = Vec::new();
                lines.push(Line::from(""));
                lines.push(Line::from(format!(
                    "  This file is {:.1} MB or appears to contain binary data.",
                    mb,
                )));
                lines.push(Line::from(
                    "  Editing it may be slow or destructive.".to_string(),
                ));
                lines.push(Line::from(""));
                let opt_style = |i: u8| -> Style {
                    if i == selection {
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Yellow)
                    }
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(" Open anyway ", opt_style(0)),
                    Span::raw("    "),
                    Span::styled(" Cancel ", opt_style(1)),
                ]));
                f.render_widget(Paragraph::new(lines), inner);
            }
        }
    }
}
