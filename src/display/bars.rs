use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::cli::{SortBy, SortOrder};
use crate::display::{dim_style, human_size, name_span, size_style, RenderOptions, Row};
use crate::walker::Node;

/// One slot in the bars list — either a real child node or the synthetic
/// "hidden items" aggregate. Both participate in sorting and bar scaling
/// identically.
enum Entry<'a> {
    Real(&'a Node),
    Hidden { size: u64 },
}

impl<'a> Entry<'a> {
    fn size(&self) -> u64 {
        match self {
            Entry::Real(n) => n.size,
            Entry::Hidden { size } => *size,
        }
    }

    fn name_key(&self) -> String {
        match self {
            Entry::Real(n) => n.name.to_lowercase(),
            Entry::Hidden { .. } => "hidden items".to_string(),
        }
    }
}

fn sort_entries(entries: &mut [Entry<'_>], sort: SortBy, order: SortOrder) {
    match (sort, order) {
        (SortBy::Size, SortOrder::Desc) => entries.sort_by(|a, b| b.size().cmp(&a.size())),
        (SortBy::Size, SortOrder::Asc) => entries.sort_by(|a, b| a.size().cmp(&b.size())),
        (SortBy::Name, SortOrder::Asc) => entries.sort_by(|a, b| a.name_key().cmp(&b.name_key())),
        (SortBy::Name, SortOrder::Desc) => entries.sort_by(|a, b| b.name_key().cmp(&a.name_key())),
    }
}

pub fn render(root: &Node, opts: &RenderOptions) -> Vec<Row> {
    let mut rows = Vec::new();

    // Header
    let header = vec![
        Span::styled(
            root.path.display().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(human_size(root.size), size_style(opts.theme, root.size)),
    ];
    rows.push(Row {
        size: root.size,
        name: root.name.clone(),
        line: Line::from(header),
        path: Some(root.path.clone()),
        is_dir: root.is_dir,
        is_hidden_summary: false,
    });

    if !root.is_dir {
        return rows;
    }

    // Real children visible at this level (with the hidden ones already
    // filtered out when `hidden_summary` is set).
    let mut entries: Vec<Entry> = root
        .children
        .iter()
        .filter(|c| c.size >= opts.min_size)
        .filter(|c| opts.show_files || c.is_dir)
        .filter(|c| opts.hidden_summary.is_none() || !c.name.starts_with('.'))
        .map(Entry::Real)
        .collect();

    // Mix in the synthetic aggregate so it participates in sorting AND in the
    // shared bar scale (max_size includes it).
    if let Some((count, size)) = opts.hidden_summary {
        if count > 0 {
            entries.push(Entry::Hidden { size });
        }
    }
    sort_entries(&mut entries, opts.sort, opts.sort_order);

    let max_size = entries.iter().map(|e| e.size()).max().unwrap_or(1).max(1);
    let total_width = opts.width.max(40) as usize;
    let name_w = 24;
    let size_w = 12;
    let bar_w = total_width.saturating_sub(name_w + size_w + 4);

    for entry in &entries {
        // Build a Node we can render through the single shared code path.
        // For real entries we render the node directly; for the synthetic
        // Hidden Items entry we synthesize a Node-shaped value with the same
        // fields a real "file" node would have (path empty, is_dir=false,
        // modified=None) so the rendering branch below produces a Row whose
        // span layout is byte-for-byte identical to a real row.
        let synthetic;
        let (child, is_synthetic): (&Node, bool) = match entry {
            Entry::Real(c) => (*c, false),
            Entry::Hidden { size } => {
                // Bake the percentage directly into the synthetic node's
                // displayed name so it reads as `Hidden Items  X.X%` inside
                // the normal name column. The row's `name` is later cleared
                // to the empty string so `draw_body` does not append its own
                // trailing percent span (which would land at the right edge
                // and visually misalign with real entries).
                let pct = (*size as f64 / root.size.max(1) as f64) * 100.0;
                let label = format!("Hidden Items  {:.1}%", pct);
                synthetic = Node {
                    path: std::path::PathBuf::new(),
                    name: label,
                    size: *size,
                    is_dir: false,
                    modified: None,
                    mode: None,
                    children: Vec::new(),
                };
                (&synthetic, true)
            }
        };

        let mut name = child.name.clone();
        if name.len() > name_w {
            name.truncate(name_w - 1);
            name.push('…');
        }
        let pad = name_w.saturating_sub(name.chars().count());
        let name_padded = format!("{}{}", name, " ".repeat(pad));

        let mut name_span_styled = name_span(child, opts.theme);
        name_span_styled.content = name_padded.into();

        let size_str = human_size(child.size);
        let size_pad = size_w.saturating_sub(size_str.chars().count());
        let size_styled = Span::styled(
            format!("{}{}", " ".repeat(size_pad), size_str),
            size_style(opts.theme, child.size),
        );

        let bar_len = ((child.size as u128 * bar_w as u128) / max_size as u128) as usize;
        let bar = "█".repeat(bar_len);
        let bar_rest = "░".repeat(bar_w.saturating_sub(bar_len));
        let bar_styled = Span::styled(bar, size_style(opts.theme, child.size));
        let bar_rest_styled = Span::styled(bar_rest, dim_style(opts.theme));

        let mut spans = vec![
            name_span_styled,
            Span::raw(" "),
            size_styled,
            Span::raw(" "),
            bar_styled,
            bar_rest_styled,
        ];
        if opts.show_modified && !is_synthetic {
            spans.push(Span::raw("  "));
            spans.push(crate::display::modified_span(child, opts.theme));
        }

        rows.push(Row {
            size: child.size,
            // For the synthetic row, leave `name` empty so `draw_body` does
            // not append its own percent column (the percent is already part
            // of the name text above).
            name: if is_synthetic {
                String::new()
            } else {
                child.name.clone()
            },
            line: Line::from(spans),
            path: if is_synthetic {
                None
            } else {
                Some(child.path.clone())
            },
            is_dir: child.is_dir,
            is_hidden_summary: is_synthetic,
        });
    }

    rows
}
