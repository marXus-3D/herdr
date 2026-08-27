//! Source-control sidebar rendering and geometry.
//!
//! Visually this is the left sidebar mirrored: a one-column separator on the
//! inner edge, lowercase bold section headers, one-space row prefixes, the same
//! scrollbar, and a chevron toggle in the bottom inner corner. What sits inside
//! is the VS Code "Source Control" view — a commit box over collapsible groups
//! of changed files.
//!
//! Geometry here is pure. `compute_git_sidebar_row_areas` runs during
//! `compute_view` and stores one rect per visible row in
//! `ViewState::git_sidebar_rows`, which both this renderer and mouse
//! hit-testing read, so a click can never land on a different row than the one
//! drawn there.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::scrollbar::{render_scrollbar, should_show_scrollbar};
use super::text::{display_width, truncate_end};
use crate::app::git_sidebar::{
    GitFileStatus, GitFileStatusKind, GitSection, GitSidebarFocus, GitSidebarRow, GitSidebarState,
};
use crate::app::state::{GitSidebarRowArea, Palette};
use crate::app::{AppState, Mode};
use crate::pane::ScrollMetrics;

/// Bordered single-line commit message box.
const MESSAGE_BOX_ROWS: u16 = 3;
/// Horizontal rule under the commit box.
const DIVIDER_ROWS: u16 = 1;
/// Bottom row holding the toggle chevron and the change summary.
const FOOTER_ROWS: u16 = 1;
/// Rows the commit box needs before it is worth showing: the box, its divider,
/// and at least one list row underneath.
const MESSAGE_BOX_BUDGET: u16 = MESSAGE_BOX_ROWS + DIVIDER_ROWS + 1;

/// Resolved geometry of an expanded source-control panel.
///
/// Every rect is `Rect::default()` when there is not enough room for that part,
/// so callers can render unconditionally and get nothing for a squeezed panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GitSidebarLayout {
    /// The panel minus its separator column.
    pub content: Rect,
    /// " source control".
    pub title: Rect,
    /// Repository name, branch, and ahead/behind counts.
    pub meta: Rect,
    /// The bordered commit box, borders included.
    pub message_box: Rect,
    /// The single editable line inside the commit box.
    pub message_input: Rect,
    /// Row of the rule between the commit box and the list.
    pub divider_y: Option<u16>,
    /// Scrolling row list, scrollbar column included.
    pub list: Rect,
    /// Toggle chevron and change summary.
    pub footer: Rect,
}

/// Split an expanded panel rect into its parts.
///
/// The panel lives on the right, so its separator is the *first* column and
/// content is inset from the left — the mirror of the left sidebar.
pub(crate) fn git_sidebar_layout(area: Rect) -> GitSidebarLayout {
    if area.width <= 1 || area.height == 0 {
        return GitSidebarLayout::default();
    }

    let content = Rect::new(area.x + 1, area.y, area.width - 1, area.height);
    let mut layout = GitSidebarLayout {
        content,
        ..GitSidebarLayout::default()
    };

    let bottom = content.y + content.height;
    let mut y = content.y;

    if y < bottom {
        layout.title = Rect::new(content.x, y, content.width, 1);
        y += 1;
    }
    if y < bottom {
        layout.meta = Rect::new(content.x, y, content.width, 1);
        y += 1;
    }

    let footer_y = bottom.saturating_sub(1);
    if footer_y >= y {
        layout.footer = Rect::new(content.x, footer_y, content.width, FOOTER_ROWS);
    }
    let list_bottom = if layout.footer.height > 0 {
        footer_y
    } else {
        bottom
    };

    if list_bottom.saturating_sub(y) >= MESSAGE_BOX_BUDGET {
        layout.message_box = Rect::new(content.x, y, content.width, MESSAGE_BOX_ROWS);
        layout.message_input =
            Rect::new(content.x + 1, y + 1, content.width.saturating_sub(2), 1);
        y += MESSAGE_BOX_ROWS;
        layout.divider_y = Some(y);
        y += DIVIDER_ROWS;
    }

    if y < list_bottom {
        layout.list = Rect::new(content.x, y, content.width, list_bottom - y);
    }

    layout
}

