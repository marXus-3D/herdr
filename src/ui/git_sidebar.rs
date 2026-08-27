use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::AppState;
use crate::terminal::TerminalRuntimeRegistry;

pub fn render_git_sidebar(
    app: &AppState,
    _terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let state = &app.git_sidebar_state;
    let p = &app.palette;

    let repo_title = if !state.repo_name.is_empty() {
        format!(" SOURCE CONTROL: {} ({}) ", state.repo_name, state.branch)
    } else {
        " SOURCE CONTROL ".to_string()
    };

    let block = Block::default()
        .borders(Borders::LEFT)
        .title(repo_title)
        .style(Style::default().fg(p.text).bg(p.sidebar_bg));

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    // Two sections: changes (staged/unstaged) + commit, and commit graphs.
    let (changes_area, _divider_y, commits_area) = split_git_sidebar_sections(inner_area);

    let changes_chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(changes_area);

    let input_bg = if state.selected_index == 0 && app.mode == crate::app::state::Mode::GitSidebar {
        p.surface_dim
    } else {
        p.sidebar_bg
    };
    let input_text = if state.commit_message.is_empty() {
        Span::styled("Message (Enter to commit)", Style::default().fg(p.overlay0).bg(input_bg))
    } else {
        Span::styled(&state.commit_message, Style::default().bg(input_bg).fg(p.text))
    };
    let input_block = Paragraph::new(input_text).block(Block::default().borders(Borders::ALL).style(Style::default().fg(p.surface_dim)));
    frame.render_widget(input_block, changes_chunks[0]);

    let mut changes_items = Vec::new();
    let mut idx = 1;

    // Staged changes
    if !state.staged_files.is_empty() {
        let chev = if state.section_collapsed_staged { "?" } else { "?" };
        changes_items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", chev), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!("Staged Changes ({})", state.staged_files.len()), Style::default().add_modifier(Modifier::BOLD)),
        ])));
        if !state.section_collapsed_staged {
            for file in &state.staged_files {
                let path_str = file.path.to_string_lossy();
                let status_str = match file.status {
                    crate::app::git_sidebar::GitFileStatusKind::Added => "A",
                    crate::app::git_sidebar::GitFileStatusKind::Deleted => "D",
                    crate::app::git_sidebar::GitFileStatusKind::Modified => "M",
                    crate::app::git_sidebar::GitFileStatusKind::Renamed => "R",
                    crate::app::git_sidebar::GitFileStatusKind::Untracked => "U",
                };
                let bg = if idx == state.selected_index && app.mode == crate::app::state::Mode::GitSidebar { p.surface_dim } else { p.sidebar_bg };
                changes_items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} ", status_str), Style::default().fg(p.green).bg(bg)),
                    Span::styled(path_str.into_owned(), Style::default().bg(bg).fg(p.text)),
                ])).style(Style::default().bg(bg)));
                idx += 1;
            }
        }
    }

    // Unstaged changes
    if !state.unstaged_files.is_empty() {
        let chev = if state.section_collapsed_changes { "?" } else { "?" };
        changes_items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", chev), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!("Changes ({})", state.unstaged_files.len()), Style::default().add_modifier(Modifier::BOLD)),
        ])));
        if !state.section_collapsed_changes {
            for file in &state.unstaged_files {
                let path_str = file.path.to_string_lossy();
                let status_str = match file.status {
                    crate::app::git_sidebar::GitFileStatusKind::Added => "A",
                    crate::app::git_sidebar::GitFileStatusKind::Deleted => "D",
                    crate::app::git_sidebar::GitFileStatusKind::Modified => "M",
                    crate::app::git_sidebar::GitFileStatusKind::Renamed => "R",
                    crate::app::git_sidebar::GitFileStatusKind::Untracked => "U",
                };
                let bg = if idx == state.selected_index && app.mode == crate::app::state::Mode::GitSidebar { p.surface_dim } else { p.sidebar_bg };
                changes_items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} ", status_str), Style::default().fg(p.yellow).bg(bg)),
                    Span::styled(path_str.into_owned(), Style::default().bg(bg).fg(p.text)),
                ])).style(Style::default().bg(bg)));
                idx += 1;
            }
        }
    }

    if changes_items.is_empty() {
        if state.is_refreshing {
            changes_items.push(ListItem::new(Line::from("Refreshing...")));
        } else if let Some(err) = &state.error_message {
            changes_items.push(ListItem::new(Line::from(Span::styled(err.clone(), Style::default().fg(p.red)))));
        } else {
            changes_items.push(ListItem::new(Line::from(Span::styled("No changes", Style::default().fg(p.overlay0)))));
        }
    }

    let mut list_state = ListState::default();
    let changes_list = List::new(changes_items);
    frame.render_stateful_widget(changes_list, changes_chunks[1], &mut list_state);

    // Commits graph in bottom section
    let mut commits_items = Vec::new();
    if !state.recent_commits.is_empty() {
        let chev = if state.section_collapsed_commits { "?" } else { "?" };
        commits_items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", chev), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("Commits", Style::default().add_modifier(Modifier::BOLD)),
        ])));
        if !state.section_collapsed_commits {
            for commit in &state.recent_commits {
                let graph_str: String = commit.graph_columns.iter().collect();
                let bg = if idx == state.selected_index && app.mode == crate::app::state::Mode::GitSidebar { p.surface_dim } else { p.sidebar_bg };
                commits_items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(graph_str, Style::default().fg(p.overlay0).bg(bg)),
                    Span::raw(" "),
                    Span::styled(commit.hash.clone(), Style::default().fg(p.yellow).bg(bg)),
                    Span::raw(" "),
                    Span::styled(commit.subject.clone(), Style::default().bg(bg).fg(p.text)),
                ])).style(Style::default().bg(bg)));
                idx += 1;
            }
        }
    }

    if commits_area.height > 0 {
        let commits_list = List::new(commits_items);
        frame.render_stateful_widget(commits_list, commits_area, &mut list_state);
    }
}

