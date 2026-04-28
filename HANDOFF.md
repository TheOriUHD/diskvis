# diskvis — Session Handoff

This document is a self-contained context dump for another model picking up
work on `diskvis`. It captures the architecture, conventions, what was built
this session, the bugs we hit, and the open follow-ups.

---

## 1. Project at a glance

`diskvis` is a Rust 1.95 stable terminal disk-usage visualizer. It has two
modes:

- **Plain CLI** — pipes ANSI-styled rows to stdout when `--no-color`,
  `--no-tty`, or piped output is detected.
- **Interactive TUI** — ratatui + crossterm with mouse, scrollbars, mode
  pills, filter input, settings overlay, warnings overlay, and (new this
  session) a Spotlight path-jumper popup.

### Build constraint

The on-disk `target/` dir at `/home/amp/diskvis/target` is **root-owned** and
sudo is unavailable. **Always** build with:

```bash
CARGO_TARGET_DIR=/tmp/diskvis-target cargo build
```

The same env var must be used for `cargo run`, `cargo check`, etc. Builds
must finish with **zero warnings**.

### Cargo deps

clap 4.5 (derive), walkdir 2.5, colored 2.1, ratatui 0.29, crossterm 0.28,
indicatif 0.17, serde 1, serde_json 1, chrono 0.4, human-panic 2, toml 0.8.

---

## 2. Source map

```
src/
├── cli.rs                 — clap CLI, SortBy/SortOrder/Mode enums.
├── config.rs              — Config (TOML at ~/.config/diskvis/config.toml),
│                            ViewOptions (show_hidden / show_empty /
│                            show_percent / show_modified), Theme, default
│                            excludes including /-prefixed vfs paths.
├── walker.rs              — walkdir-based traversal returning a Node tree
│                            and ScanResult { root, warnings }. Buffered
│                            warnings only — no eprintln during TUI.
├── main.rs                — entry point: arg parse → walker → TUI or plain
│                            text rendering.
├── display/
│   ├── mod.rs             — RenderOptions, Row, Theme styles, human_size,
│                            fmt_modified, name_span / size_span /
│                            modified_span helpers, hidden_summary helpers.
│   ├── tree.rs            — Indented ├──/└── tree.
│   ├── bars.rs            — Horizontal bar histogram (the focus of the
│                            second half of the session).
│   ├── flat.rs            — Flat sorted-files list.
│   └── treemap.rs         — Binary-split squarified treemap drawn into a
│                            ratatui Buffer.
└── tui.rs                 — ~1500 lines: App state, event loop, key/mouse
                             handling, drawing of all panels and overlays,
                             Spotlight worker thread, Spotlight UI.
```

### Key types

```rust
// display/mod.rs
pub struct Row {
    pub line: Line<'static>,
    pub path: Option<PathBuf>,
    pub is_dir: bool,
    pub size: u64,
    pub name: String,
    pub is_hidden_summary: bool,   // NEW: synthetic Hidden Items row marker
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
    pub hidden_summary: Option<(u64 /*count*/, u64 /*size*/)>,  // NEW
}

// walker.rs
pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified: Option<SystemTime>,
    pub children: Vec<Node>,
}
```

### Convention: a Row produced by a renderer is what `display::print_rows`
emits in plain mode and what `tui::draw_body` walks in interactive mode.
`draw_body` performs:

1. Filtering (by `view_options`, by current filter query).
2. Cursor clamping with `App::snap_cursor_to_selectable` so the cursor
   never lands on a `is_hidden_summary` row.
3. Row-by-row span construction:
   - Prepend `▶ ` selection arrow or `  ` padding.
   - Optionally dim hidden / zero entries.
   - Append spans from the renderer's `Row.line`.
   - **If `show_percent && !r.name.is_empty()`**, append a trailing
     `Span::raw("  ")` + `Span::styled("{:>5.1}%", Color::Magenta)`.
   - Apply selected-row background color.

This last bullet is critical: **draw_body decides whether to append the
percent column based on `Row.name` being non-empty**. We rely on this to
suppress the trailing percent for the synthetic Hidden Items row.

---

## 3. What was completed this session

### 3.1 Earlier rounds (already on disk before this session window)

- TUI rewrite to ratatui 0.29 with mouse capture, mode pills, filter pill,
  scrollbar, multiple overlays, persistent config.
