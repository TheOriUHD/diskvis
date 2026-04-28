//! Binary-split treemap renderer.
//!
//! Algorithm: take a list of nodes sorted by size descending. Split the
//! current rect into two halves by the longer axis (vertical split when wider
//! than tall, horizontal split when taller than wide). The largest single
//! node — or the largest prefix of the list whose summed size is ≥ 50 % of
//! the remaining total — goes in one half (proportional to its share); the
//! rest recurses into the other half. Stop when a rect drops below the
//! minimum renderable size (3×2).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::display::{human_size, sort_nodes, RenderOptions, Row};
use crate::walker::Node;

const KB: u64 = 1024;
const MB: u64 = 1024 * KB;

const MIN_W: u16 = 3;
const MIN_H: u16 = 2;
const TEXT_W: u16 = 6;
const TEXT_H: u16 = 3;

// ---------------------------------------------------------------------------
// Public TUI entry point — draws directly into a ratatui Buffer.
// ---------------------------------------------------------------------------

pub fn render_treemap(node: &Node, opts: &RenderOptions, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Header row at top.
    let header_y = area.y;
    let mut x = area.x;
    let path = node.path.display().to_string();
    write_str(buf, x, header_y, &path, Style::default().add_modifier(Modifier::BOLD), area);
    x = x.saturating_add(path.chars().count() as u16 + 2);
    let size_s = human_size(node.size);
    write_str(
        buf,
        x,
        header_y,
        &size_s,
        color_for(node.size, node.is_dir),
        area,
    );

    // The treemap occupies area minus the header line.
    let body = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    if body.width < MIN_W || body.height < MIN_H {
        return;
    }

    if !node.is_dir || node.children.is_empty() {
        return;
    }

    // Collect & sort visible children desc by size.
    let mut visible: Vec<&Node> = node
        .children
        .iter()
        .filter(|c| c.size >= opts.min_size && c.size > 0)
        .filter(|c| opts.show_files || c.is_dir)
        .collect();
    if visible.is_empty() {
        return;
    }
    sort_nodes(&mut visible, opts.sort, opts.sort_order);
    // Always force size-desc ordering for the layout step regardless of caller's sort.
    visible.sort_by(|a, b| b.size.cmp(&a.size));

    layout(&visible, body, 0, opts, buf);
}

// ---------------------------------------------------------------------------
// Plain-CLI compatibility: render into a temporary Buffer and convert each
// row to a styled `Row`.
// ---------------------------------------------------------------------------

pub fn render(root: &Node, opts: &RenderOptions) -> Vec<Row> {
    let w = opts.width.max(20);
    let h = opts.height.max(8);
    let area = Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let mut buf = Buffer::empty(area);

    render_treemap(root, opts, area, &mut buf);

    let mut rows: Vec<Row> = Vec::with_capacity(h as usize);
    for y in 0..h {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut buf_str = String::new();
        let mut cur_style: Option<Style> = None;
        for x in 0..w {
            let cell = buf.cell((x, y)).expect("cell in bounds");
            let style = cell_style(cell);
            let sym = cell.symbol().to_string();
            match cur_style {
                Some(s) if styles_eq(s, style) => buf_str.push_str(&sym),
                Some(s) => {
                    spans.push(Span::styled(std::mem::take(&mut buf_str), s));
                    buf_str.push_str(&sym);
                    cur_style = Some(style);
                }
                None => {
                    buf_str.push_str(&sym);
                    cur_style = Some(style);
                }
            }
        }
        if let Some(s) = cur_style {
            spans.push(Span::styled(buf_str, s));
        }
        rows.push(Row {
            size: 0,
            name: String::new(),
            line: Line::from(spans),
            path: None,
            is_dir: false,
            is_hidden_summary: false,
        });
    }
    rows
}

fn cell_style(cell: &ratatui::buffer::Cell) -> Style {
    let mut s = Style::default();
    s.fg = Some(cell.fg);
    s.bg = Some(cell.bg);
    s.add_modifier = cell.modifier;
    s
}

