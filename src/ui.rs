use chrono::{DateTime, Local};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    app::{AnalysisStatus, App, AppMode, PromptKind, ViewKind},
    format::format_size,
    theme::Theme,
    tree::{Node, NodeKind, ScanState, SizeMode},
};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let theme = Theme::for_kind(app.theme);
    let [header_area, content_area, footer_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    render_header(frame, app, header_area, theme);
    render_content(frame, app, content_area, theme);
    render_footer(frame, app, footer_area);
    render_overlay(frame, app, theme);
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let title = Line::from(vec![
        Span::styled("macDirStat", theme.title),
        Span::raw(" — "),
        Span::raw(app.root.to_string_lossy()),
    ]);
    let tabs = Line::from(
        ViewKind::ALL
            .iter()
            .enumerate()
            .flat_map(|(index, view)| {
                let style = if *view == app.active_view {
                    theme.selected
                } else {
                    theme.columns
                };
                [
                    Span::styled(format!(" {}:{} ", index + 1, view.label()), style),
                    Span::raw(" "),
                ]
            })
            .collect::<Vec<_>>(),
    );
    let usage = app
        .analysis_index
        .as_ref()
        .filter(|_| app.active_view != ViewKind::Tree)
        .map_or_else(
            || app.tree.root_known_usage(),
            |_| app.analysis_known_usage(),
        );
    let visible = if app.active_view == ViewKind::Tree {
        app.visible.len()
    } else {
        app.analysis_rows.len()
    };
    let summary = Line::from(format!(
        "{}: {}   Files: {}   Visible: {}   Sort: {}{}   Theme: {}",
        capitalize(app.size_mode.label()),
        format_size(usage.size(app.size_mode)),
        usage.files,
        visible,
        app.sort.key.label(),
        app.sort.direction.symbol(),
        app.theme.label(),
    ));
    frame.render_widget(Paragraph::new(vec![title, tabs, summary]), area);
}

fn render_content(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    if app.detail_panel && area.width >= 100 && area.height >= 4 {
        let [tree_area, detail_area] =
            Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)])
                .areas(area);
        render_primary(frame, app, tree_area, theme);
        render_detail(frame, app, detail_area, theme);
    } else if app.detail_panel && area.height >= 12 {
        let [tree_area, detail_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(8)]).areas(area);
        render_primary(frame, app, tree_area, theme);
        render_detail(frame, app, detail_area, theme);
    } else {
        render_primary(frame, app, area, theme);
    }
}

fn render_primary(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    if app.active_view == ViewKind::Tree {
        render_tree(frame, app, area, theme);
    } else {
        render_analysis(frame, app, area, theme);
    }
}

