use ratatui::text::{Line, Span};

use crate::display::{
    dim_style, modified_span, name_span, size_span, sort_nodes, RenderOptions, Row,
};
use crate::walker::Node;

pub fn render(root: &Node, opts: &RenderOptions) -> Vec<Row> {
    let mut rows = Vec::new();

    // Header row for root.
    let mut header = Vec::new();
    header.push(Span::styled(
        root.path.display().to_string(),
        ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD),
    ));
    header.push(Span::raw("  "));
    header.push(size_span(root, opts.theme));
    rows.push(Row {
        size: root.size,
        name: root.name.clone(),
        line: Line::from(header),
        path: Some(root.path.clone()),
        is_dir: root.is_dir,
        is_hidden_summary: false,
    });

    if root.is_dir && opts.depth > 0 {
        render_children(root, 0, opts, &mut Vec::new(), &mut rows);
    }

    if let Some((count, size)) = opts.hidden_summary {
        if count > 0 {
            rows.push(crate::display::hidden_summary_row(count, size, root.size));
        }
    }
    rows
}

fn render_children(
    node: &Node,
    current_depth: usize,
    opts: &RenderOptions,
    prefix_stack: &mut Vec<bool>,
    out: &mut Vec<Row>,
) {
    if current_depth >= opts.depth {
        return;
    }

    let mut visible: Vec<&Node> = node
        .children
        .iter()
        .filter(|c| c.size >= opts.min_size)
        .filter(|c| opts.show_files || c.is_dir)
        .filter(|c| opts.hidden_summary.is_none() || !c.name.starts_with('.'))
        .collect();
    sort_nodes(&mut visible, opts.sort, opts.sort_order);

    let count = visible.len();
    for (idx, child) in visible.iter().enumerate() {
        let is_last = idx + 1 == count;

        let mut prefix = String::new();
        for had_more in prefix_stack.iter() {
            if *had_more {
                prefix.push_str("│   ");
            } else {
                prefix.push_str("    ");
            }
        }
        let branch = if is_last { "└── " } else { "├── " };

        let mut spans = Vec::new();
        spans.push(Span::styled(prefix, dim_style(opts.theme)));
        spans.push(Span::styled(branch.to_string(), dim_style(opts.theme)));
        spans.push(name_span(child, opts.theme));
        spans.push(Span::raw("  "));
        spans.push(size_span(child, opts.theme));
        if opts.show_modified {
            spans.push(Span::raw("  "));
            spans.push(modified_span(child, opts.theme));
        }

        out.push(Row {
            size: child.size,
            name: child.name.clone(),
            line: Line::from(spans),
            path: Some(child.path.clone()),
            is_dir: child.is_dir,
            is_hidden_summary: false,
        });

        if child.is_dir {
            prefix_stack.push(!is_last);
            render_children(child, current_depth + 1, opts, prefix_stack, out);
            prefix_stack.pop();
        }
    }
}
