//! Runtime side of the source-control sidebar: scheduling the background `git`
//! probes, applying their results, and running the panel's mutating commands.
//!
//! State and parsing live in [`crate::app::git_sidebar`]; this file is the only
//! place that touches `App`, spawns work, or reaches for the active workspace.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::git_sidebar::{
    self, GitSection, GitSidebarAction, GitSidebarFocus, GitSidebarRow, GitSidebarSnapshot,
    GitSidebarState,
};
use super::popup::PopupGeometry;
use super::state::Mode;
use super::App;

/// How often the panel re-probes the repository while it is open.
///
/// Herdr has no filesystem watcher, so this is a poll. It only runs while the
/// panel is visible and never overlaps itself.
const GIT_SIDEBAR_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

impl App {
    /// The working directory the panel should describe: the active workspace's
    /// resolved cwd, which follows the focused pane.
    fn git_sidebar_cwd(&self) -> Option<PathBuf> {
        let ws = self
            .state
            .active
            .and_then(|idx| self.state.workspaces.get(idx))?;
        ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
    }

    /// Where mutating commands run. Falls back to the resolved cwd before the
    /// first probe has identified a repository root.
    fn git_sidebar_repo_root(&self) -> Option<PathBuf> {
        self.state
            .git_sidebar_state
            .repo_root
            .clone()
            .or_else(|| self.git_sidebar_cwd())
    }

    /// When the panel next wants a probe, so the event loop wakes up for it
    /// instead of sitting idle until the next keystroke.
    pub(crate) fn git_sidebar_refresh_deadline(&self) -> Option<Instant> {
        let state = &self.state.git_sidebar_state;
        if self.state.git_sidebar_closed || state.is_refreshing {
            return None;
        }
        if state.needs_force_refresh {
            return Some(Instant::now());
        }
        Some(state.last_refresh? + GIT_SIDEBAR_REFRESH_INTERVAL)
    }

    /// Poll hook, called once per runtime tick.
    ///
    /// Spawns at most one probe and does nothing at all while the panel is
    /// closed, so a hidden panel costs one `bool` check per tick.
    pub(crate) fn start_git_sidebar_refresh_if_due(&mut self, now: Instant) {
        if self.state.git_sidebar_closed {
            return;
        }

        let cwd = self.git_sidebar_cwd();

        // A workspace switch moves the panel to a different repository. Drop the
        // old contents at once rather than showing another repository's changes
        // until the probe lands.
        if cwd.is_some() && self.state.git_sidebar_state.cwd.as_deref() != cwd.as_deref() {
            self.state.git_sidebar_state.reset_for_new_repo();
            self.state.git_sidebar_state.needs_force_refresh = true;
        }

        if self.state.git_sidebar_state.is_refreshing {
            return;
        }

        let force = self.state.git_sidebar_state.needs_force_refresh;
        let due = self.state.git_sidebar_state.last_refresh.is_none_or(|last| {
            now.saturating_duration_since(last) >= GIT_SIDEBAR_REFRESH_INTERVAL
        });
        if !force && !due {
            return;
        }

        // Claim the slot before the early return below: a workspace with no
        // resolvable cwd must not leave an always-due deadline behind, or the
        // event loop would spin on it.
        self.state.git_sidebar_state.needs_force_refresh = false;
        self.state.git_sidebar_state.last_refresh = Some(now);

        let Some(cwd) = cwd else {
            return;
        };
        self.state.git_sidebar_state.is_refreshing = true;
        // Recorded before the probe returns so a late result for a directory we
        // have since left can be discarded.
        self.state.git_sidebar_state.cwd = Some(cwd.clone());
        git_sidebar::spawn_refresh(cwd, self.event_tx.clone());
    }

    /// Adopt a finished probe. Returns whether the view changed.
    pub(crate) fn handle_git_sidebar_refresh_complete(
        &mut self,
        snapshot: GitSidebarSnapshot,
    ) -> bool {
        let state = &mut self.state.git_sidebar_state;
        state.is_refreshing = false;
        if state.cwd.as_deref() != Some(snapshot.cwd.as_path()) {
            // The panel moved to another repository while this probe ran.
            return false;
        }
        state.apply_snapshot(snapshot);
        true
    }

    /// Record the outcome of a mutating command and re-probe.
    pub(crate) fn handle_git_sidebar_action_complete(
        &mut self,
        error: Option<String>,
        message: Option<String>,
    ) -> bool {
        let state = &mut self.state.git_sidebar_state;
        state.action_in_flight = false;
        state.needs_force_refresh = true;
        if error.is_some() {
            state.error_message = error;
            state.status_message = None;
        } else {
            state.error_message = None;
            state.status_message = message;
        }
        true
    }

