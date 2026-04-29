# diskvis — Session Handoff (v1.8.0, April 2026)

This document is a self-contained context dump for another model picking up
work on `diskvis`. It captures the architecture, the conventions, every
feature that has been built across the multi-session history, every bug we
hit + how we fixed it, and the open follow-ups. It is intentionally
exhaustive: read it once and you should be able to commit the next change
without re-discovering anything.

---

## 1. Project at a glance

`diskvis` is a Rust 2021 / stable terminal disk-usage visualizer.
Cargo `name = "diskvis"`, current `version = "1.8.0"`. It runs on Linux and
macOS (Windows is intentionally **dropped**: Unix-only `MetadataExt` /
`PermissionsExt` is used freely throughout). Two execution modes:

- **Plain CLI** — pipes ANSI-styled rows to stdout when `--no-color`,
  output is piped, or `cli.json` is set.
- **Interactive TUI** — ratatui 0.29 + crossterm 0.28, mouse capture, mode
  pills, filter bar, multiple overlays, Spotlight popup, embedded text
  editor, permissions inspector, new-file/new-dir popup, settings, help,
  warnings, treemap. Splash screen renders during the initial scan if it
  takes longer than ~300 ms.

### Build / repo conventions

```bash
cargo build                                # debug
cargo build --release                      # release (lto = true, codegen-units = 1)
RUSTFLAGS="-D warnings" cargo build        # what every change must satisfy
./release.sh <version>                     # tag, build, publish to crates.io with --allow-dirty
```

The user's standard close to every prompt is:
> *"After changes run cargo build and fix all errors until clean with zero
> warnings."*

This is non-negotiable. Always finish with a clean `cargo build` and
prefer the strict `-D warnings` invocation if anything looks suspicious.

The repo is at `/home/philipp/diskvis`. Earlier sessions ran on a
different host where `target/` was root-owned and required
`CARGO_TARGET_DIR=/tmp/diskvis-target`; that constraint **no longer
applies** on the current host. The current `target/` is writable.

### Cargo deps

```
clap 4.5 (derive), walkdir 2.5, colored 2.1,
ratatui 0.29, crossterm 0.28, indicatif 0.17,
serde 1 (derive), serde_json 1, chrono 0.4 (serde),
human-panic 2, toml 0.8, arboard 3, dirs 5,
rayon 1, users 0.11,
syntect 5 (default-features = false, default-fancy),
libc 0.2.
```

Release profile sets `lto = true, codegen-units = 1` for a small + fast
single binary.

---

## 2. Source map

```
src/
├── cli.rs          — clap CLI, SortBy / SortOrder / Mode enums.
├── config.rs       — Config (~/.config/diskvis/config.toml), ViewOptions,
│                     Theme, default excludes including /-prefixed VFS
│                     paths. depth = 1 by default.
├── walker.rs       — Filesystem traversal returning a Node tree +
│                     warnings. Hand-rolled (no longer walkdir-based for
│                     the main scan): builds a Node tree with rayon
│                     depth-gating, same-FS symlink follow, pre-allocated
│                     children vecs. v1.8 added depth-counter + root_dev
│                     params to build_dir.
├── main.rs         — Entry point. Parses args; on UNIX raises process
│                     priority via libc::setpriority(PRIO_PROCESS, 0, -5).
│                     Drives the initial scan with a centered splash
│                     (TUI mode) or an indicatif spinner (plain mode),
│                     then hands off to tui::run or print_rows.
├── editor.rs       — Embedded text editor: syntect highlight, undo/redo,
│                     atomic save (.diskvis-PID.tmp + rename), modal
│                     prompts (UnsavedClose / GotoLine / Message /
│                     LargeFile). 1037 lines.
├── tui.rs          — 3838 lines. App state, run loop, key + mouse
│                     handling, drawing of all panels and overlays,
│                     Spotlight worker thread, async scan worker, all
│                     overlays, status bar, hit-test regions.
└── display/
    ├── mod.rs      — RenderOptions, Row { Clone }, Theme styles,
    │                 human_size, fmt_modified, hidden-summary helpers.
    ├── tree.rs     — Indented ├── / └── tree (default mode).
    ├── bars.rs     — Horizontal bar histogram with Hidden-Items synthetic
    │                 row that participates in sort.
    └── treemap.rs  — Binary-split squarified treemap drawn into a
                      ratatui Buffer; CLI fallback re-encodes the buffer
                      back into Spans.
```

