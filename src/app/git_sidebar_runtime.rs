//! Runtime side of the source-control sidebar: scheduling the background `git`
//! probes, applying their results, driving the dropdowns, and running the
//! panel's mutating commands.
//!
//! State and parsing live in [`crate::app::git_sidebar`]; this file is the only
//! place that touches `App`, spawns work, or reaches for the active workspace.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::git_sidebar::{
    self, build_git_menu, GitMenuAction, GitMenuKind, GitPrompt, GitPromptKind, GitSection,
    GitSidebarAction, GitSidebarFocus, GitSidebarRow, GitSidebarSnapshot, GitSidebarState,
};
use super::popup::PopupGeometry;
use super::state::Mode;
use super::App;
use crate::ui::GitSidebarButton;

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
        // The refs walk and the stash list only change when the user acts, so
        // the three-second poll skips them: two fewer process spawns per tick.
        let full = std::mem::take(&mut self.state.git_sidebar_state.needs_full_refresh);
        self.state.git_sidebar_state.is_refreshing = true;
        // Recorded before the probe returns so a late result for a directory we
        // have since left can be discarded.
        self.state.git_sidebar_state.cwd = Some(cwd.clone());
        git_sidebar::spawn_refresh(cwd, full, self.event_tx.clone());
    }

    /// Re-probe now, including the refs and stash walk.
    pub(crate) fn force_git_sidebar_refresh(&mut self) {
        let state = &mut self.state.git_sidebar_state;
        state.needs_force_refresh = true;
        state.needs_full_refresh = true;
        state.status_message = None;
        state.error_message = None;
        self.start_git_sidebar_refresh_if_due(Instant::now());
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
        // An open dropdown was built from the previous snapshot; rebuild it so a
        // branch or stash list stays live, keeping the user's place in it.
        self.rebuild_open_git_menu();
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
        // A command can change branches, stashes, or the upstream, so the next
        // probe has to be a full one.
        state.needs_full_refresh = true;
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
        let state = &mut self.state.git_sidebar_state;
        state.action_in_flight = true;
        state.pending_discard = None;
        state.error_message = None;
        // A network command can sit for a long time; say so rather than leaving
        // the panel looking frozen behind the spinner in the title.
        state.status_message = action
            .is_network()
            .then(|| "contacting remote...".to_string());
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

    /// Discard the selected file's changes. Requires two presses: the first
    /// arms `pending_discard`, the second runs the command.
    ///
    /// A tracked file is restored from the index; an untracked one is deleted
    /// from disk, which is what VS Code's discard does and which git cannot
    /// undo — hence the same confirmation.
    pub(crate) fn discard_selected_git_sidebar_file(&mut self) {
        let selected = self
            .state
            .git_sidebar_state
            .selected_file()
            .map(|(section, file)| (section, file.path.clone()));
        let Some((section, path)) = selected else {
            return;
        };
        let armed = self.state.git_sidebar_state.pending_discard.as_deref() == Some(path.as_path());
        let untracked = section == GitSection::Untracked;
        if !armed {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            self.state.git_sidebar_state.pending_discard = Some(path);
            self.state.git_sidebar_state.status_message = Some(if untracked {
                format!("press again to delete {name}")
            } else {
                format!("press again to discard {name}")
            });
            return;
        }
        let action = if untracked {
            GitSidebarAction::Clean(path)
        } else {
            GitSidebarAction::Discard(path)
        };
        self.run_git_sidebar_action(action);
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
        let state = &mut self.state.git_sidebar_state;
        state.pending_discard = None;
        state.menu = None;
        state.prompt = None;
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
// Buttons, dropdowns, and prompts
// ---------------------------------------------------------------------------

impl App {
    /// Run the action bar button that was clicked or invoked by key.
    pub(crate) fn activate_git_sidebar_button(&mut self, button: GitSidebarButton) {
        match button {
            GitSidebarButton::Commit => self.commit_from_git_sidebar(),
            GitSidebarButton::Pull => self.run_git_sidebar_action(GitSidebarAction::Pull),
            GitSidebarButton::Push => {
                // A branch with no upstream has nothing to push to; publish it.
                let state = &self.state.git_sidebar_state;
                let action = if state.upstream.is_some() {
                    GitSidebarAction::Push
                } else {
                    GitSidebarAction::PushSetUpstream(state.default_remote.clone())
                };
                self.run_git_sidebar_action(action);
            }
            GitSidebarButton::Branch => self.toggle_git_sidebar_menu(GitMenuKind::Branch),
            GitSidebarButton::Stash => self.toggle_git_sidebar_menu(GitMenuKind::Stash),
            GitSidebarButton::More => self.toggle_git_sidebar_menu(GitMenuKind::More),
        }
    }

    /// Open `kind`, or close the menu when that same one is already open — the
    /// way clicking a button twice behaves everywhere else.
    pub(crate) fn toggle_git_sidebar_menu(&mut self, kind: GitMenuKind) {
        let already_open = self
            .state
            .git_sidebar_state
            .menu
            .as_ref()
            .is_some_and(|menu| menu.kind == kind);
        if already_open {
            self.close_git_sidebar_menu();
            return;
        }
        self.open_git_sidebar_menu(kind);
    }

    pub(crate) fn open_git_sidebar_menu(&mut self, kind: GitMenuKind) {
        self.state.mode = Mode::GitSidebar;
        self.state.git_sidebar_state.prompt = None;
        self.state.git_sidebar_state.pending_discard = None;
        let menu = build_git_menu(kind, &self.state.git_sidebar_state);
        self.state.git_sidebar_state.menu = Some(menu);
        // Branch and stash lists come from the full probe; ask for a fresh one
        // so a menu opened right after an external `git` call is not stale.
        if matches!(
            kind,
            GitMenuKind::Branch | GitMenuKind::Branches(_) | GitMenuKind::Stash
        ) {
            self.state.git_sidebar_state.needs_full_refresh = true;
            self.state.git_sidebar_state.needs_force_refresh = true;
        }
    }

    pub(crate) fn close_git_sidebar_menu(&mut self) {
        self.state.git_sidebar_state.menu = None;
    }

    /// Go back to the menu this one was opened from, or close it.
    fn git_sidebar_menu_back(&mut self) {
        let parent = self
            .state
            .git_sidebar_state
            .menu
            .as_ref()
            .and_then(|menu| menu.parent);
        match parent {
            Some(kind) => self.open_git_sidebar_menu(kind),
            None => self.close_git_sidebar_menu(),
        }
    }

    /// Rebuild the open dropdown against the latest snapshot, keeping the
    /// filter and, as far as the new contents allow, the cursor.
    fn rebuild_open_git_menu(&mut self) {
        let Some(existing) = self.state.git_sidebar_state.menu.as_ref() else {
            return;
        };
        let (kind, filter, selected, scroll) = (
            existing.kind,
            existing.filter.clone(),
            existing.selected,
            existing.scroll,
        );
        let mut menu = build_git_menu(kind, &self.state.git_sidebar_state);
        menu.filter = filter;
        menu.selected = selected;
        menu.scroll = scroll;
        menu.clamp_selection();
        // The rebuilt list can be shorter than the old one; keep at least one
        // row on screen until the next key event scrolls properly.
        menu.scroll = menu.scroll.min(menu.visible().len().saturating_sub(1));
        self.state.git_sidebar_state.menu = Some(menu);
    }

    /// Run the highlighted dropdown row. A danger row needs a second press.
    pub(crate) fn activate_git_sidebar_menu_item(&mut self) {
        let Some(menu) = self.state.git_sidebar_state.menu.as_ref() else {
            return;
        };
        let Some(index) = menu.selected_item_index() else {
            return;
        };
        let Some(item) = menu.items.get(index) else {
            return;
        };
        let danger = item.danger;
        let armed = menu.armed == Some(index);
        let label = item.label.clone();
        let action = item.action.clone();
        let Some(action) = action else {
            return;
        };

        if danger && !armed {
            if let Some(menu) = self.state.git_sidebar_state.menu.as_mut() {
                menu.armed = Some(index);
            }
            self.state.git_sidebar_state.status_message =
                Some(format!("press enter again: {}", label.to_lowercase()));
            return;
        }

        match action {
            GitMenuAction::Open(kind) => self.open_git_sidebar_menu(kind),
            GitMenuAction::Ask(kind) => {
                self.close_git_sidebar_menu();
                self.open_git_sidebar_prompt(kind);
            }
            GitMenuAction::Run(action) => {
                self.close_git_sidebar_menu();
                self.run_git_sidebar_action(action);
            }
            GitMenuAction::Popup(command) => {
                self.close_git_sidebar_menu();
                self.open_git_sidebar_popup(&command);
            }
            GitMenuAction::Refresh => {
                self.close_git_sidebar_menu();
                self.force_git_sidebar_refresh();
            }
            GitMenuAction::FocusCommitMessage => {
                self.close_git_sidebar_menu();
                self.state.git_sidebar_state.focus_message();
            }
        }
    }

    pub(crate) fn open_git_sidebar_prompt(&mut self, kind: GitPromptKind) {
        self.state.mode = Mode::GitSidebar;
        self.state.git_sidebar_state.menu = None;
        self.state.git_sidebar_state.error_message = None;
        self.state.git_sidebar_state.prompt = Some(GitPrompt::new(kind));
    }

    fn submit_git_sidebar_prompt(&mut self) {
        let Some(prompt) = self.state.git_sidebar_state.prompt.take() else {
            return;
        };
        let value = prompt.input.trim().to_string();
        if value.is_empty() && !prompt.kind.allows_empty() {
            self.state.git_sidebar_state.error_message =
                Some(format!("{} is empty", prompt.kind.label()));
            return;
        }

        let action = match prompt.kind {
            GitPromptKind::NewBranch => {
                if let Err(err) = git_sidebar::validate_branch_name(&value) {
                    self.state.git_sidebar_state.error_message = Some(err);
                    return;
                }
                GitSidebarAction::CreateBranch(value)
            }
            GitPromptKind::StashMessage { include_untracked } => GitSidebarAction::StashPush {
                include_untracked,
                message: (!value.is_empty()).then_some(value),
            },
        };
        self.run_git_sidebar_action(action);
    }

    /// Open the dropdown for the row under the cursor: commit actions on a
    /// commit, otherwise the general action menu.
    pub(crate) fn open_git_sidebar_row_menu(&mut self) {
        let kind = match self.state.git_sidebar_state.selected_row() {
            Some(GitSidebarRow::Commit { index }) => GitMenuKind::Commit(index),
            _ => GitMenuKind::More,
        };
        self.open_git_sidebar_menu(kind);
    }
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

impl App {
    /// Handle a key while the source-control panel has focus.
    ///
    /// A dropdown or a prompt takes the keyboard whole while it is open. With
    /// neither up there are two sub-modes, switched with Tab: the commit box
    /// takes text, the list takes commands. The bindings follow VS Code's
    /// Source Control view where there is an equivalent.
    pub(crate) fn handle_git_sidebar_key(&mut self, key: KeyEvent) {
        if self.state.git_sidebar_state.menu.is_some() {
            self.handle_git_sidebar_menu_key(key);
            return;
        }
        if self.state.git_sidebar_state.prompt.is_some() {
            self.handle_git_sidebar_prompt_key(key);
            return;
        }

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
            KeyCode::Char('r') if !ctrl => self.force_git_sidebar_refresh(),

            // Dropdowns, mirroring the action bar buttons.
            KeyCode::Char('b') if !ctrl => self.toggle_git_sidebar_menu(GitMenuKind::Branch),
            KeyCode::Char('S') => self.toggle_git_sidebar_menu(GitMenuKind::Stash),
            KeyCode::Char('m') if !ctrl => self.toggle_git_sidebar_menu(GitMenuKind::More),
            KeyCode::Char('.') => self.open_git_sidebar_row_menu(),

            // Remote operations, straight from the list.
            KeyCode::Char('f') if !ctrl => self.run_git_sidebar_action(GitSidebarAction::Fetch),
            KeyCode::Char('p') if !ctrl => self.activate_git_sidebar_button(GitSidebarButton::Pull),
            KeyCode::Char('P') => self.activate_git_sidebar_button(GitSidebarButton::Push),
            _ => {}
        }

        // Any key other than the discard confirmation cancels the arming.
        if key.code != KeyCode::Char('D') {
            self.state.git_sidebar_state.pending_discard = None;
        }
    }

    fn handle_git_sidebar_menu_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Paging follows the height the dropdown actually got on screen.
        let viewport = self
            .state
            .git_sidebar_state
            .menu
            .as_ref()
            .and_then(|menu| crate::ui::git_menu_layout(self.state.view.git_sidebar_rect, menu))
            .map(|layout| layout.list.height as usize)
            .unwrap_or(0);

        let mut close = false;
        let mut activate = false;
        let mut back = false;
        {
            let Some(menu) = self.state.git_sidebar_state.menu.as_mut() else {
                return;
            };
            let page = viewport.max(1) as isize;
            match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Enter => activate = true,
                KeyCode::Up => menu.move_selection(-1),
                KeyCode::Down => menu.move_selection(1),
                KeyCode::PageUp => menu.move_selection(-page),
                KeyCode::PageDown => menu.move_selection(page),
                KeyCode::Home => {
                    menu.selected = 0;
                    menu.clamp_selection();
                }
                KeyCode::End => {
                    menu.selected = menu.visible().len().saturating_sub(1);
                    menu.clamp_selection();
                }
                KeyCode::Char('n') if ctrl => menu.move_selection(1),
                KeyCode::Char('p') if ctrl => menu.move_selection(-1),
                // A filterable list spends its letters on the filter, so only a
                // plain menu gets the vim pair.
                KeyCode::Char('k') if !ctrl && !menu.filterable => menu.move_selection(-1),
                KeyCode::Char('j') if !ctrl && !menu.filterable => menu.move_selection(1),
                KeyCode::Char(c) if !ctrl => menu.push_filter(c),
                // Backspace edits the filter first and only then walks back.
                KeyCode::Backspace => back = !menu.pop_filter(),
                KeyCode::Left => back = true,
                KeyCode::Tab | KeyCode::BackTab => close = true,
                _ => {}
            }
            menu.scroll_into_view(viewport);
        }

        if close {
            self.close_git_sidebar_menu();
            return;
        }
        if back {
            self.git_sidebar_menu_back();
            return;
        }
        if activate {
            self.activate_git_sidebar_menu_item();
        }
    }

    fn handle_git_sidebar_prompt_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.state.git_sidebar_state.prompt = None;
                return;
            }
            KeyCode::Enter => {
                self.submit_git_sidebar_prompt();
                return;
            }
            _ => {}
        }

        let Some(prompt) = self.state.git_sidebar_state.prompt.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => prompt.insert(&c.to_string()),
            KeyCode::Backspace => prompt.backspace(),
            KeyCode::Delete => prompt.delete_char(),
            KeyCode::Left => prompt.move_cursor(-1),
            KeyCode::Right => prompt.move_cursor(1),
            KeyCode::Home => prompt.cursor = 0,
            KeyCode::End => prompt.cursor = prompt.input.len(),
            _ => {}
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