    /// Queue one mutating git command. Ignored while another is in flight so a
    /// held-down key cannot stack `git add` calls on the same index lock.
    pub(crate) fn run_git_sidebar_action(&mut self, action: GitSidebarAction) {
        if self.state.git_sidebar_state.action_in_flight {
            return;
        }
        let Some(repo_root) = self.git_sidebar_repo_root() else {
            return;
        };
        self.state.git_sidebar_state.action_in_flight = true;
        self.state.git_sidebar_state.status_message = None;
        self.state.git_sidebar_state.pending_discard = None;
        git_sidebar::spawn_action(repo_root, action, self.event_tx.clone());
    }

    /// Stage or unstage the file under the cursor, depending on its group.
    pub(crate) fn toggle_git_sidebar_stage(&mut self) {
        let Some((section, file)) = self.state.git_sidebar_state.selected_file() else {
            return;
        };
        let action = if section.is_staged() {
            GitSidebarAction::Unstage(file.path.clone())
        } else {
            GitSidebarAction::Stage(file.path.clone())
        };
        self.run_git_sidebar_action(action);
    }

    /// Commit the message in the box. Refuses an empty message or an empty index.
    pub(crate) fn commit_from_git_sidebar(&mut self) {
        let state = &mut self.state.git_sidebar_state;
        let message = state.commit_message.trim().to_string();
        if message.is_empty() {
            state.error_message = Some("commit message is empty".to_string());
            return;
        }
        if state.staged_files.is_empty() && state.merge_files.is_empty() {
            state.error_message = Some("nothing staged to commit".to_string());
            return;
        }
        state.clear_commit_message();
        state.error_message = None;
        self.run_git_sidebar_action(GitSidebarAction::Commit(message));
    }

    /// Discard the worktree changes of the selected file. Requires two presses:
    /// the first arms `pending_discard`, the second runs the restore.
    pub(crate) fn discard_selected_git_sidebar_file(&mut self) {
        let selected = self
            .state
            .git_sidebar_state
            .selected_file()
            .map(|(section, file)| (section, file.path.clone()));
        let Some((section, path)) = selected else {
            return;
        };
        if section == GitSection::Untracked {
            // Deleting an untracked file is not a git operation; leave it alone
            // rather than removing something git cannot restore.
            self.state.git_sidebar_state.error_message =
                Some("cannot discard an untracked file".to_string());
            return;
        }
        let armed = self.state.git_sidebar_state.pending_discard.as_deref() == Some(path.as_path());
        if !armed {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            self.state.git_sidebar_state.pending_discard = Some(path);
            self.state.git_sidebar_state.status_message =
                Some(format!("press D again to discard {name}"));
            return;
        }
        self.run_git_sidebar_action(GitSidebarAction::Discard(path));
    }

    /// Show the diff of the row under the cursor in a popup pane.
    pub(crate) fn open_git_sidebar_diff(&mut self) {
        let Some(row) = self.state.git_sidebar_state.selected_row() else {
            return;
        };
        let command = match row {
            GitSidebarRow::File { section, index } => self
                .state
                .git_sidebar_state
                .file_at(section, index)
                .map(|file| git_sidebar::diff_command(section, file)),
            GitSidebarRow::Commit { index } => self
                .state
                .git_sidebar_state
                .commits
                .get(index)
                .map(git_sidebar::show_command),
            _ => None,
        };
        let Some(command) = command else {
            return;
        };
        self.open_git_sidebar_popup(&command);
    }

    /// Run one read-only git command in a popup pane rooted at the repository.
    pub(crate) fn open_git_sidebar_popup(&mut self, command: &str) {
        let Some(repo_root) = self.git_sidebar_repo_root() else {
            return;
        };
        let spawned = self.spawn_popup_shell_command(
            command,
            Some(repo_root),
            Vec::new(),
            PopupGeometry::default(),
        );
        if let Err(err) = spawned {
            self.state.git_sidebar_state.error_message = Some(err.to_string());
        }
    }

    /// Hand the keyboard back to the terminal, leaving the panel visible.
    pub(crate) fn blur_git_sidebar(&mut self) {
        if self.state.mode == Mode::GitSidebar {
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
        }
        self.state.git_sidebar_state.pending_discard = None;
    }

    /// Toggle panel visibility, as the `toggle_git_sidebar` keybind does.
    pub(crate) fn toggle_git_sidebar(&mut self) {
        self.state.toggle_git_sidebar_visibility();
        self.start_git_sidebar_refresh_if_due(Instant::now());
    }
}

impl GitSidebarState {
    /// Focus the commit box, placing the caret at the end of the message.
    pub fn focus_message(&mut self) {
        self.focus = GitSidebarFocus::Message;
        self.commit_cursor = self.commit_message.len();
    }