`flat.rs` was **removed** — `Mode` no longer has a `Flat` variant; Tree is
the default and `show_files` is the toggle that brings file rows in.

Per-file line counts (current):

```
src/cli.rs              87
src/config.rs          148
src/display/bars.rs    183
src/display/mod.rs     277
src/display/tree.rs    100
src/display/treemap.rs 413
src/editor.rs         1037
src/main.rs            394
src/tui.rs            3838
src/walker.rs          339
total                 6816
```

---

## 3. Key types

```rust
// display/mod.rs
#[derive(Clone)]                           // v1.8 made this Clone-able.
pub struct Row {
    pub line: Line<'static>,
    pub path: Option<PathBuf>,
    pub is_dir: bool,
    pub size: u64,
    pub name: String,
    pub is_hidden_summary: bool,           // synthetic Hidden Items marker
}

#[derive(Clone, Copy)]
pub struct RenderOptions {
    pub theme: Theme,
    pub depth: usize,
    pub min_size: u64,
    pub sort: cli::SortBy,
    pub sort_order: cli::SortOrder,
    pub show_files: bool,
    pub show_modified: bool,
    pub width: u16,
    pub height: u16,
    pub hidden_summary: Option<(u64 /*count*/, u64 /*size*/)>,
}

// walker.rs
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
    pub mode: Option<u32>,                 // unix mode bits (12 lower bits)
    pub children: Vec<Node>,
}

impl Node {
    pub fn has_unusual_perms(&self) -> bool { /* world-writable / suid / sgid */ }
    pub fn placeholder(path: PathBuf) -> Self { /* v1.8: empty stand-in */ }
    pub fn file_count(&self) -> usize { … }
    pub fn dir_count(&self) -> usize { … }
}

pub struct ScanResult {
    pub root: Node,
    pub warnings: Vec<String>,
}

pub struct WalkOptions<'a> {
    pub excludes: &'a [String],
    pub on_progress: Option<&'a (dyn Fn(&Path) + Sync)>,
}
```

The walker takes `&Path` + `&WalkOptions` and returns
`io::Result<ScanResult>`. There is **no depth parameter** on the walker —
it always builds the full tree. `app.config.depth` is purely a render cap
applied inside `display::tree::render` / `bars::render` /
`treemap::render`. (This matters: see §6.6.)

```rust
// tui.rs  (v1.8 fields, in addition to existing UI state)
pub struct App {
    pub root: Node,
    pub initial_path: PathBuf,
    pub cursor: usize,
    pub scroll: usize,
    pub config: Config,
    pub min_size: u64,
    view: View,                            // Main | Settings | Help | Warnings
                                           //   | Permissions | Editor | NewItem
    settings_cursor: usize,
    settings_scroll: usize,
    nav_stack: Vec<PathBuf>,
    status_msg: Option<(String, Instant)>,
    last_size: (u16, u16),
    rescanning: bool,                      // drives spinner in draw_status
    pub warnings: Vec<String>,
    warnings_scroll: usize,
    filter: FilterState,
    hits: Vec<HitRegion>,
    body_rect: Rect,
    body_scroll: usize,
    body_visible_rows: usize,
    last_click: Option<(Instant, u16, u16)>,
    session_excludes: Vec<String>,         // 'x' to exclude under cursor
    pub spotlight: Option<SpotlightState>,
    spot_tx: Option<Sender<SpotlightReq>>,
    spot_rx: Option<Receiver<SpotlightResp>>,
    pub perms: Option<PermsState>,
    pub editor: Option<Editor>,
    pub new_item: Option<NewItemState>,
    pub should_quit: bool,

    // === v1.8 performance fields ===
    cached_rows: RefCell<Option<(u64, Vec<Row>)>>,
    treemap_cache: RefCell<Option<TreemapCache>>,
    scan_rx: Option<Receiver<ScanMsg>>,
    scan_target: Option<PathBuf>,
    scan_cache: HashMap<PathBuf, ScanCacheEntry>,
}
```