/// The scrolling row list of an expanded panel.
pub(crate) fn git_sidebar_list_rect(area: Rect) -> Rect {
    git_sidebar_layout(area).list
}

/// The row list minus the scrollbar column.
fn git_sidebar_body_rect(list: Rect, has_scrollbar: bool) -> Rect {
    if list.width == 0 || list.height == 0 {
        return Rect::default();
    }
    let width = if has_scrollbar {
        list.width.saturating_sub(1)
    } else {
        list.width
    };
    if width == 0 {
        return Rect::default();
    }
    Rect::new(list.x, list.y, width, list.height)
}

fn git_sidebar_scrollbar_track(list: Rect) -> Rect {
    if list.width < 2 || list.height == 0 {
        return Rect::default();
    }
    Rect::new(list.x + list.width - 1, list.y, 1, list.height)
}

/// Scroll state of the row list, in the same convention as every other herdr
/// scrollbar: `offset_from_bottom` counts rows hidden *below* the viewport.
pub(crate) fn git_sidebar_scroll_metrics(state: &GitSidebarState, list: Rect) -> ScrollMetrics {
    scroll_metrics_for(state.rows().len(), state.scroll, list)
}

/// `git_sidebar_scroll_metrics` for a row count already in hand, so callers that
/// build the row list anyway do not walk the file groups twice per frame.
fn scroll_metrics_for(total: usize, scroll: usize, list: Rect) -> ScrollMetrics {
    let viewport_rows = list.height as usize;
    let max_offset_from_bottom = total.saturating_sub(viewport_rows);
    ScrollMetrics {
        offset_from_bottom: max_offset_from_bottom.saturating_sub(scroll),
        max_offset_from_bottom,
        viewport_rows,
    }
}

/// The scrollbar track, when the list overflows.
pub(crate) fn git_sidebar_scrollbar_rect(state: &GitSidebarState, area: Rect) -> Option<Rect> {
    let list = git_sidebar_layout(area).list;
    let metrics = git_sidebar_scroll_metrics(state, list);
    if !should_show_scrollbar(metrics) {
        return None;
    }
    let track = git_sidebar_scrollbar_track(list);
    (track.width > 0).then_some(track)
}

/// One rect per visible row, in draw order.
///
/// Called from `compute_view`; the result is what mouse hit-testing consults.
pub(crate) fn compute_git_sidebar_row_areas(
    state: &GitSidebarState,
    area: Rect,
) -> Vec<GitSidebarRowArea> {
    let list = git_sidebar_layout(area).list;
    let rows = state.rows();
    let metrics = scroll_metrics_for(rows.len(), state.scroll, list);
    let body = git_sidebar_body_rect(list, should_show_scrollbar(metrics));
    if body.height == 0 {
        return Vec::new();
    }

    rows.into_iter()
        .enumerate()
        .skip(state.scroll)
        .take(body.height as usize)
        .enumerate()
        .map(|(offset, (index, row))| GitSidebarRowArea {
            index,
            row,
            rect: Rect::new(body.x, body.y + offset as u16, body.width, 1),
        })
        .collect()
}

/// The chevron that closes an expanded panel: bottom row, inner edge.
pub(crate) fn expanded_git_sidebar_toggle_rect(area: Rect) -> Rect {
    if area.width <= 1 || area.height == 0 {
        return Rect::default();
    }
    Rect::new(area.x + 1, area.y + area.height - 1, 1, 1)
}

/// The chevron that reopens a collapsed panel.
pub(crate) fn collapsed_git_sidebar_toggle_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::default();
    }
    let x = if area.width > 1 { area.x + 1 } else { area.x };
    Rect::new(x, area.y + area.height - 1, 1, 1)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn status_color(kind: GitFileStatusKind, p: &Palette) -> Color {
    match kind {
        GitFileStatusKind::Added | GitFileStatusKind::Copied | GitFileStatusKind::Untracked => {
            p.green
        }
        GitFileStatusKind::Modified | GitFileStatusKind::TypeChanged => p.yellow,
        GitFileStatusKind::Deleted => p.red,
        GitFileStatusKind::Renamed => p.blue,
        GitFileStatusKind::Conflicted => p.peach,
    }
}

