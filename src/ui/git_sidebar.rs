use ratatui::{
    layout::{Rect, Layout, Constraint},
    Frame,
};
use ratatui::widgets::{Block, Borders, Paragraph, List, ListItem, ListState};
use ratatui::style::{Style, Color, Modifier};
use ratatui::text::{Span, Line};
use crate::app::AppState;
use crate::terminal::TerminalRuntimeRegistry;

pub fn render_git_sidebar(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .title("SOURCE CONTROL")
        .style(Style::default().fg(Color::Gray));

    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
    ]).split(inner_area);

    let state = &app.git_sidebar_state;
    
    let input_bg = if state.selected_index == 0 && app.mode == crate::app::state::Mode::GitSidebar { Color::DarkGray } else { Color::Reset };
    let input_text = if state.commit_message.is_empty() {
        Span::styled("Message (Enter to commit)", Style::default().fg(Color::DarkGray).bg(input_bg))
    } else {
        Span::styled(&state.commit_message, Style::default().bg(input_bg))
    };
    let input_block = Paragraph::new(input_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(input_block, chunks[0]);

    let mut items = Vec::new();
    let mut idx = 1;

    // Staged changes
    if !state.staged_files.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("v ", Style::default().add_modifier(Modifier::BOLD)),
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
                let bg = if idx == state.selected_index && app.mode == crate::app::state::Mode::GitSidebar { Color::DarkGray } else { Color::Reset };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} ", status_str), Style::default().fg(Color::Green).bg(bg)),
                    Span::styled(path_str.into_owned(), Style::default().bg(bg)),
                ])).style(Style::default().bg(bg)));
                idx += 1;
            }
        }
    }

    // Unstaged changes
    if !state.unstaged_files.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("v ", Style::default().add_modifier(Modifier::BOLD)),
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
                let bg = if idx == state.selected_index && app.mode == crate::app::state::Mode::GitSidebar { Color::DarkGray } else { Color::Reset };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{} ", status_str), Style::default().fg(Color::Yellow).bg(bg)),
                    Span::styled(path_str.into_owned(), Style::default().bg(bg)),
                ])).style(Style::default().bg(bg)));
                idx += 1;
            }
        }
    }
    
    // Commits graph
    if !state.recent_commits.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("v ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("Commits", Style::default().add_modifier(Modifier::BOLD)),
        ])));
        if !state.section_collapsed_commits {
            for commit in &state.recent_commits {
                let graph_str: String = commit.graph_columns.iter().collect();
                let bg = if idx == state.selected_index && app.mode == crate::app::state::Mode::GitSidebar { Color::DarkGray } else { Color::Reset };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(graph_str, Style::default().fg(Color::DarkGray).bg(bg)),
                    Span::raw(" "),
                    Span::styled(commit.hash.clone(), Style::default().fg(Color::Yellow).bg(bg)),
                    Span::raw(" "),
                    Span::styled(commit.subject.clone(), Style::default().bg(bg)),
                ])).style(Style::default().bg(bg)));
                idx += 1;
            }
        }
    }
    
    if items.is_empty() {
        if state.is_refreshing {
            items.push(ListItem::new(Line::from("Refreshing...")));
        } else if let Some(err) = &state.error_message {
            items.push(ListItem::new(Line::from(Span::styled(err.clone(), Style::default().fg(Color::Red)))));
        } else {
            items.push(ListItem::new(Line::from("No changes")));
        }
    }

    let mut list_state = ListState::default();
    // TODO: wire up scroll state
    let list = List::new(items);
    frame.render_stateful_widget(list, chunks[1], &mut list_state);
}

pub fn render_git_sidebar_collapsed(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
) {
    // Phase 4 implementation
}