`Row` is `Clone` so `build_rows()` can return a cached vec by `clone()`.
`cached_rows` and `treemap_cache` are `RefCell<…>` so the `&self`
`build_rows()` / `draw_treemap_into()` can mutate the cache.

---

## 4. Feature catalog (chronological)

This is everything `diskvis` supports today. Items grouped by the version
that introduced them.

### 4.1 Original TUI (pre-v1.3)

- ratatui rewrite with mouse capture + scrollbar.
- Mode pills (Tree / Bars / Treemap).
- Filter pill: press `/` to open input, `Esc` to clear.
- Settings overlay (`S`): all view toggles, depth +/-, theme cycle,
  exclude toggles, reset-to-defaults.
- Help overlay (`?`).
- Warnings overlay (`w`): permission errors / IO failures buffered by the
  walker so the TUI never `eprintln!`s during raw mode.
- Spotlight path-jumper popup (`Space` / `Ctrl+P`): worker-thread async
  autocomplete with mpsc, generation counters, 50 ms debounce, per-parent
  cache, `Tab` LCP completion, `Ctrl-W`/`Ctrl-U` editing.
- Hidden-items synthetic row in tree / bars when `view.show_hidden=false`.
- Per-mode toggles: `j/k Up/Down`, `g/G`, `Home/End`, `PageUp/PageDown`,
  arrow keys, mouse wheel, click navigation; cursor never lands on a
  synthetic summary row.
- Atomic config save on exit (`~/.config/diskvis/config.toml`).

### 4.2 v1.3 — drop Windows + permissions inspector

- All Windows code paths removed; switched to `std::os::unix::fs::*`
  freely.
- Permissions inspector overlay (`i` on a selected entry): rwx grid for
  owner/group/other + suid/sgid/sticky toggles + Apply / Apply Recursively
  / Cancel buttons. Recursive apply walks the subtree with rayon and
  flashes a count summary.
- `Node.mode: Option<u32>` populated during scan; `has_unusual_perms()`
  flags world-writable / setuid / setgid for visual highlight.

### 4.3 v1.4 — embedded editor + macOS .pkg

- `editor.rs` (1037 lines): full text editor opened with `e` on a file.
  - Syntect highlight: `default-fancy` features, syntax picked by
    extension, theme `base16-ocean.dark`. `HighlightLines::new(...)`
    rebuilt on every render starting from line 0 so multi-line styles
    stay correct across the visible window.
  - Undo / redo: snapshot-based with batching window of `UNDO_BATCH_MS`
    so rapid keystrokes coalesce into one undo step. `MAX_UNDO` cap.
  - Modal prompts via `EditorPrompt` enum:
    `UnsavedClose { selection }`, `GotoLine { input }`,
    `Message { text }`, `LargeFile { selection, size }`.
  - Atomic save: write to `.<name>.diskvis-<pid>.tmp` next to the file
    then `fs::rename` over the target. On failure remove the tmp.
  - Tab-aware display columns. `TAB_WIDTH = 4`. Helper
    `display_col_of(line, char_idx)` walks chars, expanding tabs to the
    next multiple of 4.
- macOS `.pkg` build added in CI (release.sh handles the upload step).
- `release.sh` now publishes to crates.io with `cargo publish
  --allow-dirty` so a tagged release can be published without a
  blocking-clean tree (config write at exit otherwise pollutes the tree).

### 4.4 v1.5 — Flat removal + walker gating + splash

- `Mode::Flat` removed. `Mode::Tree` is the default; `show_files` brings
  file rows into the tree view.
- Walker started rayon-gating: parallelize only when a directory has more
  than 4 children — for fewer children parallel dispatch is pure
  overhead.
- Initial scan now driven by a thread + a centered splash screen rendered
  with raw crossterm (no alt-screen) so the TUI's own EnterAlternateScreen
  still works cleanly afterwards. Splash: ASCII-art logo, current path
  spinner, animated bar capped to `inner_w - 4` (fixes earlier clip
  bug), elapsed seconds. On scan completion the splash is cleared and
  the TUI takes over.