fn selection_background(p: &Palette) -> Color {
    if p.selection_bg == Color::Reset {
        p.surface0
    } else {
        p.selection_bg
    }
}

pub(super) fn render_git_sidebar(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let p = &app.palette;
    let state = &app.git_sidebar_state;
    let focused = app.mode == Mode::GitSidebar;

    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(p.sidebar_bg));

    // Separator on the inner (left) edge, accented while the panel has focus —
    // the mirror of the left sidebar's outer-edge rule.
    let sep_style = if focused {
        Style::default().fg(p.accent)
    } else {
        Style::default().fg(p.surface_dim)
    };
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buf[(area.x, y)].set_symbol("│");
        buf[(area.x, y)].set_style(sep_style);
    }

    let layout = git_sidebar_layout(area);
    if layout.content.width == 0 {
        return;
    }

    render_title(state, frame, layout.title, p);
    render_meta(state, frame, layout.meta, p);
    render_message_box(app, frame, &layout, p, focused);

    if let Some(divider_y) = layout.divider_y {
        let buf = frame.buffer_mut();
        for x in layout.content.x..layout.content.x + layout.content.width {
            buf[(x, divider_y)].set_symbol("─");
            buf[(x, divider_y)].set_style(Style::default().fg(p.surface_dim));
        }
    }

    render_rows(app, frame, layout.list, p, focused);
    render_footer(app, frame, layout.footer, p);
    render_git_sidebar_toggle(frame, area, false, p);
}

fn render_title(state: &GitSidebarState, frame: &mut Frame, rect: Rect, p: &Palette) {
    if rect.width == 0 {
        return;
    }
    let mut spans = vec![Span::styled(
        " source control",
        Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
    )];
    if state.is_refreshing || state.action_in_flight {
        spans.push(Span::styled(
            " …",
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

fn render_meta(state: &GitSidebarState, frame: &mut Frame, rect: Rect, p: &Palette) {
    if rect.width == 0 {
        return;
    }

    if !state.has_repo() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " no repository",
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            )),
            rect,
        );
        return;
    }

    // Budget: 1 leading space, then repo, branch, and the ahead/behind pair.
    let ahead_behind = match (state.ahead, state.behind) {
        (0, 0) => String::new(),
        (ahead, 0) => format!(" ↑{ahead}"),
        (0, behind) => format!(" ↓{behind}"),
        (ahead, behind) => format!(" ↑{ahead} ↓{behind}"),
    };
    let fixed = 1 + display_width(&ahead_behind);
    let available = (rect.width as usize).saturating_sub(fixed);
    let branch_label = if state.branch.is_empty() {
        String::new()
    } else {
        format!(" ⎇ {}", state.branch)
    };
    // The branch matters more than the repository name; give it the space first.
    let branch_budget = available.min(display_width(&branch_label));
    let repo_budget = available.saturating_sub(branch_budget);

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            truncate_end(&state.repo_name, repo_budget),
            Style::default().fg(p.subtext0),
        ),
        Span::styled(
            truncate_end(&branch_label, branch_budget),
            Style::default().fg(p.mauve),
        ),
    ];
    if state.ahead > 0 {
        spans.push(Span::styled(
            format!(" ↑{}", state.ahead),
            Style::default().fg(p.green),
        ));
    }
    if state.behind > 0 {
        spans.push(Span::styled(
            format!(" ↓{}", state.behind),
            Style::default().fg(p.red),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

fn render_message_box(
    app: &AppState,
    frame: &mut Frame,
    layout: &GitSidebarLayout,
    p: &Palette,
    focused: bool,
) {
    if layout.message_box.width == 0 || layout.message_box.height == 0 {
        return;
    }
    let state = &app.git_sidebar_state;
    let editing = focused && state.focus == GitSidebarFocus::Message;

    let border_style = if editing {
        Style::default().fg(p.accent)
    } else {
        Style::default().fg(p.surface_dim)
    };
    frame.render_widget(
        Block::default().borders(Borders::ALL).style(border_style),
        layout.message_box,
    );

    let input = layout.message_input;
    if input.width == 0 {
        return;
    }
    let visible = input.width as usize;

    if state.commit_message.is_empty() && !editing {
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_end("message (⏎ to commit)", visible),
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            )),
            input,
        );
        return;
    }

    // Slide the window so the caret stays on screen in a long message.
    let cursor_col = state.commit_cursor_display_col();
    let scroll = cursor_col.saturating_sub(visible.saturating_sub(1));
    let text: String = state
        .commit_message
        .chars()
        .skip(scroll)
        .take(visible)
        .collect();
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(p.text))),
        input,
    );

    if editing {
        let caret_x = input.x + (cursor_col - scroll) as u16;
        if caret_x < input.x + input.width {
            frame.buffer_mut()[(caret_x, input.y)]
                .set_style(Style::default().bg(p.accent).fg(p.panel_bg));
        }
    }
}