fn render_analysis(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let [columns_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let columns = match app.active_view {
        ViewKind::Largest => "Mark       Size       Files  Type / Name",
        ViewKind::Types => "           Size       Files  Group",
        ViewKind::Duplicates => "Mark    Reclaim/Size   Files  Duplicate group / file",
        ViewKind::Changes => "Mark       Size       Files  Change / Name",
        ViewKind::Tree => "",
    };
    frame.render_widget(Paragraph::new(columns).style(theme.columns), columns_area);
    app.set_list_area(list_area);

    let end = app
        .analysis_scroll
        .saturating_add(app.page_size)
        .min(app.analysis_rows.len());
    let mut items = app.analysis_rows[app.analysis_scroll..end]
        .iter()
        .map(|row| {
            let marker = row
                .path
                .as_ref()
                .filter(|path| app.can_delete_path(path))
                .map_or("   ", |path| {
                    if app.is_path_marked(path) {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                });
            let name = format!("{}{}", "  ".repeat(row.indent), row.label);
            ListItem::new(Line::from(vec![
                Span::raw(format!(
                    "{marker} {:>10} {:>10}  ",
                    format_size(row.usage.size(app.size_mode)),
                    row.files
                )),
                Span::styled(format!("{} · {name}", row.detail), theme.directory),
            ]))
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        let message = match app.analysis_status {
            AnalysisStatus::Indexing => "… Building full analysis index in the background",
            AnalysisStatus::Hashing => "… Hashing exact duplicate candidates",
            AnalysisStatus::Ready if app.active_view == ViewKind::Changes => {
                "No changes against the previous snapshot"
            }
            AnalysisStatus::Ready => "No matching entries",
            AnalysisStatus::Idle => "Analysis has not started",
        };
        items.push(ListItem::new(Line::from(message)));
    }
    let mut state = ListState::default();
    if !app.analysis_rows.is_empty()
        && app.analysis_selected >= app.analysis_scroll
        && app.analysis_selected < end
    {
        state.select(Some(app.analysis_selected - app.analysis_scroll));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(theme.selected),
        list_area,
        &mut state,
    );
}

fn render_tree(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: Theme) {
    let [columns_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new("Mark Src       Size      %      Files  Name").style(theme.columns),
        columns_area,
    );

    app.set_list_area(list_area);
    let end = app
        .scroll_offset
        .saturating_add(app.page_size)
        .min(app.visible.len());
    let items: Vec<_> = app.visible[app.scroll_offset..end]
        .iter()
        .filter_map(|visible| {
            app.tree.nodes.get(visible.node_id).map(|node| {
                render_node(
                    node,
                    visible.depth,
                    app.is_marked(visible.node_id),
                    visible.matched,
                    app.size_mode,
                    app.percentage(visible.node_id),
                    theme,
                )
            })
        })
        .collect();

    let mut list_state = ListState::default();
    if let Some(selected) = app.selected_index()
        && selected >= app.scroll_offset
        && selected < end
    {
        list_state.select(Some(selected - app.scroll_offset));
    }
    let list = List::new(items).highlight_style(theme.selected);
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

fn render_node(
    node: &Node,
    depth: usize,
    marked: bool,
    matched: bool,
    size_mode: SizeMode,
    percentage: Option<f64>,
    theme: Theme,
) -> ListItem<'static> {
    let size = match (node.usage, node.scan_state, node.mountpoint) {
        (_, _, true) => "mount".to_owned(),
        (Some(usage), _, _) => format_size(usage.size(size_mode)),
        (None, ScanState::Error, _) => "?".to_owned(),
        (None, _, _) => "…".to_owned(),
    };
    let files = node
        .usage
        .map_or_else(|| "–".to_owned(), |usage| usage.files.to_string());
    let icon = match node.kind {
        NodeKind::Directory if node.mountpoint => "⛁",
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
    let source = if node.cached { "≈" } else { " " };
    let marker = if marked { "[x]" } else { "[ ]" };
    let indentation = "  ".repeat(depth);
    let name = node.name.to_string_lossy().into_owned();
    let mut style = match (node.scan_state, node.mountpoint, node.kind) {
        (ScanState::Error, _, _) => theme.error,
        (_, true, _) => theme.mountpoint,
        (_, _, NodeKind::Directory) => theme.directory,
        (_, _, NodeKind::Symlink) => theme.symlink,
        _ => Style::default(),
    };
    if matched {
        style = style.patch(theme.matched);
    }
    let line = Line::from(vec![
        Span::styled(
            format!(
                "{marker} {source}{warning} {size:>10} {:>6} {files:>10}  ",
                format_percentage(percentage)
            ),
            style,
        ),
        Span::raw(indentation),
        Span::styled(format!("{icon} {name}"), style),
    ]);
    ListItem::new(line)
}

fn render_detail(frame: &mut Frame<'_>, app: &App, area: Rect, theme: Theme) {
    let lines =
        if app.active_view != ViewKind::Tree {
            app.selected_analysis_row().map_or_else(
                || {
                    vec![Line::from(match app.analysis_status {
                        AnalysisStatus::Idle => "Analysis has not started",
                        AnalysisStatus::Indexing => "Building analysis index…",
                        AnalysisStatus::Hashing => "Hashing duplicate candidates…",
                        AnalysisStatus::Ready => "No analysis row selected",
                    })]
                },
                |row| {
                    vec![
                        Line::from(format!("{} · {}", app.active_view.label(), row.detail)),
                        Line::from(format!(
                            "Logical: {} | Physical: {}",
                            format_size(row.usage.logical),
                            format_size(row.usage.physical)
                        )),
                        Line::from(format!("Files: {}", row.files)),
                        Line::from(row.path.as_ref().map_or_else(
                            || row.label.clone(),
                            |path| path.to_string_lossy().into(),
                        )),
                        Line::from("Enter reveals paths in Tree; Space marks individual paths"),
                    ]
                },
            )
        } else {
            app.selected_node().map_or_else(
                || vec![Line::from("No item selected")],
                |node| {
                    let logical = node
                        .usage
                        .map_or_else(|| "unknown".to_owned(), |usage| format_size(usage.logical));
                    let physical = node
                        .usage
                        .map_or_else(|| "unknown".to_owned(), |usage| format_size(usage.physical));
                    let files = node
                        .usage
                        .map_or_else(|| "unknown".to_owned(), |usage| usage.files.to_string());
                    let modified = node.modified.map_or_else(
                        || "unknown".to_owned(),
                        |modified| {
                            let timestamp: DateTime<Local> = modified.into();
                            timestamp.format("%Y-%m-%d %H:%M:%S").to_string()
                        },
                    );
                    let source = if node.cached { "cached" } else { "fresh" };
                    let mountpoint = if node.mountpoint {
                        " | mount point"
                    } else {
                        ""
                    };
                    let error = node.error.as_deref().unwrap_or("none");
                    vec![
                        Line::from(format!(
                            "{} | {} | {source}{mountpoint}",
                            node.kind.label(),
                            node.scan_state.label()
                        )),
                        Line::from(format!("Logical: {logical} | Physical: {physical}")),
                        Line::from(format!(
                            "Files: {files} | Relative: {}",
                            format_percentage(app.selected_percentage())
                        )),
                        Line::from(format!("Modified: {modified}")),
                        Line::from(node.path.to_string_lossy().into_owned()),
                        Line::from(format!("Warnings: {} | Error: {error}", node.warning_count)),
                    ]
                },
            )
        };
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.dialog)
            .block(Block::default().title(" Details ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let filter = app
        .filter_query
        .as_ref()
        .map_or_else(String::new, |query| format!(" | filter: {query}"));
    let footer = vec![
        Line::from(format!("{}{}", app.status_text(), filter)),
        Line::from(
            "? Help  Tab/1-5 Views  ↑↓/jk Move  Enter Reveal/Open  / Search  f Filter  e Export  w Watch  Space Mark  d/x Trash  q Quit",
        ),
    ];
    frame.render_widget(Paragraph::new(footer), area);
}

fn render_overlay(frame: &mut Frame<'_>, app: &App, theme: Theme) {
    match &app.mode {
        AppMode::Browse => {}
        AppMode::Help => render_help(frame, app, theme),
        AppMode::Input { kind, value, .. } => render_prompt(frame, *kind, value, theme),
        AppMode::ConfirmDelete { items, multi } => {
            render_delete_confirmation(frame, app, items, *multi, theme);
        }
        AppMode::Deleting { items, .. } => render_message(
            frame,
            " Moving to Trash ",
            vec![
                Line::from(format!("Moving {} item(s) to Trash", items.len())),
                Line::from(""),
                Line::from("Please wait…"),
            ],
            theme,
        ),
        AppMode::ErrorDialog(message) => render_message(
            frame,
            " Error ",
            vec![
                Line::from(message.clone()),
                Line::from(""),
                Line::from("Press Enter or Esc to close"),
            ],
            theme,
        ),
    }
}

fn render_delete_confirmation(
    frame: &mut Frame<'_>,
    app: &App,
    items: &[crate::delete::DeleteItem],
    multi: bool,
    theme: Theme,
) {
    if !multi && items.len() == 1 {
        let item = &items[0];
        let node = item.node_id.and_then(|node_id| app.tree.nodes.get(node_id));
        let usage = node.and_then(|node| node.usage).or_else(|| {
            app.analysis_index.as_ref().and_then(|index| {
                index
                    .files
                    .iter()
                    .find(|file| file.path == item.path)
                    .map(|file| file.usage)
            })
        });
        let size = usage.map_or_else(
            || "unknown size".to_owned(),
            |usage| format_size(usage.size(app.size_mode)),
        );
        let name = item
            .path
            .file_name()
            .unwrap_or(item.path.as_os_str())
            .to_string_lossy();
        render_message(
            frame,
            " Move to Trash ",
            vec![
                Line::from(format!("Move \"{}\" to Trash?", name)),
                Line::from(format!("Size: {size}")),
                Line::from(item.path.to_string_lossy().into_owned()),
                Line::from(""),
                Line::from("[y] Yes    [n/Esc] Cancel"),
            ],
            theme,
        );
        return;
    }

    let usages: Vec<_> = items
        .iter()
        .map(|item| {
            item.node_id
                .and_then(|node_id| app.tree.nodes.get(node_id))
                .and_then(|node| node.usage)
                .or_else(|| {
                    app.analysis_index.as_ref().and_then(|index| {
                        index
                            .files
                            .iter()
                            .find(|file| file.path == item.path)
                            .map(|file| file.usage)
                    })
                })
        })
        .collect();
    if items.is_empty() {
        return;
    }
    let known_total = usages
        .iter()
        .filter_map(|usage| *usage)
        .map(|usage| usage.size(app.size_mode))
        .fold(0, u64::saturating_add);
    let unknown = usages.iter().filter(|usage| usage.is_none()).count();
    let size = if unknown == 0 {
        format_size(known_total)
    } else {
        format!(
            "at least {} ({unknown} still unknown)",
            format_size(known_total)
        )
    };
    render_message(
        frame,
        " Move marked items to Trash ",
        vec![
            Line::from(format!("Move {} marked items to Trash?", items.len())),
            Line::from(format!("Combined size: {size}")),
            Line::from(""),
            Line::from("All items are validated before the first move."),
            Line::from("[y] Yes    [n/Esc] Cancel"),
        ],
        theme,
    );
}

fn render_help(frame: &mut Frame<'_>, app: &App, theme: Theme) {
    let lines = [
        "Navigation",
        "  ↑/k, ↓/j        move selection",
        "  ←/h, →/l        close/open directory",
        "  Enter           toggle directory",
        "  g/G             first/last loaded row",
        "  Home/End/PgUp/PgDn",
        "  Backspace       use parent directory as root",
        "",
        "Find and view",
        "  Tab/Shift-Tab   next/previous view",
        "  1..5            Tree/Largest/Types/Duplicates/Changes",
        "  Enter           open directory or reveal analysis path",
        "  /               search loaded nodes",
        "  n/N             next/previous search result",
        "  f/F             set/clear AND filter predicates",
        "                  size>1GiB age>30d ext:log type:image",
        "  s/S             cycle sort key/reverse direction",
        "  z               logical/physical size",
        "  i               toggle detail panel",
        "  t               cycle theme",
        "  m               toggle mouse capture",
        "  w               toggle native filesystem watcher",
        "  e               export full JSON snapshot",
        "",
        "Scanning and cleanup",
        "  Esc             cancel active scan",
        "  r               fresh root scan without cache",
        "  Space           mark/unmark item",
        "  d               trash selected item",
        "  x               trash all marked items",
        "  q / Ctrl+C      quit",
        "",
        "Mouse (when enabled)",
        "  click row       select",
        "  click checkbox  mark/unmark",
        "  double-click    open/close directory",
        "  wheel           move selection",
        "",
        "Press ? or Esc to close help",
    ];
    let area = centered(frame.area(), 86, frame.area().height.saturating_sub(2));
    let available = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(available);
    let scroll = app.help_scroll.min(max_scroll);
    let visible: Vec<_> = lines
        .iter()
        .skip(scroll)
        .take(available)
        .map(|line| Line::from(*line))
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(visible)
            .style(theme.dialog)
            .block(Block::default().title(" Help ").borders(Borders::ALL)),
        area,
    );
}

fn render_prompt(frame: &mut Frame<'_>, kind: PromptKind, value: &str, theme: Theme) {
    let (title, hint) = match kind {
        PromptKind::Search => (" Search loaded tree ", "Enter search  Esc cancel"),
        PromptKind::Filter => (
            " Filter tree and analysis ",
            "AND: name size>1GiB age>30d ext:log|none type:image  Enter keep",
        ),
        PromptKind::Export => (" Export JSON snapshot ", "Enter write file  Esc cancel"),
    };
    let area = centered(frame.area(), 72, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![Span::styled("> ", theme.accent), Span::raw(value)]),
            Line::from(""),
            Line::from(hint),
        ])
        .style(theme.dialog)
        .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn render_message(frame: &mut Frame<'_>, title: &str, lines: Vec<Line<'static>>, theme: Theme) {
    let area = centered(frame.area(), 72, 9);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.dialog)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn format_percentage(value: Option<f64>) -> String {
    match value {
        Some(value) if value > 0.0 && value < 1.0 => "<1%".to_owned(),
        Some(value) => format!("{value:.0}%"),
        None => "–".to_owned(),
    }
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
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
    use crate::{
        app::{AnalysisRow, AnalysisStatus, ViewKind},
        theme::ThemeKind,
        tree::UsageStats,
    };

    #[test]
    fn renders_small_narrow_and_wide_layouts_in_all_themes() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("file"), b"data").unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();

        for theme in [
            ThemeKind::Default,
            ThemeKind::Monochrome,
            ThemeKind::HighContrast,
        ] {
            for (width, height) in [(20, 4), (80, 24), (120, 30)] {
                let mut app = App::new(root_path.clone(), 1).unwrap();
                app.theme = theme;
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn renders_help_and_prompt_overlays() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(root_path, 1).unwrap();

        app.mode = AppMode::Help;
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.mode = AppMode::Input {
            kind: PromptKind::Filter,
            value: "cache".to_owned(),
            previous_filter: None,
        };
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
    }

    #[test]
    fn renders_every_analysis_tab_with_adaptive_details() {
        let root = tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        for view in [
            ViewKind::Largest,
            ViewKind::Types,
            ViewKind::Duplicates,
            ViewKind::Changes,
        ] {
            let mut app = App::new(root_path.clone(), 1).unwrap();
            app.active_view = view;
            app.analysis_status = AnalysisStatus::Ready;
            app.analysis_rows = vec![AnalysisRow {
                label: "sample".to_owned(),
                path: Some(root_path.join("sample")),
                usage: UsageStats {
                    logical: 100,
                    physical: 50,
                    files: 1,
                },
                files: 1,
                detail: view.label().to_owned(),
                indent: usize::from(view == ViewKind::Duplicates),
            }];
            for (width, height) in [(50, 16), (120, 30)] {
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
            }
        }
    }

    #[test]
    fn formats_percentages() {
        assert_eq!(format_percentage(None), "–");
        assert_eq!(format_percentage(Some(0.4)), "<1%");
        assert_eq!(format_percentage(Some(42.4)), "42%");
    }
}