### 4.5 v1.6 — new-item popup + editor exit cursor reset

- `n` (new file) / `N` (new directory) opens `NewItemState` overlay:
  - Name input field.
  - Permission octal input (default 644 / 755), validated to 4 digits
    ≤ `0o7777`.
  - Create / Cancel buttons.
  - On create: writes the file (empty) or `mkdir`, applies mode via
    `PermissionsExt`, refreshes the row cache and current view.
  - Errors surfaced in the overlay before commit.
- Editor close (`Ctrl+W`) re-validates the cursor against the row list
  to prevent out-of-bounds indexing on the next draw.
- Splash bar clipping fixed.

### 4.6 v1.7 — depth keybinds + status bar polish

- Default `Config.depth = 1`. Use `+` / `-` to adjust at runtime.
- Status bar gained a `depth: N` segment with an `Action::Noop` click
  region (purely informational — clicking does nothing).
- Editor cursor reset confirmed on Editor → Main transition.

### 4.7 v1.7.1 — editor stack-overflow + invisible-cursor fix (this session)

Two editor bugs fixed in `editor.rs`:

1. **Stack overflow after Ctrl+W (Save & Close)**: the
   `UnsavedClose → selection 0 (Save & Close)` branch called
   `self.save()`, which set a flash `Message` prompt. The next keypress
   entered `handle_key()`, observed `prompt = Some(Message { … })`,
   dispatched to `handle_prompt_key`, which itself recursively called
   `self.handle_key(key)` *without first clearing the prompt*. Each
   recursive call observed the same Message prompt and recursed again
   — stack overflow.
   - **Fix**: in the `EditorPrompt::Message` arm of
     `handle_prompt_key`, clear the prompt and consume the keystroke
     (return `EditorOutcome::Continue`) instead of recursing.
   - Also dropped the now-unused `until: Instant` field from
     `EditorPrompt::Message` and the `Duration` import — neither was
     read anywhere.

2. **No cursor visible in the editor**: `view_lines()` only tinted the
   cursor row's background; it never injected a `█` glyph. UX was "I
   can't see where I am."
   - **Fix**: after the row-fill pad, walk the existing spans of the
     cursor row tracking display columns; when display column ==
     `cursor_disp.saturating_sub(self.scroll_col)`, splice in a styled
     `Span("█", fg=RGB(255,200,0) bold, bg=RGB(30,30,45))` and skip the
     character under the cursor (the block visually replaces it). For
     empty lines / cursor past EOL, pad with spaces and emit the block.
     Tab-aware via `display_col_of(self.cur_line(), self.cursor.1)`.

### 4.8 v1.8.0 — comprehensive performance overhaul (this session)

Released. Headline reason: large directories (tens of thousands of
entries) caused UI hitches and seconds-long stalls. Seven changes:

#### a. Row cache (renderer memoization)

```rust
cached_rows: RefCell<Option<(u64, Vec<Row>)>>
```

`build_rows()` first computes a `u64` hash over every input that affects
the rendered list — root identity (path + size + child count + mtime)
plus every relevant `Config` field, `last_size`, `min_size`, `filter`
state, all excludes (config + session). On hit, the cached `Vec<Row>` is
returned via `clone()`. On miss, the renderer runs and the result is
cached. This eliminates redundant per-frame renders; build_rows had
been called 5+ times per frame from various code paths.

`compute_cache_key()` uses
`std::collections::hash_map::DefaultHasher::new()`. Modified time is
folded in as `Duration::as_nanos()` since `UNIX_EPOCH`.

`invalidate_caches(&self)` (RefCell-mediated) drops both the row cache
and the treemap cache. Called after explicit state changes that aren't
captured by the cache key (e.g. swapping in a placeholder Node).

#### b. Async scanning via mpsc

Walker calls **never** block the UI. `rescan_at(path)`:

1. Validates the path exists + is a directory.
2. If a fresh (≤ 30 s) entry exists in `scan_cache`, swap it in
   immediately (stale-while-revalidate).
3. Otherwise, when navigating to a *different* path, install
   `walker::Node::placeholder(path)` so the body can repaint without
   blanking the screen. When refreshing the *current* root we keep
   showing the existing tree until the new scan completes.