fn render_rows(app: &AppState, frame: &mut Frame, list: Rect, p: &Palette, focused: bool) {
    if list.width == 0 || list.height == 0 {
        return;
    }
    let state = &app.git_sidebar_state;
    let metrics = git_sidebar_scroll_metrics(state, list);
    let has_scrollbar = should_show_scrollbar(metrics);
    let body = git_sidebar_body_rect(list, has_scrollbar);
    if body.height == 0 {
        return;
    }

    for area in &app.view.git_sidebar_rows {
        let selected = focused && area.index == state.selected && area.row.is_selectable();
        // `Paragraph::style` fills the row, so the cursor highlight rides along
        // with the text instead of needing a separate pass.
        let row_style = if selected {
            Style::default().bg(selection_background(p))
        } else {
            Style::default()
        };
        let line = match area.row {
            GitSidebarRow::SectionHeader(section) => {
                section_header_line(state, section, area.rect.width, p)
            }
            GitSidebarRow::File { section, index } => match state.file_at(section, index) {
                Some(file) => file_line(file, selected, area.rect.width, p),
                None => continue,
            },
            GitSidebarRow::Commit { index } => match state.commits.get(index) {
                Some(commit) => Line::from(vec![
                    Span::raw(" "),
                    Span::styled(commit.hash.clone(), Style::default().fg(p.peach)),
                    Span::raw(" "),
                    Span::styled(
                        truncate_end(
                            &commit.subject,
                            (area.rect.width as usize)
                                .saturating_sub(2 + display_width(&commit.hash)),
                        ),
                        Style::default().fg(p.subtext0),
                    ),
                ]),
                None => continue,
            },
            GitSidebarRow::Placeholder => placeholder_line(state, area.rect.width, p),
        };
        frame.render_widget(Paragraph::new(line).style(row_style), area.rect);
    }

    if has_scrollbar {
        let track = git_sidebar_scrollbar_track(list);
        if track.width > 0 {
            render_scrollbar(frame, metrics, track, p.surface_dim, p.overlay0, "▕");
        }
    }
}

/// A collapsible group header, styled like the left sidebar's " spaces" /
/// " agents" labels with a count on the right.
fn section_header_line(
    state: &GitSidebarState,
    section: GitSection,
    width: u16,
    p: &Palette,
) -> Line<'static> {
    let chevron = if state.is_collapsed(section) {
        "▸"
    } else {
        "▾"
    };
    let count = state.section_len(section).to_string();
    let title = section.title().to_lowercase();
    // " ▾ " + title + padding + count
    let used = 3 + display_width(&count);
    let title = truncate_end(&title, (width as usize).saturating_sub(used));
    let pad = (width as usize).saturating_sub(3 + display_width(&title) + display_width(&count));

    Line::from(vec![
        Span::raw(" "),
        Span::styled(chevron, Style::default().fg(p.accent)),
        Span::raw(" "),
        Span::styled(
            title,
            Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(count, Style::default().fg(p.overlay0)),
    ])
}