pub fn render_git_sidebar_collapsed(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let p = &app.palette;
    frame.buffer_mut().set_style(area, Style::default().bg(p.sidebar_bg));
    
    // Draw left border
    let sep_style = Style::default().fg(p.surface_dim);
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buf[(area.x, y)].set_symbol("¦");
        buf[(area.x, y)].set_style(sep_style);
    }

    // Vertical text "SC"
    if area.height >= 3 && area.width >= 3 {
        let text_x = area.x + 1;
        let text_y = area.y + 1;
        buf[(text_x, text_y)].set_symbol("S");
        buf[(text_x, text_y)].set_style(Style::default().fg(p.text));
        buf[(text_x, text_y + 1)].set_symbol("C");
        buf[(text_x, text_y + 1)].set_style(Style::default().fg(p.text));
    }
}

pub fn render_git_sidebar_toggle(
    _app: &AppState,
    frame: &mut Frame,
    area: Rect,
    collapsed: bool,
    p: &crate::app::state::Palette,
) {
    let toggle_area = if collapsed {
        collapsed_git_sidebar_toggle_rect(area)
    } else {
        expanded_git_sidebar_toggle_rect(area)
    };
    if toggle_area == Rect::default() {
        return;
    }
    
    // Left-pointing arrow when expanded, right-pointing when collapsed
    let icon = if collapsed { "?" } else { "?" };
    let icon_style = Style::default().fg(p.overlay0);
    frame.render_widget(Paragraph::new(Span::styled(icon, icon_style)), toggle_area);
}

pub(crate) fn collapsed_git_sidebar_toggle_rect(area: Rect) -> Rect {
    let bottom_y = area.y + area.height.saturating_sub(1);
    let content_w = area.width.saturating_sub(1);
    if content_w == 0 || area.height == 0 {
        return Rect::default();
    }
    Rect::new(area.x, bottom_y, 1, 1)
}

pub(crate) fn expanded_git_sidebar_toggle_rect(area: Rect) -> Rect {
    if area.width <= 1 || area.height == 0 {
        return Rect::default();
    }
    Rect::new(area.x, area.y + area.height.saturating_sub(1), 1, 1)
}

fn split_git_sidebar_sections(area: Rect) -> (Rect, Option<u16>, Rect) {
    if area.height < 7 {
        return (area, None, Rect::default());
    }

    let total_h = area.height as usize;
    let changes_h = total_h.div_ceil(2);
    let commits_h = total_h.saturating_sub(changes_h + 1);
    
    let divider_y = area.y + changes_h as u16;
    let changes_area = Rect::new(area.x, area.y, area.width, changes_h as u16);
    let commits_area = Rect::new(area.x, divider_y + 1, area.width, commits_h as u16);
    (changes_area, Some(divider_y), commits_area)
}