4. Call `start_background_scan(path)`.

`start_background_scan(path)`:

1. **De-dupes**: if `scan_target == Some(path)` and `scan_rx.is_some()`,
   returns immediately. Prevents holding `r` from spawning N identical
   orphan worker threads.
2. Builds the excludes vec by combining `config.active_excludes()` and
   `session_excludes`.
3. Spawns a thread that:
   - Calls `libc::setpriority(PRIO_PROCESS, 0, 10)` — nice itself
     down so the UI thread always wins the scheduler.
   - Runs `walker::build_tree(&scan_path, &opts)`.
   - Sends `ScanMsg { target, result }` over the `mpsc::channel`.
4. Flashes `scan: <path> (render depth N)` so the depth currently
   honoured by the renderer is visible per scan.

`drain_scan_results()` is called at the top of `run_loop` before each
`terminal.draw(...)`. It does `try_recv()` on `scan_rx`, and on a
result:

- Always inserts the result into `scan_cache` (even if the user
  navigated away).
- Calls `evict_scan_cache()`.
- Only swaps `self.root` when `self.root.path == msg.target` (i.e. the
  scan hasn't been superseded by a navigation event).
- Clamps the cursor to the new last selectable row.
- Calls `invalidate_caches()`.

Poll interval drops from 200 ms → 80 ms while `rescanning` so the
spinner animates smoothly.

#### c. Walker optimizations

- **rayon depth gating**: only depth 0 and 1 fan out into rayon.
  Deeper levels iterate serially. Spawning a worker per directory in
  deep trees was scheduler-bound, not IO-bound. Threshold also keeps
  `subdirs.len() > 4` from earlier.
- **Same-FS symlink follow**: `entry.metadata()` returns the symlink's
  own metadata; we then call `fs::metadata(&path)` to follow the
  link, and only descend if the target is a directory **and**
  `target.dev() == root_dev`. The scan root's `dev` is captured once
  at the top-level `build_tree` call and threaded through every
  recursion. Prevents the scanner from following symlinks into
  `/proc`, network mounts, or arbitrary out-of-tree locations.
- **One syscall per entry**: switched `fs::symlink_metadata(path)` →
  `entry.metadata()`. Saves a stat per file on directories with many
  children.
- **Pre-allocation**:
  `entries: Vec<DirEntry> = read_dir(...).filter_map(|e| e.ok()).collect()`,
  then `Vec::with_capacity(entries.len())` for both `leaves` and
  `subdirs`.

`build_dir` now takes
`(node, opts, warnings, depth: usize, root_dev: u64)`.

#### d. Treemap memoization

```rust
struct TreemapCache { key: u64, width: u16, height: u16, buf: Buffer }
```

`draw_body` calls `app.draw_treemap_into(area, f.buffer_mut())` instead
of running `display::treemap::render_treemap` every frame. The cached
ratatui `Buffer` is blitted (cell-by-cell `*d = s.clone()`) when
`(key, width, height)` matches; only when the cache key changes (root,
sort, depth, etc.) or the area resizes is the binary-split layout
re-run. The CLI plain-mode fallback in `display/treemap.rs::render` is
unchanged — it still allocates a temp Buffer per call.

#### e. Virtual scrolling

`draw_body` already iterated
`rows.iter().enumerate().skip(app.scroll).take(view_h)` — no change
needed beyond the row cache making the underlying `build_rows()` call
free. With both, only the visible window is materialised per frame.

#### f. Process priority

- Main thread: `main.rs` calls `libc::setpriority(PRIO_PROCESS, 0, -5)`
  on entry. Best-effort: requires CAP_SYS_NICE for negative values; a
  failure leaves us at default niceness.
- Scan worker: `setpriority(PRIO_PROCESS, 0, 10)` inside the spawned
  thread.

#### g. Smart scan cache

```rust
scan_cache: HashMap<PathBuf, ScanCacheEntry { root, warnings, at: Instant }>
```

- Insert on every successful scan completion (including superseded
  ones).
- Lookup at `rescan_at` start: hit + age < 30 s → swap in immediately
  (keeps `start_background_scan` running for revalidation).
- Eviction (`evict_scan_cache`):
  - Drop entries older than 5 minutes.
  - If size > 50, keep the 50 most-recent entries (sort by `at` desc).
- Cleared by `r` (explicit rescan) so user-driven refresh always
  re-walks the filesystem.

### 4.9 v1.8 async-scan re-trigger audit (this session)

The user asked for an audit of v1.8's async scan loop for infinite scan
cycles. Findings:

- `drain_scan_results` does **not** spawn another scan. It only updates
  the cache, calls `evict_scan_cache`, and swaps the root if the user
  is still on the same path.
- `rescan_at` had no recursive caller chain.
- `evict_scan_cache` only mutates a HashMap.
- `app.config.depth` is **not** passed to the walker. The walker walks
  the full tree; depth is purely a render cap. So no chance of an
  unbounded "depth = 255" walker scan.
- **Real issue**: `start_background_scan` overwrote `scan_rx`
  unconditionally. Rapid `r` or rapid navigation could spawn N
  identical orphan worker threads — not infinite-recursive, but
  fan-out duplication.
- **Fix applied**: early-return when
  `scan_target == path && scan_rx.is_some()`. Plus per-scan flash
  `scan: <path> (render depth N)` for visibility into the depth value.

---

## 5. Run loop and event flow

```
run() -> run_loop:
  loop {
    drain_spotlight_responses();                 // mpsc try_recv
    if spotlight.is_some() { spotlight_refresh(false); }
    drain_scan_results();                        // v1.8: async scan
    terminal.draw(|f| { last_size = …; draw(f, app, area); });
    poll_ms = if spotlight { 30 }
              else if rescanning { 80 }
              else { 200 };
    if event::poll(poll_ms)? {
      match event::read()? {
        Event::Key(k)   => if handle_key_event(app, k) { return Ok(()); },
        Event::Mouse(m) => handle_mouse_event(app, m); if app.should_quit { return; },
        Event::Resize(_,_) => {}
      }
    }
  }
```

`handle_key_event` priority list (top wins; keys are swallowed in order):

1. `app.editor.is_some()` → editor handles all keys; on
   `EditorOutcome::Close` tear down and return to `View::Main`,
   reclamping cursor.
2. `app.new_item.is_some()` → `handle_new_item_key`.
3. `app.perms.is_some()`   → `handle_perms_key`.
4. `app.spotlight.is_some()` → `handle_spotlight_key`.
5. `filter.editing` → filter input swallows almost everything.
6. Otherwise main keymap (mode pills, navigation, depth `+` / `-`,
   `r` rescan, `e` edit, `i` perms, `n` / `N` new, `Space` /
   `Ctrl+P` spotlight, `?` help, `w` warnings, `S` settings,
   `q` / `Esc`).

`draw(f, app, area)`:

1. Layout: title (1) | body (min) | sel-path (1) | filter-bar (0/1) |
   status (2).
2. `draw_title`, `draw_body`, `draw_selected_path`,
   `draw_filter_bar` (when active), `draw_status`.
3. Overlay match by `app.view`:
   `Help / Settings / Warnings / Permissions / Editor / NewItem /
   Main(no overlay)`.
4. Spotlight is independent of `view` — drawn last so it overlays
   everything when `spotlight.is_some()`.

---

## 6. Bugs encountered + fixes (running log)

### 6.1 Bars Hidden-Items row wrapping

`Paragraph::wrap` broke long lines on whitespace; the trailing
percent column was the longest span and broke first. **Fix**: bake the
percent into the synthetic Node's `name`, set the emitted Row's
`name = String::new()` so `draw_body` does not append its own percent
span, and skip the modified column entirely for the synthetic row. The
synthetic row's `is_hidden_summary = true`; cursor / mouse skip it.

### 6.2 ViewOptions defaults inverted

Earlier round — fixed by setting `show_hidden=true, show_empty=false,
show_percent=true, show_modified=true` in `ViewOptions::default()`.

### 6.3 Splash bar clipping

The progress bar bled into the terminal border on narrow widths.
**Fix**: cap the bar to `inner_w - 4`.

### 6.4 Editor stack overflow (v1.7.1)

Save & Close → `flash()` set a Message prompt → next key entered the
Message arm of `handle_prompt_key` which **recursively called
self.handle_key(key)** without first clearing the prompt → infinite
recursion → stack overflow. **Fix**: clear the prompt and return
`Continue`. Also removed the unused `until` field on Message and the
now-orphan `Duration` import.

### 6.5 Editor invisible cursor (v1.7.1)

`view_lines` only tinted the cursor row's background. **Fix**: inject
a styled `█` block at `cursor_disp - scroll_col` by walking spans
display-column-aware and splicing.

### 6.6 v1.8 async-scan duplicate worker threads

See §4.9. Fixed with the same-path guard in `start_background_scan`.

### 6.7 Generic conventions hit through the sessions

- `target/` was root-owned on a previous host, requiring
  `CARGO_TARGET_DIR=/tmp/diskvis-target`. **No longer applies on the
  current host** but worth keeping in mind if the user moves machines.
- `rg` is sometimes unavailable; fall back to `grep_search` /
  `grep` / `find`.
- TUI must NEVER `eprintln!` while raw mode is on. Buffer warnings
  through `walker::ScanResult.warnings` and surface via the warnings
  overlay.

---

## 7. Conventions to preserve

1. **Rows are the source of truth** for both plain mode and the TUI.
   Keep them consistent: any new row type or styling must compile
   through `display::print_rows` and `tui::draw_body` unchanged.
2. **`is_hidden_summary` is the single marker** for non-selectable
   synthetic rows. Don't add ad-hoc sentinel checks elsewhere; extend
   `selectable_len` / `step_forward` / `step_backward` /
   `snap_cursor_to_selectable` if needed.
3. **`Row.name == ""` means "do not append percent"** in `draw_body`.
   Load-bearing for the bars synthetic row.
4. **TUI must never `eprintln!`** during runtime — alt-screen + raw
   mode. Use `flash()` for transient messages and the warnings overlay
   for persistent ones.
5. **Builds must be warning-free.** Always finish with a clean
   `cargo build`. The user expects `RUSTFLAGS="-D warnings" cargo
   build` to pass.
6. **Don't refactor unrequested code.** Targeted, minimal edits.
7. **Don't add docstrings/comments/types to code you didn't change.**
8. **`build_rows()` is `&self`**; it goes through the `RefCell` cache.
   Don't make it `&mut self` — that would force every caller path to
   thread mutable borrows through the App.
9. **The walker has no depth knob.** If you ever need a max-depth cap
   in the walker itself, add it as a `WalkOptions` field, do **not**
   re-use `app.config.depth` (which is a render cap).
10. **Async scan dedup**: if you add new scan-spawning code paths,
    route through `start_background_scan` so the same-path guard is
    honoured.
11. **Row clone on cache hit is intentional.** Renderer outputs are
    measured-cheap; cloning a `Vec<Row>` of a few thousand items is
    well below a single render. Don't try to return `&[Row]` — the
    `RefCell` borrow would leak.
12. **Editor prompts must clear themselves before returning.** Never
    recurse into `handle_key` from a `handle_prompt_key` arm without
    first clearing `self.prompt`. (See §6.4.)

---

## 8. Quick-reference: where to make common changes

| Want to… | Edit |
|---|---|
| Add a new view toggle | `config.rs::ViewOptions`, `tui.rs::settings_items` + matching `apply_setting_action`, `tui.rs::filter_rows` if it gates row visibility, hash it into `compute_cache_key` |
| Add a new render mode | `cli.rs::Mode`, `display/<mode>.rs`, `main.rs` plain dispatch, `tui.rs::build_rows`, mode pills in `tui.rs::draw_title`, treemap-style cache hooks if applicable |
| Adjust bars layout | `display/bars.rs`: `name_w`, `size_w`, `bar_w` constants near top of `render` |
| Tweak Spotlight UI | `tui.rs::draw_spotlight` |
| Tweak Spotlight behavior | `tui.rs::handle_spotlight_key`, `spotlight_complete`, `spotlight_refresh` |
| Add a status-bar pill | `tui.rs::draw_status` (note: row 2 has the `Action::Noop` click region for `depth: N`) |
| Suppress a row from selection | Set `Row::is_hidden_summary = true` |
| Add an overlay | new `View::*` variant + `draw_*` fn, register in `draw()` overlay match, update key dispatch priority list in §5, add `Esc → close` handler |
| Add an editor prompt | `EditorPrompt` enum in `editor.rs`, render branch in `tui.rs::draw_editor` prompt overlay, handler arm in `editor.rs::handle_prompt_key`. **Never recurse into `handle_key` from a prompt arm without first clearing the prompt** (see §6.4). |
| Invalidate the row cache | call `app.invalidate_caches()` after the state change. If your change is captured by a Config field already hashed in `compute_cache_key`, you don't need to do anything. |
| Add a long-running task | spawn a thread + mpsc channel, drain at the top of `run_loop` before `terminal.draw`. Don't block on `recv`. Route through the same-path dedup pattern from `start_background_scan` if relevant. |

---

## 9. Smoke tests

The interactive TUI cannot be driven from a non-TTY shell. To
exercise plain-mode output:

```bash
cargo run --quiet -- ~ -d 1 --no-color 2>/dev/null | head -20
cargo run --quiet -- /var/log -m bars --no-color 2>/dev/null | head -20
```

For interactive testing the user runs `diskvis` in a real terminal
and exercises the keymap by hand. There are no automated tests.

Spot-check checklist before any large change:

- `cargo build` clean, zero warnings.
- `RUSTFLAGS="-D warnings" cargo build` clean.
- TUI starts, scans `~`, splash appears for large home dirs and
  vanishes when the scan completes.
- Press `r`: status flashes `scan: <path> (render depth N)`. No
  blanking, no stack overflow, no duplicate worker threads.
- Press `e` on a file: editor opens, cursor visible as a bright-yellow
  block on the current row. Type, save with Ctrl+S (flash "saved …"),
  Ctrl+W → Save & Close → returns to main view, no crash.
- Treemap mode: resize the terminal — re-renders. Navigate with
  arrows — instantaneous.
- Spotlight (`Space`): type `/etc/`, see entries, Tab completes,
  Enter navigates.
- Settings (`S`): toggle `show_files` — tree view updates without a
  rescan.
- `+` / `-` adjust depth in the status bar.

---

## 10. Open items / follow-ups

- The treemap mode does not display a Hidden Items entry; only
  list-based modes do. If wanted, would need a hidden-aware
  reservation in the binary-split layout — non-trivial.
- The async scan worker uses a single channel; if a user navigates
  faster than the scan completes, the in-flight result is **kept**
  (pushed into `scan_cache`) but **not swapped in** (since
  `self.root.path` no longer matches). This is by design — the
  freshly-navigated path's own scan will swap. But if the user goes
  back to the original root within 30 s, the cached result is used
  immediately. Watch for cache staleness if the filesystem changes
  underneath us.
- `compute_cache_key()` hashes `last_size.0` and `last_size.1`. Every
  terminal resize invalidates the row cache. That's correct (bars
  layout depends on width) but a window-drag burst re-renders every
  step. Could debounce if it becomes a bottleneck.
- The treemap cache is per-buffer cell-clone on blit. If this shows
  up in a profile, a single `*dst = entry.buf.clone()` would work
  but only if the dst Buffer's area exactly matches — currently we
  blit at an offset (area.x, area.y) inside a larger frame buffer.
- `setpriority` calls are best-effort and silently ignored on systems
  without CAP_SYS_NICE. No fallback / no warning. Fine.
- No release-grade error reporting on the scan thread beyond the
  status flash. If it panics, the channel disconnects and
  `drain_scan_results` quietly clears `rescanning`. We don't surface
  the panic to the user.
- The release pipeline (`./release.sh <version>`) tags + builds +
  publishes via `cargo publish --allow-dirty`. The `--allow-dirty`
  flag is intentional because the TUI writes config on exit;
  attempting a clean publish from a freshly-run repo would always
  fail.
- Spotlight has never been driven interactively in agent sessions
  (no TTY). It compiles and the architecture is sound, but UX edge
  cases (very small terminals < 12 rows, exotic keyboards) are
  unverified.

---

End of handoff.