/// `" M name  dir"` — status badge, file name, dimmed parent directory.
fn file_line(file: &GitFileStatus, selected: bool, width: u16, p: &Palette) -> Line<'static> {
    let name = file.file_name();
    let dir = file.parent_dir();
    let name_style = if selected {
        Style::default().fg(p.text).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.subtext0)
    };

    // " X " badge plus the name; the directory takes whatever is left.
    let prefix_width = 3;
    let available = (width as usize).saturating_sub(prefix_width);
    let name_budget = available.min(display_width(&name));
    let dir_budget = available
        .saturating_sub(name_budget)
        .saturating_sub(1)
        .min(dir.as_deref().map(display_width).unwrap_or(0));

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            file.status.letter().to_string(),
            Style::default().fg(status_color(file.status, p)),
        ),
        Span::raw(" "),
        Span::styled(truncate_end(&name, name_budget), name_style),
    ];
    if let Some(dir) = dir.filter(|_| dir_budget > 0) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            truncate_end(&dir, dir_budget),
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// The empty/error/loading line that stands in for a group-less list.
fn placeholder_line(state: &GitSidebarState, width: u16, p: &Palette) -> Line<'static> {
    let (text, style) = if !state.has_repo() {
        (
            "not a git repository".to_string(),
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        )
    } else if let Some(error) = &state.error_message {
        (error.clone(), Style::default().fg(p.red))
    } else if !state.loaded {
        (
            "loading…".to_string(),
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        )
    } else {
        (
            "no changes".to_string(),
            Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
        )
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            truncate_end(&text, (width as usize).saturating_sub(1)),
            style,
        ),
    ])
}

/// Bottom row: the change summary, or whatever the last action reported.
fn render_footer(app: &AppState, frame: &mut Frame, rect: Rect, p: &Palette) {
    if rect.width <= 1 {
        return;
    }
    let state = &app.git_sidebar_state;
    // Column 0 belongs to the toggle chevron.
    let text_rect = Rect::new(rect.x + 1, rect.y, rect.width - 1, 1);

    let (text, style) = if let Some(error) = &state.error_message {
        (error.clone(), Style::default().fg(p.red))
    } else if let Some(status) = &state.status_message {
        (status.clone(), Style::default().fg(p.green))
    } else {
        let count = state.change_count();
        let text = match count {
            0 => String::new(),
            1 => "1 change".to_string(),
            n => format!("{n} changes"),
        };
        (text, Style::default().fg(p.overlay0))
    };
    if text.is_empty() {
        return;
    }

    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_end(&text, text_rect.width as usize),
            style,
        ))
        .alignment(Alignment::Right),
        text_rect,
    );
}

/// Compact strip shown when the panel is closed in `compact` mode.
pub(super) fn render_git_sidebar_collapsed(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let p = &app.palette;
    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(p.sidebar_bg));

    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buf[(area.x, y)].set_symbol("│");
        buf[(area.x, y)].set_style(Style::default().fg(p.surface_dim));
    }

    let count = app.git_sidebar_state.change_count();
    if area.width > 1 && count > 0 {
        let label = if count > 99 {
            "99+".to_string()
        } else {
            count.to_string()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default().fg(p.yellow).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
            Rect::new(area.x + 1, area.y, area.width - 1, 1),
        );
    }

    render_git_sidebar_toggle(frame, area, true, p);
}