- `ViewOptions` added with `show_hidden / show_empty / show_percent /
  show_modified` toggles, defaulting to `(true, false, true, true)`.
  `Config` migrates older configs forward via `#[serde(default)]`.
- Treemap rewrite: squarify → binary-split, drawing directly into ratatui
  `Buffer` via `cell_mut`/`set_symbol`/`set_style`. Plain CLI compatibility
  achieved by `treemap::render(...)` allocating a temp `Buffer::empty(area)`
  and RLE-encoding each row back into spans.

### 3.2 Spotlight path-jumper

A LazyVim-style command palette popup for navigating to any path.

- **Trigger:** `Space` or `Ctrl+P` on the main view.
- **UI:** centered popup, ~60% screen width, fixed height 12, positioned in
  upper third (`area.y + area.height/6`), rounded cyan border, title
  ` Spotlight `. Inner layout: input row → divider → up to 8 result rows →
  dim hint footer. Selected row highlighted with full-width `Color::Rgb(40,
  40, 70)` background and `▸ ` arrow. Dirs blue/bold + trailing `/`,
  files white.
- **Async autocomplete:** worker thread spawned lazily by
  `App::ensure_spotlight_worker`. mpsc `Sender<SpotlightReq>` /
  `Receiver<SpotlightResp>` with **coalescing recv** (drains queued
  requests and only services the latest before reading the directory) and
  **generation counters** so stale results are discarded.
- **Debounce:** 50 ms via `last_input_change` timestamp. Forced refresh on
  `/` typed (expand into directory immediately), Tab, Backspace.
- **Cache:** `SpotlightState::cache: Option<(PathBuf, Vec<Entry>)>`,
  keyed by parent dir, so backspacing within the same parent is instant.
