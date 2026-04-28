pub mod bars;
pub mod flat;
pub mod tree;
pub mod treemap;

use chrono::{DateTime, Local};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::Theme;
use crate::walker::Node;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;
const TB: u64 = 1024 * GB;

pub fn human_size(bytes: u64) -> String {
    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

pub fn fmt_modified(t: Option<std::time::SystemTime>) -> String {
    match t {
        Some(st) => {
            let dt: DateTime<Local> = st.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "-".to_string(),
    }
}

/// Style helpers parameterized on theme.
pub fn size_style(theme: Theme, bytes: u64) -> Style {
    match theme {
        Theme::Default => {
            if bytes < MB {
                Style::default().fg(Color::Green)
            } else if bytes <= 100 * MB {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Red)
            }
        }
        Theme::HighContrast => {
            if bytes < MB {
                Style::default().fg(Color::DarkGray)
            } else if bytes <= 100 * MB {
                Style::default().fg(Color::White)
            } else {
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD)
            }
        }
        Theme::Monochrome => Style::default(),
    }
}

pub fn dir_style(theme: Theme) -> Style {
    match theme {
        Theme::Default => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
        Theme::HighContrast => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        Theme::Monochrome => Style::default().add_modifier(Modifier::BOLD),
    }
}

pub fn file_style(theme: Theme) -> Style {
    match theme {
        Theme::Default => Style::default(),
        Theme::HighContrast => Style::default().fg(Color::Gray),
        Theme::Monochrome => Style::default(),
    }
}

pub fn dim_style(theme: Theme) -> Style {
    match theme {
        Theme::Monochrome => Style::default(),
        _ => Style::default().add_modifier(Modifier::DIM),
    }
}

#[derive(Clone, Copy)]
pub struct RenderOptions {
    pub theme: Theme,
    pub depth: usize,
    pub min_size: u64,
    pub sort: crate::cli::SortBy,
    pub sort_order: crate::cli::SortOrder,
    pub show_files: bool,
    pub show_modified: bool,
    pub width: u16,
    pub height: u16,
    /// When `Some((count, total_size))` and count > 0, list-based renderers
    /// should hide entries whose name begins with `.` and append a single
    /// synthetic summary row at the bottom of the listing. The percentage is
    /// computed against the parent (root) directory size by the renderer.
    pub hidden_summary: Option<(u64, u64)>,
}

/// Compute the (count, total_size) tuple of hidden direct children of a
/// directory (names starting with `.`). Returns `None` when `show_hidden` is
/// true or when there are no hidden children.
pub fn compute_hidden_summary(node: &Node, show_hidden: bool) -> Option<(u64, u64)> {
    if show_hidden {
        return None;
    }
    let mut count: u64 = 0;
    let mut size: u64 = 0;
    for c in &node.children {
        if c.name.starts_with('.') {
            count += 1;
            size = size.saturating_add(c.size);
        }
    }
    if count == 0 {
        None
    } else {
        Some((count, size))
    }
}

/// Construct a synthetic, dim/italic, non-selectable summary row used by all
/// list-based renderers when hidden entries are filtered.
pub fn hidden_summary_row(count: u64, size: u64, parent_size: u64) -> Row {
    let pct = if parent_size > 0 {
        (size as f64 / parent_size as f64) * 100.0
    } else {
        0.0
    };
    let plural = if count == 1 { "item" } else { "items" };
    let text = format!(
        "··· {} hidden {}  ({}  |  {:.0}%)",
        count,
        plural,
        human_size(size),
        pct
    );
    let style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
    Row {
        line: Line::from(Span::styled(text, style)),
        path: None,
        is_dir: false,
        size: 0,
        name: String::new(),
        is_hidden_summary: true,
    }
}

pub fn sort_nodes<'a>(nodes: &mut Vec<&'a Node>, sort: crate::cli::SortBy, order: crate::cli::SortOrder) {
    use crate::cli::{SortBy, SortOrder};
    match (sort, order) {
        (SortBy::Size, SortOrder::Desc) => nodes.sort_by(|a, b| b.size.cmp(&a.size)),
        (SortBy::Size, SortOrder::Asc) => nodes.sort_by(|a, b| a.size.cmp(&b.size)),
        (SortBy::Name, SortOrder::Asc) => {
            nodes.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }
        (SortBy::Name, SortOrder::Desc) => {
            nodes.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()))
        }
    }
}

/// A flat row produced by a renderer with metadata for TUI navigation.
pub struct Row {
    pub line: Line<'static>,
    /// Path of the entry this row represents (for navigation), if any.
    pub path: Option<std::path::PathBuf>,
    pub is_dir: bool,
    pub size: u64,
    pub name: String,
    /// True for the synthetic `··· N hidden items` summary row appended when
    /// hidden entries are filtered out. Such rows are non-selectable in the
    /// TUI and ignored by mouse hit-testing.
    pub is_hidden_summary: bool,
}

impl Row {
    #[allow(dead_code)]
    pub fn plain(line: Line<'static>) -> Self {
        Row {
            line,
            path: None,
            is_dir: false,
            size: 0,
            name: String::new(),
            is_hidden_summary: false,
        }
    }
}

/// Print all rows as plain text using ANSI escape codes derived from spans.
pub fn print_rows(rows: &[Row], use_color: bool) {
    for row in rows {
        let mut buf = String::new();
        for span in &row.line.spans {
            if use_color {
                buf.push_str(&style_to_ansi(span.style));
                buf.push_str(&span.content);
                buf.push_str("\x1b[0m");
            } else {
                buf.push_str(&span.content);
            }
        }
        println!("{}", buf);
    }
}

fn style_to_ansi(style: Style) -> String {
    let mut s = String::new();
    if style.add_modifier.contains(Modifier::BOLD) {
        s.push_str("\x1b[1m");
    }
    if style.add_modifier.contains(Modifier::DIM) {
        s.push_str("\x1b[2m");
    }
    if let Some(c) = style.fg {
        let code = match c {
            Color::Black => "30",
            Color::Red => "31",
            Color::Green => "32",
            Color::Yellow => "33",
            Color::Blue => "34",
            Color::Magenta => "35",
            Color::Cyan => "36",
            Color::Gray => "37",
            Color::DarkGray => "90",
            Color::LightRed => "91",
            Color::LightGreen => "92",
            Color::LightYellow => "93",
            Color::LightBlue => "94",
            Color::LightMagenta => "95",
            Color::LightCyan => "96",
            Color::White => "97",
            _ => "39",
        };
        s.push_str("\x1b[");
        s.push_str(code);
        s.push('m');
    }
    s
}

pub fn name_span(node: &Node, theme: Theme) -> Span<'static> {
    let style = if node.has_unusual_perms() {
        // Orange overlay for SUID / SGID / world-writable entries.
        Style::default()
            .fg(Color::Rgb(255, 140, 0))
            .add_modifier(Modifier::BOLD)
    } else if node.is_dir {
        dir_style(theme)
    } else {
        file_style(theme)
    };
    Span::styled(node.name.clone(), style)
}

pub fn size_span(node: &Node, theme: Theme) -> Span<'static> {
    Span::styled(human_size(node.size), size_style(theme, node.size))
}

pub fn modified_span(node: &Node, theme: Theme) -> Span<'static> {
    Span::styled(fmt_modified(node.modified), dim_style(theme))
}
