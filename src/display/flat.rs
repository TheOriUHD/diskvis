use std::path::Path;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::cli::SortOrder;
use crate::display::{
    dim_style, human_size, modified_span, size_style, RenderOptions, Row,
};
use crate::walker::Node;

pub fn render(root: &Node, opts: &RenderOptions) -> Vec<Row> {
    let mut rows = Vec::new();

    let header = vec![
        Span::styled(
            format!("{} (flat files)", root.path.display()),
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

    let mut files: Vec<&Node> = root
        .all_files()
        .into_iter()
        .filter(|c| c.size >= opts.min_size)
        .filter(|c| {
            // When hiding hidden entries, also drop any file that resides
            // anywhere under a hidden top-level directory or whose own name
            // begins with `.`.
            if opts.hidden_summary.is_none() {
                return true;
            }
            if c.name.starts_with('.') {
                return false;
            }
            if let Ok(rel) = c.path.strip_prefix(root.path.as_path()) {
                if let Some(first) = rel.components().next() {
                    let s = first.as_os_str().to_string_lossy();
                    if s.starts_with('.') {
                        return false;
                    }
                }
            }
            true
        })
        .collect();
    match (opts.sort, opts.sort_order) {
        (crate::cli::SortBy::Size, SortOrder::Desc) => files.sort_by(|a, b| b.size.cmp(&a.size)),
        (crate::cli::SortBy::Size, SortOrder::Asc) => files.sort_by(|a, b| a.size.cmp(&b.size)),
        (crate::cli::SortBy::Name, SortOrder::Asc) => {
            files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }
        (crate::cli::SortBy::Name, SortOrder::Desc) => {
            files.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase()))
        }
    }

    let root_path = root.path.as_path();
    for file in files {
        let rel = relative_path(file.path.as_path(), root_path);
        let size_str = human_size(file.size);
        let size_pad = 12usize.saturating_sub(size_str.chars().count());
        let size_styled = Span::styled(
            format!("{}{}", " ".repeat(size_pad), size_str),
            size_style(opts.theme, file.size),
        );

        let mut spans = vec![
            size_styled,
            Span::raw("  "),
            Span::styled(rel, dim_style(opts.theme)),
        ];
        if opts.show_modified {
            spans.push(Span::raw("  "));
            spans.push(modified_span(file, opts.theme));
        }
        rows.push(Row {
            size: file.size,
            name: file.name.clone(),
            line: Line::from(spans),
            path: Some(file.path.clone()),
            is_dir: false,
            is_hidden_summary: false,
        });
    }

    if let Some((count, size)) = opts.hidden_summary {
        if count > 0 {
            rows.push(crate::display::hidden_summary_row(count, size, root.size));
        }
    }
    rows
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}