- **Keys:**
  - `Esc` close
  - `Enter` navigate (selected entry, or typed path if it's a valid dir)
    via existing `App::nav_stack.push() + rescan_at()`
  - `↑/↓` move selection
  - `Tab` / `→` complete (single-accept / longest-common-prefix /
    fallback to highlighted)
  - `Backspace` remove last char (force refresh)
  - `Ctrl-W` drop trailing path component
  - `Ctrl-U` clear
  - any printable char appends; `/` triggers immediate refresh
- **Run loop integration:** when spotlight is open, poll interval drops to
  30 ms (vs 200 ms otherwise). Each tick: `drain_spotlight_responses()`
  then `spotlight_refresh(false)` (debounce-aware).
- **Help overlay** updated with `Space / Ctrl-P  open path spotlight`.

Key types added:

```rust
pub struct SpotlightEntry { path: PathBuf, name: String, is_dir: bool, size_hint: Option<u64> }
pub struct SpotlightState {
    input: String,
    candidates: Vec<SpotlightEntry>,
    selected: usize,
    last_input_change: Instant,
    pending_refresh: bool,
    current_generation: u64,
    cache: Option<(PathBuf, Vec<SpotlightEntry>)>,
}
struct SpotlightReq { generation: u64, parent: PathBuf, partial: String }
struct SpotlightResp { generation: u64, parent: PathBuf, entries: Vec<SpotlightEntry> }
```

`App` gained: `spotlight: Option<SpotlightState>`, `spot_tx`, `spot_rx`.

Helpers (free fns at file scope of `tui.rs`):
- `split_input(input)` → `(parent: PathBuf, partial: String)`. Handles `~`
  expansion, trailing `/` (treat as "list children of parent"), default to
  `/` when empty.
- `expand_tilde`, `list_dir_entries` (dirs first then alpha),
  `filter_entries` (case-insensitive `starts_with`, take 64),
  `longest_common_prefix` (case-insensitive byte-wise).

### 3.3 Hidden-items summary row

Append a single synthetic summary row when `view.show_hidden` is `false`.

- Added `display::compute_hidden_summary(&Node, show_hidden) ->
  Option<(count, size)>` — sums children whose name starts with `.`.
- Added `display::hidden_summary_row(count, size, parent_size) -> Row` —
  the **default annotation form** used by `tree.rs` and `flat.rs`.
- Each list-based renderer:
  - Filters dot-prefixed direct children when `opts.hidden_summary`
    is `Some(_)`.
  - Appends the synthetic row at the bottom (in tree/flat) or sorts it
    inline by size (in bars — see §3.4).
- `flat.rs` additionally drops files whose first relative path component
  begins with `.` (so files under `~/.config/...` don't slip through).
- `treemap.rs` updated only to keep its single Row literal compiling
  (added `is_hidden_summary: false`); it does not display a summary row.

Plain main.rs and TUI render_opts both call `compute_hidden_summary`
before invoking renderers.

### 3.4 Bars.rs Hidden Items row — the saga

The user iterated several times on how this should look. Final state:

> **The synthetic Hidden Items row renders through the exact same code
> path as a real entry**, by building a stack-local synthetic `Node`
> with `name = "Hidden Items  X.X%"`, `size = combined`, `is_dir = false`,
> `path = PathBuf::new()`, `modified = None`. The shared loop body then
> emits identical span structure (name span padded to `name_w` →
> `Span::raw(" ")` → size span → `Span::raw(" ")` → bar full → bar rest).
>
> The modified column is **explicitly skipped** for the synthetic row
> (no `-` placeholder, no separator). Real entries still render their
> modified date.
>
> The Row's `name` field is set to `String::new()` for the synthetic row
> so `draw_body` skips its trailing percent column (the percent is
> already baked into the displayed name).
>
> The row keeps `is_hidden_summary: true` and `path: None`, so:
> - Cursor navigation in the TUI skips it.
> - Mouse clicks ignore it.
> - It cannot be drilled into, excluded, or yanked.
> - Scrollbar math counts it (it's still a row in the visible list).

The synthetic row also participates in `sort_entries` so it can land
between real entries — e.g. when sorted by size descending it slots
above smaller real entries.

A small `Entry<'a>` enum (`Real(&'a Node) | Hidden { size: u64 }`) and a
local `sort_entries` exist solely for sorting the mixed set; once the
loop runs, both arms unify on a `&Node`.

### 3.5 TUI navigation hardening for non-selectable rows

Because the Hidden Items row in bars mode can sort *anywhere* in the
list, the TUI navigation needed to skip it positionally rather than
just trim from the end. Added `App` methods:

- `selectable_len(&[Row]) -> usize` — count of non-summary rows.
- `last_selectable_idx(&[Row]) -> usize` — index of last selectable row.
- `step_forward(&[Row], from, steps) -> usize` — advance N selectable
  rows, skipping summary rows.
- `step_backward(&[Row], from, steps) -> usize` — same in reverse.
- `snap_cursor_to_selectable(&[Row], cursor) -> usize` — if cursor lands
  on a summary row, snap forward (preferred) or backward.

All cursor-moving keybinds (`j k Down Up Home End g G PageUp PageDown`),
mouse `ScrollUp` / `ScrollDown`, and the body-area click hit-test now
use these helpers. `draw_body` snaps the cursor as a final guard before
rendering each frame. `selected_row_path()` returns `None` if the row is
a summary row (defensive).

---

## 4. Bugs encountered and fixes

### 4.1 Bars Hidden Items row wrapping onto a second line

**Symptoms** (sequential, after each attempted fix):
1. Initially the synthetic row used a `··· N hidden items` annotation
   pinned to bottom — fine but inconsistent with the user's request
   for a real bar entry.
2. After making it a real bar entry, the TUI wrapped the row across
   two visual lines: the percent split off below.
3. Removing the dim/italic styling and using `█` fill didn't help.
4. Padding the modified placeholder via `format!("{:<16}", "-")`
   introduced 15 internal spaces — `Paragraph::wrap` happily breaks
   on whitespace inside a single Span.
5. Replacing internal whitespace with NBSP (U+00A0) in a single
   pre-built content string also failed in some terminals due to
   ratatui's wrap behavior.

**Root cause:** `tui::draw_body` builds the body with `Paragraph::new(
lines).wrap(Wrap { trim: false })`. When a line's total visual width
exceeds `content_w`, ratatui's wrapper breaks at whitespace.
The trailing percent column was the longest span and broke first.

**Final fix:**
- Build the synthetic node's `name` as `"Hidden Items  X.X%"` (the
  percent is part of the name).
- Set the **emitted Row's** `name` to `String::new()` so `draw_body`
  does **not** append its own trailing percent span.
- Skip the modified column entirely for the synthetic row
  (`if opts.show_modified && !is_synthetic { ... }`).

### 4.2 ViewOptions defaults inverted

Earlier round — fixed by setting `show_hidden=true, show_empty=false,
show_percent=true, show_modified=true` in `ViewOptions::default()`.

### 4.3 `target/` dir root-owned

The user has `sudo` unavailable on this box. Always export
`CARGO_TARGET_DIR=/tmp/diskvis-target` for any cargo command.

### 4.4 `rg` may not be installed

Use `grep` fallbacks for log/file scans on this host.

---

## 5. Conventions to preserve

1. **Rows produced by a renderer are the source of truth** for both
   plain mode and the TUI. Keep them consistent: any new row type or
   styling must compile through `display::print_rows` and
   `tui::draw_body` unchanged.
2. **`is_hidden_summary` is the single marker** for non-selectable
   synthetic rows. Don't add ad-hoc sentinel checks elsewhere; extend
   `selectable_len` etc. if needed.
3. **`Row.name == ""` means "do not append percent"** in `draw_body`.
   This is load-bearing for the bars synthetic row.
4. **TUI must never `eprintln!`** during runtime — it's in alternate
   screen + raw mode. Buffer warnings via `walker::ScanResult.warnings`
   and surface them through the warnings overlay.
5. **Builds must be warning-free.** The user's standard close to every
   request is: *"After changes run cargo build and fix all errors until
   clean with zero warnings."*
6. **Don't refactor unrequested code.** The user prefers minimal,
   targeted edits.

---

## 6. Plain-mode reference output

Bars mode with `show_hidden=false, show_modified=true, show_percent=true`,
running `cargo run -- ~ -d 1 --no-color`:

```
/home/amp  100.38 GB
Hidden Items  100.0%        100.37 GB ████████████████████████████████████████
kantidev-plugin               4.98 MB ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  2026-04-27 17:22
ReflectionTool                4.74 MB ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  2026-04-17 11:02
...
```

Key observations:
- Hidden Items has no modified column.
- Hidden Items name column contains the baked-in percent.
- All other rows have name (24) → size (12) → bar (rest) → modified.

---

## 7. Useful smoke-test snippet

The interactive TUI cannot be driven from a non-TTY shell. To test
plain-mode output with a temporarily flipped `show_hidden`:

```bash
cp ~/.config/diskvis/config.toml /tmp/diskvis-config-backup.toml
python3 - <<'PY'
import re,pathlib
p=pathlib.Path.home()/".config/diskvis/config.toml"
t=p.read_text()
t=re.sub(r'mode\s*=\s*"\w+"','mode = "bars"',t)
t=re.sub(r'show_hidden\s*=\s*true','show_hidden = false',t)
p.write_text(t)
PY
CARGO_TARGET_DIR=/tmp/diskvis-target cargo run --quiet -- ~ -d 1 --no-color 2>/dev/null | head -5
cp /tmp/diskvis-config-backup.toml ~/.config/diskvis/config.toml
```

---

## 8. Open items / not done

- The Spotlight feature has **never been driven interactively** in this
  session (no TTY). It compiles cleanly and the architecture is sound,
  but there could be UX edge cases (e.g. arrow-key handling on non-xterm
  terminals, very small terminal sizes < 12 rows).
- Settings overlay currently exposes `show_modified / show_hidden /
  show_empty / show_percent` toggles. Adding new view toggles requires
  updating both `settings_items()` and the toggle handlers.
- No automated tests exist. The user has not requested any.
- The `treemap` mode does not display a Hidden Items entry; only
  list-based modes do. If that's wanted, it'd require a hidden-aware
  reservation in the binary-split layout, which is non-trivial.

---

## 9. Quick-reference: where to make common changes

| Want to… | Edit |
|---|---|
| Add a new view toggle | `config.rs::ViewOptions`, `tui.rs::settings_items` and matching `apply_setting_action`, `tui.rs::filter_rows` if it gates row visibility |
| Add a new render mode | `cli.rs::Mode`, `display/<mode>.rs`, `main.rs` plain dispatch, `tui.rs::build_rows`, mode pills in `tui.rs::draw_title` |
| Adjust bars layout | `display/bars.rs`: `name_w`, `size_w`, `bar_w` constants near top of `render` |
| Tweak Spotlight UI | `tui.rs::draw_spotlight` |
| Tweak Spotlight behavior | `tui.rs::handle_spotlight_key`, `spotlight_complete`, `spotlight_refresh` |
| Add a status-bar pill | `tui.rs::draw_status` |
| Suppress a row from selection | Set `Row::is_hidden_summary = true` |

---

End of handoff.