fn styles_eq(a: Style, b: Style) -> bool {
    a.fg == b.fg && a.bg == b.bg && a.add_modifier == b.add_modifier
}

// ---------------------------------------------------------------------------
// Binary-split layout
// ---------------------------------------------------------------------------

fn layout(items: &[&Node], rect: Rect, depth: usize, opts: &RenderOptions, buf: &mut Buffer) {
    if rect.width < MIN_W || rect.height < MIN_H || items.is_empty() {
        return;
    }

    if items.len() == 1 {
        draw_block(items[0], rect, buf);
        recurse_into(items[0], rect, depth, opts, buf);
        return;
    }

    // Decide split point: largest prefix whose summed size is >= 50% of total.
    let total: u64 = items.iter().map(|n| n.size).sum();
    if total == 0 {
        return;
    }
    let half = total / 2;
    let mut acc: u64 = 0;
    let mut split_at = 1usize;
    for (i, n) in items.iter().enumerate() {
        acc = acc.saturating_add(n.size);
        if acc >= half || i == items.len() - 2 {
            split_at = i + 1;
            break;
        }
    }
    if split_at == 0 {
        split_at = 1;
    }
    if split_at >= items.len() {
        split_at = items.len() - 1;
    }

    let left = &items[..split_at];
    let right = &items[split_at..];
    let left_size: u64 = left.iter().map(|n| n.size).sum();

    // Pick split orientation by longest dimension.
    let vertical = rect.width as u32 > rect.height as u32 * 2;

    let frac = (left_size as f64 / total as f64).clamp(0.05, 0.95);

    if vertical {
        // Split left/right.
        let left_w = ((rect.width as f64) * frac).round() as u16;
        let left_w = left_w.clamp(1, rect.width.saturating_sub(1));
        let right_w = rect.width - left_w;
        let left_rect = Rect {
            x: rect.x,
            y: rect.y,
            width: left_w,
            height: rect.height,
        };
        let right_rect = Rect {
            x: rect.x + left_w,
            y: rect.y,
            width: right_w,
            height: rect.height,
        };
        recurse_side(left, left_rect, depth, opts, buf);
        recurse_side(right, right_rect, depth, opts, buf);
    } else {
        // Split top/bottom.
        let top_h = ((rect.height as f64) * frac).round() as u16;
        let top_h = top_h.clamp(1, rect.height.saturating_sub(1));
        let bot_h = rect.height - top_h;
        let top_rect = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: top_h,
        };
        let bot_rect = Rect {
            x: rect.x,
            y: rect.y + top_h,
            width: rect.width,
            height: bot_h,
        };
        recurse_side(left, top_rect, depth, opts, buf);
        recurse_side(right, bot_rect, depth, opts, buf);
    }
}

fn recurse_side(items: &[&Node], rect: Rect, depth: usize, opts: &RenderOptions, buf: &mut Buffer) {
    if items.is_empty() || rect.width < MIN_W || rect.height < MIN_H {
        return;
    }
    if items.len() == 1 {
        draw_block(items[0], rect, buf);
        recurse_into(items[0], rect, depth, opts, buf);
    } else {
        layout(items, rect, depth, opts, buf);
    }
}

fn recurse_into(node: &Node, rect: Rect, depth: usize, opts: &RenderOptions, buf: &mut Buffer) {
    if !node.is_dir || node.children.is_empty() {
        return;
    }
    if depth + 1 >= opts.depth.max(1) {
        return;
    }
    // Inset by 1 (border) and reserve top 2 rows for the label.
    if rect.width < TEXT_W + 2 || rect.height < TEXT_H + 2 {
        return;
    }
    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 3,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(4),
    };
    if inner.width < MIN_W || inner.height < MIN_H {
        return;
    }

    let mut visible: Vec<&Node> = node
        .children
        .iter()
        .filter(|c| c.size >= opts.min_size && c.size > 0)
        .filter(|c| opts.show_files || c.is_dir)
        .collect();
    if visible.is_empty() {
        return;
    }
    visible.sort_by(|a, b| b.size.cmp(&a.size));
    layout(&visible, inner, depth + 1, opts, buf);
}

