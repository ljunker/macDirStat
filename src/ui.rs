use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    app::{App, AppMode},
    format::format_size,
    tree::{Node, NodeKind, ScanState},
};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let [header_area, columns_area, list_area, footer_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    let title = Line::from(vec![
        Span::styled("macDirStat", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" — "),
        Span::raw(app.root.to_string_lossy()),
    ]);
    let summary = Line::from(format!(
        "Known total: {}   Visible: {}   Workers: {}",
        format_size(app.tree.root_known_size()),
        app.visible.len(),
        app.workers
    ));
    frame.render_widget(Paragraph::new(vec![title, summary]), header_area);

    frame.render_widget(
        Paragraph::new("Mark       Size  Name")
            .style(Style::default().add_modifier(Modifier::BOLD)),
        columns_area,
    );

    app.set_page_size(list_area.height as usize);
    let end = app
        .scroll_offset
        .saturating_add(app.page_size)
        .min(app.visible.len());
    let items: Vec<_> = app.visible[app.scroll_offset..end]
        .iter()
        .filter_map(|visible| {
            app.tree
                .nodes
                .get(visible.node_id)
                .map(|node| render_node(node, visible.depth, app.is_marked(visible.node_id)))
        })
        .collect();

    let mut list_state = ListState::default();
    if let Some(selected) = app.selected_index()
        && selected >= app.scroll_offset
        && selected < end
    {
        list_state.select(Some(selected - app.scroll_offset));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, list_area, &mut list_state);

    let footer = vec![
        Line::from(app.status_text()),
        Line::from(
            "↑↓ Navigate  g Top  → Open  ← Close  Space Mark  x Trash marked  d Trash one  r Refresh  q Quit",
        ),
    ];
    frame.render_widget(Paragraph::new(footer), footer_area);

    render_dialog(frame, app);
}

fn render_node(node: &Node, depth: usize, marked: bool) -> ListItem<'static> {
    let size = match (node.size, node.scan_state) {
        (Some(size), _) => format_size(size),
        (None, ScanState::Error) => "?".to_owned(),
        (None, _) => "…".to_owned(),
    };
    let icon = match node.kind {
        NodeKind::Directory if node.children_loading && node.expanded => "⟳",
        NodeKind::Directory if node.expanded => "▼",
        NodeKind::Directory => "▶",
        NodeKind::Symlink => "@",
        NodeKind::File => " ",
        NodeKind::Other => "?",
    };
    let warning = if node.warning_count > 0 || node.scan_state == ScanState::Error {
        "!"
    } else {
        " "
    };
    let marker = if marked { "[x]" } else { "[ ]" };
    let indentation = "  ".repeat(depth);
    let name = node.name.to_string_lossy().into_owned();
    let style = match (node.scan_state, node.kind) {
        (ScanState::Error, _) => Style::default().fg(Color::Red),
        (_, NodeKind::Directory) => Style::default().fg(Color::Cyan),
        (_, NodeKind::Symlink) => Style::default().fg(Color::Magenta),
        _ => Style::default(),
    };
    let line = Line::from(vec![
        Span::styled(format!("{marker} {warning} {size:>9}  "), style),
        Span::raw(indentation),
        Span::styled(format!("{icon} {name}"), style),
    ]);
    ListItem::new(line)
}

fn render_dialog(frame: &mut Frame<'_>, app: &App) {
    let (title, lines) = match &app.mode {
        AppMode::Browse => return,
        AppMode::ConfirmDelete { node_ids, multi } if !multi && node_ids.len() == 1 => {
            let Some(node) = app.tree.nodes.get(node_ids[0]) else {
                return;
            };
            let size = node
                .size
                .map_or_else(|| "unknown size".to_owned(), format_size);
            (
                " Move to Trash ",
                vec![
                    Line::from(format!(
                        "Move \"{}\" to Trash?",
                        node.name.to_string_lossy()
                    )),
                    Line::from(format!("Size: {size}")),
                    Line::from(node.path.to_string_lossy().into_owned()),
                    Line::from(""),
                    Line::from("[y] Yes    [n/Esc] Cancel"),
                ],
            )
        }
        AppMode::ConfirmDelete { node_ids, .. } => {
            let nodes: Vec<_> = node_ids
                .iter()
                .filter_map(|node_id| app.tree.nodes.get(*node_id))
                .collect();
            if nodes.is_empty() {
                return;
            }
            let known_total = nodes
                .iter()
                .filter_map(|node| node.size)
                .fold(0, u64::saturating_add);
            let unknown = nodes.iter().filter(|node| node.size.is_none()).count();
            let size = if unknown == 0 {
                format_size(known_total)
            } else {
                format!(
                    "at least {} ({unknown} still unknown)",
                    format_size(known_total)
                )
            };
            (
                " Move marked items to Trash ",
                vec![
                    Line::from(format!("Move {} marked items to Trash?", nodes.len())),
                    Line::from(format!("Combined size: {size}")),
                    Line::from(""),
                    Line::from("All items are validated before the first move."),
                    Line::from("[y] Yes    [n/Esc] Cancel"),
                ],
            )
        }
        AppMode::Deleting { node_ids, .. } => (
            " Moving to Trash ",
            vec![
                Line::from(format!("Moving {} item(s) to Trash", node_ids.len())),
                Line::from(""),
                Line::from("Please wait…"),
            ],
        ),
        AppMode::ErrorDialog(message) => (
            " Error ",
            vec![
                Line::from(message.clone()),
                Line::from(""),
                Line::from("Press Enter or Esc to close"),
            ],
        ),
    };

    let area = centered(frame.area(), 72, 9);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered(area: Rect, requested_width: u16, requested_height: u16) -> Rect {
    let width = requested_width.min(area.width);
    let height = requested_height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::{Terminal, backend::TestBackend};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn renders_at_small_and_normal_terminal_sizes() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("file"), b"data").unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();

        for (width, height) in [(20, 4), (100, 30)] {
            let mut app = App::new(root_path.clone(), 1).unwrap();
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
        }
    }
}
