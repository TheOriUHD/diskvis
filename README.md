# diskvis

A premium interactive terminal disk usage visualizer.

`diskvis` scans a directory tree and presents disk usage in your terminal —
either as styled output piped to stdout, or as a fully interactive TUI with
mouse support, multiple visualization modes, fuzzy path navigation, and
persistent settings.

## Features

- **Four visualization modes**
  - **Tree** — classic indented `├──/└──` hierarchy
  - **Bars** — horizontal-bar histogram with size and modified columns
  - **Treemap** — squarified binary-split treemap rendered in the terminal
  - **Flat** — sorted flat list of largest files
- **Interactive TUI** built on ratatui + crossterm, with:
  - Mouse support — click to select, double-click to drill in, scroll wheel
  - Scrollbar, mode pills, sort indicator, filter pill, status bar
  - Settings overlay, warnings overlay, help overlay
  - Live filter bar (`/`) with case-insensitive substring matching
- **Spotlight path search** — LazyVim-style popup (`Space` or `Ctrl+P`) for
  jumping to any directory with async autocomplete, longest-common-prefix
  completion, debounced filesystem reads, and per-directory caching
- **Hidden files toggle** — collapse dotfiles into a single synthetic
  "Hidden Items" summary row, or show them inline (`.` / `h`)
- **Themes** — cycle built-in color themes with `c`
- **View toggles** for files, empty dirs, percentage column, and modified
  date column — all persisted across runs
- **Config persistence** at `~/.config/diskvis/config.toml`
- **JSON export** — emit the full scanned tree to stdout with `--json`
- **Pipe-aware** — automatically falls back to plain styled output when
  stdout is not a TTY or `--no-color` is passed
- **Glob excludes** via `-e/--exclude` (repeatable) plus a sensible default
  exclude list for virtual filesystems
- **Buffered scan warnings** surfaced through an in-TUI overlay (no stderr
  spam during interactive use)

## Installation

### From crates.io

```bash
cargo install diskvis
```

### Pre-built binary

Download the latest release for your platform from the
[GitHub releases page](https://github.com/theoriuhd/diskvis/releases),
extract it, and place the `diskvis` binary somewhere on your `PATH`.

### From source

```bash
git clone https://github.com/theoriuhd/diskvis
cd diskvis
cargo build --release
# binary will be at target/release/diskvis
```

Requires Rust 1.95 or later.

## Usage

```bash
# Scan the current directory in interactive TUI mode
diskvis

# Scan your home directory limited to depth 2
diskvis ~ -d 2

# Plain bars output, no colors, with modified dates
diskvis /var/log -M bars --no-color --modified

# Show individual files (not just directories) in tree mode
diskvis . -M tree -f

# Hide entries smaller than 10 MiB
diskvis / -m 10485760

# Exclude directories by glob (repeatable)
diskvis ~ -e 'node_modules' -e '*.cache'

# Sort by name ascending
diskvis . -s name

# Dump the full scanned tree as JSON
diskvis ~/projects --json > tree.json
```

### Flags

| Flag | Description |
|---|---|
| `<PATH>` | Directory to analyze (default: `.`) |
| `-d`, `--depth <N>` | Maximum tree depth to display |
| `-m`, `--min-size <BYTES>` | Hide entries below N bytes |
| `-s`, `--sort <size\|name>` | Sort order |
| `-M`, `--mode <tree\|bars\|treemap\|flat>` | Visualization mode |
| `-e`, `--exclude <GLOB>` | Glob patterns to exclude (repeatable) |
| `-f`, `--show-files` | Show individual files in tree mode |
| `--modified` | Show last modified date next to each entry |
| `--json` | Output the scanned tree as JSON to stdout |
| `--no-color` | Disable colors and force plain output |
| `-h`, `--help` | Print help |
| `-V`, `--version` | Print version |

## Keybinds

All keybinds apply in the interactive TUI.

### Navigation

| Key | Action |
|---|---|
| `j` / `k` / `↓` / `↑` | Move cursor one row |
| `PgDn` / `PgUp` | Move half a page |
| `g` / `Home` | Jump to top |
| `G` / `End` | Jump to bottom |
| `Enter` | Drill into selected directory |
| `u` / `Backspace` | Go up to parent directory |
| `~` | Jump to home directory |
| `r` | Rescan current directory |
| `Space` / `Ctrl+P` | Open Spotlight path picker |

### Display

| Key | Action |
|---|---|
| `1` / `2` / `3` / `4` | Tree / Bars / Treemap / Flat |
| `s` | Cycle sort (size↓ → size↑ → name↑ → name↓) |
| `c` | Cycle theme |
| `f` | Toggle showing files |
| `.` / `h` | Toggle hidden files |
| `d` | Toggle empty directories |
| `p` | Toggle percentage column |
| `m` | Toggle modified-date column |

### Filter

| Key | Action |
|---|---|
| `/` | Open filter bar |
| `Esc` | Clear active filter |

### Actions

| Key | Action |
|---|---|
| `x` | Exclude selected entry for this session |
| `o` | Open in system file manager (`xdg-open`) |
| `y` | Yank path to clipboard |
| `e` | Open settings panel |
| `w` | Open warnings overlay |
| `?` | Show this help |
| `q` / `Ctrl+C` | Quit |

### Spotlight

| Key | Action |
|---|---|
| `Tab` / `→` | Complete (single-accept / longest-common-prefix) |
| `↑` / `↓` | Move selection |
| `Enter` | Navigate to highlighted (or typed) path |
| `Backspace` | Remove last character |
| `Ctrl+W` | Drop trailing path component |
| `Ctrl+U` | Clear input |
| `Esc` | Close Spotlight |

### Mouse

- **Click** a row to select it
- **Double-click** a directory row to drill in
- **Scroll wheel** to navigate

## Configuration

`diskvis` persists user preferences to `~/.config/diskvis/config.toml`. The
file is created automatically on first run and updated whenever you change
the mode, sort order, theme, or any view toggle from inside the TUI.

Example:

```toml
mode = "bars"
sort = "size"
sort_order = "desc"
theme = "default"

[view_options]
show_hidden = true
show_empty = false
show_percent = true
show_modified = true
```

You can edit the file by hand; unknown or missing fields fall back to
defaults so older configs continue to work after upgrades.

The default exclude list covers common virtual filesystems
(`/proc`, `/sys`, `/dev`, etc.) so scanning `/` does not wander into
kernel-managed paths.

## Screenshots

<!-- Add screenshots / asciinema recordings here. -->

_Coming soon._

## License

MIT — see [LICENSE](LICENSE).