// ---------------------------------------------------------------------------
// Block rendering
// ---------------------------------------------------------------------------

fn draw_block(node: &Node, r: Rect, buf: &mut Buffer) {
    if r.width < MIN_W || r.height < MIN_H {
        return;
    }
    let style = color_for(node.size, node.is_dir);

    // Too small for borders + text → fill with shading.
    if r.width < TEXT_W || r.height < TEXT_H {
        let dim = Style::default()
            .fg(style.fg.unwrap_or(Color::Gray))
            .add_modifier(Modifier::DIM);
        for y in r.y..r.y + r.height {
            for x in r.x..r.x + r.width {
                set_cell(buf, x, y, "░", dim);
            }
        }
        return;
    }

    let x0 = r.x;
    let x1 = r.x + r.width - 1;
    let y0 = r.y;
    let y1 = r.y + r.height - 1;

    set_cell(buf, x0, y0, "┌", style);
    set_cell(buf, x1, y0, "┐", style);
    set_cell(buf, x0, y1, "└", style);
    set_cell(buf, x1, y1, "┘", style);
    for x in (x0 + 1)..x1 {
        set_cell(buf, x, y0, "─", style);
        set_cell(buf, x, y1, "─", style);
    }
    for y in (y0 + 1)..y1 {
        set_cell(buf, x0, y, "│", style);
        set_cell(buf, x1, y, "│", style);
    }
    // Clear interior.
    for y in (y0 + 1)..y1 {
        for x in (x0 + 1)..x1 {
            set_cell(buf, x, y, " ", Style::default());
        }
    }

    let inner_w = (r.width - 2) as usize;
    let inner_h = (r.height - 2) as usize;
    let inner_x = x0 + 1;
    let inner_y = y0 + 1;

    let name = truncate_to(&node.name, inner_w);
    let size_s = truncate_to(&human_size(node.size), inner_w);
    let name_style = if node.is_dir {
        style.add_modifier(Modifier::BOLD)
    } else {
        style.remove_modifier(Modifier::BOLD)
    };
    let size_style = Style::default()
        .fg(style.fg.unwrap_or(Color::Gray))
        .add_modifier(Modifier::DIM);

    if inner_h >= 2 {
        let top_pad = (inner_h - 2) / 2;
        let name_y = inner_y + top_pad as u16;
        let size_y = name_y + 1;
        let nx = inner_x + ((inner_w - name.chars().count()) / 2) as u16;
        let sx = inner_x + ((inner_w - size_s.chars().count()) / 2) as u16;
        write_str(buf, nx, name_y, &name, name_style, r);
        write_str(buf, sx, size_y, &size_s, size_style, r);
    } else if inner_h == 1 {
        let nx = inner_x + ((inner_w - name.chars().count()) / 2) as u16;
        write_str(buf, nx, inner_y, &name, name_style, r);
    }
}

// ---------------------------------------------------------------------------
// Buffer helpers
// ---------------------------------------------------------------------------

fn set_cell(buf: &mut Buffer, x: u16, y: u16, sym: &str, style: Style) {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(sym);
        cell.set_style(style);
    }
}

fn write_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style, clip: Rect) {
    let mut cx = x;
    let max_x = clip.x.saturating_add(clip.width);
    for ch in s.chars() {
        if cx >= max_x {
            break;
        }
        let mut tmp = [0u8; 4];
        let sym: &str = ch.encode_utf8(&mut tmp);
        set_cell(buf, cx, y, sym, style);
        cx = cx.saturating_add(1);
    }
}

fn truncate_to(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max <= 1 {
        return s.chars().take(max).collect();
    }
    let take = max - 1;
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

fn color_for(size: u64, is_dir: bool) -> Style {
    if is_dir {
        return Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD);
    }
    if size < MB {
        Style::default().fg(Color::Green)
    } else if size <= 100 * MB {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    }
}