    pub fn focus_list(&mut self) {
        self.focus = GitSidebarFocus::List;
    }
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

impl App {
    /// Handle a key while the source-control panel has focus.
    ///
    /// Two sub-modes, switched with Tab: the commit box takes text, the list
    /// takes commands. The bindings follow VS Code's Source Control view where
    /// there is an equivalent.
    pub(crate) fn handle_git_sidebar_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Bindings that work from either sub-mode.
        match key.code {
            KeyCode::Esc => {
                self.blur_git_sidebar();
                if self.state.git_sidebar_escape_to_dismiss {
                    self.state.git_sidebar_closed = true;
                    self.state.mark_session_dirty();
                }
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if self.state.git_sidebar_state.focus == GitSidebarFocus::Message {
                    self.state.git_sidebar_state.focus_list();
                } else {
                    self.state.git_sidebar_state.focus_message();
                }
                return;
            }
            KeyCode::Enter if ctrl => {
                self.commit_from_git_sidebar();
                return;
            }
            _ => {}
        }

        if self.state.git_sidebar_state.focus == GitSidebarFocus::Message {
            self.handle_git_sidebar_message_key(key, ctrl, alt);
        } else {
            self.handle_git_sidebar_list_key(key, ctrl);
        }
    }

    fn handle_git_sidebar_message_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool) {
        if key.code == KeyCode::Enter {
            self.commit_from_git_sidebar();
            return;
        }
        let state = &mut self.state.git_sidebar_state;
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => state.insert_commit_text(&c.to_string()),
            KeyCode::Backspace => state.backspace_commit(),
            KeyCode::Delete => state.delete_commit_char(),
            KeyCode::Left => state.move_commit_cursor(-1),
            KeyCode::Right => state.move_commit_cursor(1),
            KeyCode::Home => state.commit_cursor = 0,
            KeyCode::End => state.commit_cursor = state.commit_message.len(),
            // Vertical movement leaves the one-line box for the list.
            KeyCode::Down => {
                state.focus_list();
            }
            _ => {}
        }
    }

    fn handle_git_sidebar_list_key(&mut self, key: KeyEvent, ctrl: bool) {
        let list = crate::ui::git_sidebar_list_rect(self.state.view.git_sidebar_rect);
        let page = list.height.max(1) as isize;

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.state.git_sidebar_state.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.state.git_sidebar_state.move_selection(1),
            KeyCode::PageUp => self.state.git_sidebar_state.move_selection(-page),
            KeyCode::PageDown => self.state.git_sidebar_state.move_selection(page),
            KeyCode::Home | KeyCode::Char('g') => self.state.git_sidebar_state.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.state.git_sidebar_state.select_last(),

            // Collapse / expand the group under (or owning) the cursor.
            KeyCode::Left | KeyCode::Char('h') => self.set_selected_git_section_collapsed(true),
            KeyCode::Right | KeyCode::Char('l') => self.set_selected_git_section_collapsed(false),

            KeyCode::Char(' ') | KeyCode::Char('s') => {
                let row = self.state.git_sidebar_state.selected_row();
                if let Some(GitSidebarRow::SectionHeader(section)) = row {
                    self.state.git_sidebar_state.toggle_collapsed(section);
                } else {
                    self.toggle_git_sidebar_stage();
                }
            }
            KeyCode::Enter => self.open_git_sidebar_diff(),

            KeyCode::Char('a') if !ctrl => {
                self.run_git_sidebar_action(GitSidebarAction::StageAll)
            }
            KeyCode::Char('u') if !ctrl => {
                self.run_git_sidebar_action(GitSidebarAction::UnstageAll)
            }
            KeyCode::Char('c') if !ctrl => self.state.git_sidebar_state.focus_message(),
            KeyCode::Char('D') => self.discard_selected_git_sidebar_file(),
            KeyCode::Char('r') if !ctrl => {
                self.state.git_sidebar_state.needs_force_refresh = true;
                self.state.git_sidebar_state.status_message = None;
                self.state.git_sidebar_state.error_message = None;
                self.start_git_sidebar_refresh_if_due(Instant::now());
            }
            _ => {}
        }

        // Any key other than the discard confirmation cancels the arming.
        if key.code != KeyCode::Char('D') {
            self.state.git_sidebar_state.pending_discard = None;
        }
    }

    /// Collapse or expand the group the cursor sits in, moving the cursor to
    /// that group's header when collapsing from inside it.
    fn set_selected_git_section_collapsed(&mut self, collapsed: bool) {
        let Some(row) = self.state.git_sidebar_state.selected_row() else {
            return;
        };
        let section = match row {
            GitSidebarRow::SectionHeader(section) => section,
            GitSidebarRow::File { section, .. } => section,
            GitSidebarRow::Commit { .. } => GitSection::Commits,
            GitSidebarRow::Placeholder => return,
        };
        let was_collapsed = self.state.git_sidebar_state.is_collapsed(section);
        self.state.git_sidebar_state.set_collapsed(section, collapsed);
        if collapsed && !was_collapsed {
            let header = self
                .state
                .git_sidebar_state
                .rows()
                .iter()
                .position(|row| *row == GitSidebarRow::SectionHeader(section));
            if let Some(index) = header {
                self.state.git_sidebar_state.select_index(index);
            }
        }
    }
}