/// The open/close chevron, mirroring the left sidebar's `«`/`»`.
pub(super) fn render_git_sidebar_toggle(
    frame: &mut Frame,
    area: Rect,
    collapsed: bool,
    p: &Palette,
) {
    let toggle_area = if collapsed {
        collapsed_git_sidebar_toggle_rect(area)
    } else {
        expanded_git_sidebar_toggle_rect(area)
    };
    if toggle_area == Rect::default() {
        return;
    }
    let icon = if collapsed { "«" } else { "»" };
    frame.render_widget(
        Paragraph::new(Span::styled(icon, Style::default().fg(p.overlay0))),
        toggle_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state_with(staged: usize, unstaged: usize) -> GitSidebarState {
        let file = |n: usize| GitFileStatus {
            path: PathBuf::from(format!("src/file{n}.rs")),
            original_path: None,
            status: GitFileStatusKind::Modified,
        };
        GitSidebarState {
            repo_root: Some(PathBuf::from("/repo")),
            repo_name: "repo".to_string(),
            branch: "main".to_string(),
            staged_files: (0..staged).map(file).collect(),
            unstaged_files: (0..unstaged).map(file).collect(),
            loaded: true,
            ..GitSidebarState::default()
        }
    }

    #[test]
    fn layout_reserves_separator_footer_and_message_box() {
        let area = Rect::new(70, 0, 30, 20);
        let layout = git_sidebar_layout(area);
        assert_eq!(layout.content, Rect::new(71, 0, 29, 20));
        assert_eq!(layout.title, Rect::new(71, 0, 29, 1));
        assert_eq!(layout.meta, Rect::new(71, 1, 29, 1));
        assert_eq!(layout.message_box, Rect::new(71, 2, 29, 3));
        assert_eq!(layout.message_input, Rect::new(72, 3, 27, 1));
        assert_eq!(layout.divider_y, Some(5));
        assert_eq!(layout.list, Rect::new(71, 6, 29, 13));
        assert_eq!(layout.footer, Rect::new(71, 19, 29, 1));
    }

    #[test]
    fn layout_drops_the_message_box_in_a_short_panel() {
        let layout = git_sidebar_layout(Rect::new(70, 0, 30, 6));
        assert_eq!(layout.message_box, Rect::default());
        assert_eq!(layout.divider_y, None);
        assert_eq!(layout.list, Rect::new(71, 2, 29, 3));
    }

    #[test]
    fn layout_of_a_one_column_panel_is_empty() {
        assert_eq!(git_sidebar_layout(Rect::new(70, 0, 1, 20)), GitSidebarLayout::default());
    }

    #[test]
    fn row_areas_follow_the_scroll_offset() {
        let mut state = state_with(1, 20);
        let area = Rect::new(70, 0, 30, 20);
        let list = git_sidebar_layout(area).list;
        assert_eq!(list.height, 13);

        let areas = compute_git_sidebar_row_areas(&state, area);
        assert_eq!(areas.len(), 13);
        assert_eq!(areas[0].index, 0);
        assert_eq!(areas[0].rect.y, list.y);
        // 23 rows do not fit in 13, so a scrollbar column is reserved.
        assert_eq!(areas[0].rect.width, list.width - 1);

        state.scroll = 5;
        let areas = compute_git_sidebar_row_areas(&state, area);
        assert_eq!(areas[0].index, 5);
        assert_eq!(areas[0].rect.y, list.y);
    }

    #[test]
    fn scroll_metrics_use_the_shared_bottom_offset_convention() {
        let mut state = state_with(0, 20);
        let list = git_sidebar_layout(Rect::new(70, 0, 30, 20)).list;
        let metrics = git_sidebar_scroll_metrics(&state, list);
        // 21 rows (header + 20 files) in a 13-row viewport.
        assert_eq!(metrics.max_offset_from_bottom, 8);
        assert_eq!(metrics.offset_from_bottom, 8);
        state.scroll = 8;
        let metrics = git_sidebar_scroll_metrics(&state, list);
        assert_eq!(metrics.offset_from_bottom, 0);
    }

    #[test]
    fn toggle_sits_in_the_bottom_inner_corner() {
        let area = Rect::new(70, 0, 30, 20);
        assert_eq!(expanded_git_sidebar_toggle_rect(area), Rect::new(71, 19, 1, 1));
        assert_eq!(
            collapsed_git_sidebar_toggle_rect(Rect::new(96, 0, 4, 20)),
            Rect::new(97, 19, 1, 1)
        );
    }
}
