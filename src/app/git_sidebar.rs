//! Source-control sidebar state.
//!
//! Mirrors the VS Code "Source Control" view: a commit message box plus
//! collapsible groups of merge/staged/unstaged/untracked files and a recent
//! commit list, all scoped to the repository owning the active workspace's
//! working directory.
//!
//! This module is pure data plus the background `git` probes that fill it. It
//! never touches the terminal, the view, or the renderer: geometry lives in
//! `crate::ui::git_sidebar` and hit-testing reads `ViewState::git_sidebar_rows`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::events::AppEvent;

/// How many commits the recent-commit group loads.
const RECENT_COMMIT_LIMIT: usize = 30;
/// Field separator used in `git log --pretty`; never appears in git output.
const LOG_FIELD_SEP: char = '\u{1f}';
/// How many refs the branch menu lists, newest first.
const BRANCH_LIMIT: usize = 300;
/// Remote used for publishing when the repository has one by this name.
const DEFAULT_REMOTE: &str = "origin";

/// One group in the source-control list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSection {
    /// Unmerged paths (conflicts).
    Merge,
    /// Files with index changes.
    Staged,
    /// Tracked files with worktree changes.
    Changes,
    /// Untracked paths.
    Untracked,
    /// Recent commits on the current branch.
    Commits,
}

impl GitSection {
    pub fn title(self) -> &'static str {
        match self {
            GitSection::Merge => "Merge Changes",
            GitSection::Staged => "Staged Changes",
            GitSection::Changes => "Changes",
            GitSection::Untracked => "Untracked",
            GitSection::Commits => "Commits",
        }
    }

    /// Whether files in this group live in the index (so the stage toggle
    /// unstages instead of staging).
    pub fn is_staged(self) -> bool {
        matches!(self, GitSection::Staged)
    }
}

/// Porcelain status letter for one path within one group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileStatusKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl GitFileStatusKind {
    /// Single-letter badge, matching git's own porcelain letters.
    pub fn letter(self) -> &'static str {
        match self {
            GitFileStatusKind::Added => "A",
            GitFileStatusKind::Modified => "M",
            GitFileStatusKind::Deleted => "D",
            GitFileStatusKind::Renamed => "R",
            GitFileStatusKind::Copied => "C",
            GitFileStatusKind::TypeChanged => "T",
            GitFileStatusKind::Untracked => "U",
            GitFileStatusKind::Conflicted => "!",
        }
    }

    fn from_porcelain(code: char) -> Self {
        match code {
            'A' => GitFileStatusKind::Added,
            'D' => GitFileStatusKind::Deleted,
            'R' => GitFileStatusKind::Renamed,
            'C' => GitFileStatusKind::Copied,
            'T' => GitFileStatusKind::TypeChanged,
            'U' => GitFileStatusKind::Conflicted,
            '?' => GitFileStatusKind::Untracked,
            _ => GitFileStatusKind::Modified,
        }
    }
}

/// One changed path in one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileStatus {
    /// Repository-relative path, as git reports it.
    pub path: PathBuf,
    /// Previous path for renames and copies.
    pub original_path: Option<PathBuf>,
    pub status: GitFileStatusKind,
}

impl GitFileStatus {
    /// Path as a display string with forward slashes, the way git prints it.
    pub fn display_path(&self) -> String {
        self.path.to_string_lossy().replace('\\', "/")
    }

    /// Trailing file name, used as the primary label.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.display_path())
    }

    /// Parent directory, used as the dimmed secondary label.
    pub fn parent_dir(&self) -> Option<String> {
        let parent = self.path.parent()?;
        if parent.as_os_str().is_empty() {
            return None;
        }
        Some(parent.to_string_lossy().replace('\\', "/"))
    }
}

/// One entry in the recent-commit group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitEntry {
    pub hash: String,
    pub subject: String,
    pub author: String,
    pub relative_time: String,
}

/// One ref the repository knows about, local or remote-tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranchEntry {
    /// Short name: `main`, or `origin/main` for a remote-tracking branch.
    pub name: String,
    /// Upstream of a local branch, when it has one.
    pub upstream: Option<String>,
    pub is_remote: bool,
    /// The branch `HEAD` currently points at.
    pub is_current: bool,
}

impl GitBranchEntry {
    /// The name to hand `git checkout`: a remote-tracking branch is checked out
    /// by its short name so git's DWIM creates the local tracking branch.
    pub fn checkout_name(&self) -> &str {
        if self.is_remote {
            self.name
                .split_once('/')
                .map_or(self.name.as_str(), |(_, rest)| rest)
        } else {
            &self.name
        }
    }
}

/// One entry of `git stash list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStashEntry {
    /// Position in the stash stack; `0` is the most recent.
    pub index: usize,
    pub message: String,
    /// Branch the stash was taken on, when git recorded one.
    pub branch: Option<String>,
}

impl GitStashEntry {
    /// `stash@{N}`, the revision the stash subcommands take.
    pub fn reference(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }
}

/// A single-line question asked in place of the commit box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPromptKind {
    NewBranch,
    StashMessage { include_untracked: bool },
}

impl GitPromptKind {
    pub fn label(self) -> &'static str {
        match self {
            GitPromptKind::NewBranch => "new branch name",
            GitPromptKind::StashMessage { .. } => "stash message (optional)",
        }
    }

    /// Whether submitting an empty answer means anything.
    pub fn allows_empty(self) -> bool {
        matches!(self, GitPromptKind::StashMessage { .. })
    }
}

/// An open question and the answer typed so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPrompt {
    pub kind: GitPromptKind,
    pub input: String,
    /// Byte offset of the caret inside `input`.
    pub cursor: usize,
}

impl GitPrompt {
    pub fn new(kind: GitPromptKind) -> Self {
        Self {
            kind,
            input: String::new(),
            cursor: 0,
        }
    }

    pub fn insert(&mut self, value: &str) {
        insert_text(&mut self.input, &mut self.cursor, value);
    }

    pub fn backspace(&mut self) {
        backspace_text(&mut self.input, &mut self.cursor);
    }

    pub fn delete_char(&mut self) {
        delete_text_char(&mut self.input, &mut self.cursor);
    }

    pub fn move_cursor(&mut self, delta: isize) {
        move_text_cursor(&self.input, &mut self.cursor, delta);
    }

    pub fn cursor_display_col(&self) -> usize {
        text_cursor_display_col(&self.input, self.cursor)
    }
}

/// What a branch list is being picked *for*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchPurpose {
    Checkout,
    Merge,
    Rebase,
    Delete,
}

impl BranchPurpose {
    fn title(self) -> &'static str {
        match self {
            BranchPurpose::Checkout => "checkout branch",
            BranchPurpose::Merge => "merge into HEAD",
            BranchPurpose::Rebase => "rebase HEAD onto",
            BranchPurpose::Delete => "delete branch",
        }
    }
}

/// Which dropdown is open over the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitMenuKind {
    /// The `...` button: everything that is not a file operation.
    More,
    /// The branch button: branch commands and the branch-list entry points.
    Branch,
    /// A filterable branch list.
    Branches(BranchPurpose),
    /// The stash button: stash commands and the stash list.
    Stash,
    /// Apply / pop / drop for one stash entry.
    StashEntry(usize),
    /// Actions on one commit row.
    Commit(usize),
}

/// What activating a menu row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitMenuAction {
    /// Run a mutating git command.
    Run(GitSidebarAction),
    /// Replace this menu with another one.
    Open(GitMenuKind),
    /// Ask for a line of text, then run the command it parametrises.
    Ask(GitPromptKind),
    /// Run a read-only git command in a popup pane.
    Popup(String),
    /// Re-probe the repository now.
    Refresh,
    /// Close the menu and put the caret in the commit box.
    FocusCommitMessage,
}

/// One row of a dropdown. An item with no action is a non-selectable rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMenuItem {
    pub label: String,
    /// Dimmed right-hand annotation (upstream, branch, relative time).
    pub detail: Option<String>,
    pub action: Option<GitMenuAction>,
    /// Drawn in the danger colour and gated behind a confirming second Enter.
    pub danger: bool,
}

impl GitMenuItem {
    fn new(label: impl Into<String>, action: GitMenuAction) -> Self {
        Self {
            label: label.into(),
            detail: None,
            action: Some(action),
            danger: false,
        }
    }

    fn run(label: impl Into<String>, action: GitSidebarAction) -> Self {
        Self::new(label, GitMenuAction::Run(action))
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if !detail.is_empty() {
            self.detail = Some(detail);
        }
        self
    }

    fn dangerous(mut self) -> Self {
        self.danger = true;
        self
    }

    /// A dimmed, non-selectable line explaining why a list is empty.
    fn notice(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
            action: None,
            danger: false,
        }
    }

    /// A horizontal rule between groups. Rendered as a rule because its label
    /// is empty; a [`GitMenuItem::notice`] is the labelled counterpart.
    fn rule() -> Self {
        Self {
            label: String::new(),
            detail: None,
            action: None,
            danger: false,
        }
    }

    pub fn is_selectable(&self) -> bool {
        self.action.is_some()
    }
}

/// An open dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMenuState {
    pub kind: GitMenuKind,
    pub title: String,
    pub items: Vec<GitMenuItem>,
    /// Whether typing filters the list rather than doing nothing.
    pub filterable: bool,
    pub filter: String,
    /// Index into [`GitMenuState::visible`].
    pub selected: usize,
    /// First visible row of [`GitMenuState::visible`].
    pub scroll: usize,
    /// Menu to return to on Backspace / Left.
    pub parent: Option<GitMenuKind>,
    /// Item index of a danger row awaiting its confirming second Enter.
    pub armed: Option<usize>,
}

impl GitMenuState {
    /// Item indices matching the current filter, in draw order.
    ///
    /// Rules drop out while a filter is active so a filtered list never opens
    /// or closes on one.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|index| {
                let item = &self.items[*index];
                if needle.is_empty() {
                    return true;
                }
                item.is_selectable() && item.label.to_lowercase().contains(&needle)
            })
            .collect()
    }

    pub fn selected_item(&self) -> Option<&GitMenuItem> {
        let visible = self.visible();
        self.items.get(*visible.get(self.selected)?)
    }

    /// Absolute item index under the cursor.
    pub fn selected_item_index(&self) -> Option<usize> {
        self.visible().get(self.selected).copied()
    }

    /// Snap the cursor onto the nearest selectable row.
    pub fn clamp_selection(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected >= visible.len() {
            self.selected = visible.len() - 1;
        }
        if self.items[visible[self.selected]].is_selectable() {
            return;
        }
        let next = (self.selected..visible.len()).find(|i| self.items[visible[*i]].is_selectable());
        let prev = (0..=self.selected)
            .rev()
            .find(|i| self.items[visible[*i]].is_selectable());
        self.selected = next.or(prev).unwrap_or(0);
    }

    /// Move the cursor by `delta` selectable rows.
    pub fn move_selection(&mut self, delta: isize) {
        self.armed = None;
        let visible = self.visible();
        let selectable: Vec<usize> = (0..visible.len())
            .filter(|i| self.items[visible[*i]].is_selectable())
            .collect();
        if selectable.is_empty() {
            self.selected = 0;
            return;
        }
        let current = selectable
            .iter()
            .position(|i| *i >= self.selected)
            .unwrap_or(selectable.len() - 1);
        let next = (current as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
        self.selected = selectable[next];
    }

    /// Select the visible row at `index`, snapping onto a selectable row.
    ///
    /// Re-selecting the row already under the cursor keeps a danger row armed,
    /// so a second click on it confirms the way a second Enter does.
    pub fn select_visible(&mut self, index: usize) {
        if self.selected != index {
            self.armed = None;
        }
        self.selected = index;
        self.clamp_selection();
    }

    /// Keep the cursor inside a `viewport_rows`-high window.
    pub fn scroll_into_view(&mut self, viewport_rows: usize) {
        let total = self.visible().len();
        if viewport_rows == 0 || total == 0 {
            self.scroll = 0;
            return;
        }
        let max_scroll = total.saturating_sub(viewport_rows);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + viewport_rows {
            self.scroll = self.selected + 1 - viewport_rows;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Scroll without moving the cursor, for the wheel.
    pub fn scroll_by(&mut self, delta: isize, viewport_rows: usize) {
        let total = self.visible().len();
        let max_scroll = total.saturating_sub(viewport_rows) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max_scroll.max(0)) as usize;
    }

    pub fn push_filter(&mut self, value: char) {
        if !self.filterable || value.is_control() {
            return;
        }
        self.filter.push(value);
        self.selected = 0;
        self.scroll = 0;
        self.armed = None;
        self.clamp_selection();
    }

    /// Remove the last filter character. Returns whether anything changed, so
    /// the caller can fall back to "go to the parent menu".
    pub fn pop_filter(&mut self) -> bool {
        if !self.filterable || self.filter.is_empty() {
            return false;
        }
        self.filter.pop();
        self.selected = 0;
        self.scroll = 0;
        self.armed = None;
        self.clamp_selection();
        true
    }
}

/// Which part of the panel takes keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSidebarFocus {
    /// The commit message box.
    Message,
    /// The change/commit list.
    List,
}

/// A single rendered, hit-testable line in the source-control list.
///
/// Produced by [`GitSidebarState::rows`] and stored per-frame in
/// `ViewState::git_sidebar_rows` so mouse hit-testing and rendering can never
/// disagree about which line is where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSidebarRow {
    SectionHeader(GitSection),
    File {
        section: GitSection,
        index: usize,
    },
    Commit {
        index: usize,
    },
    /// A non-selectable informational line ("No changes", an error, ...).
    Placeholder,
}

impl GitSidebarRow {
    pub fn is_selectable(self) -> bool {
        !matches!(self, GitSidebarRow::Placeholder)
    }
}

/// Everything one background refresh learned about the repository.
///
/// Sent whole through [`AppEvent::GitSidebarRefreshComplete`] so the main loop
/// swaps state in one move instead of field by field.
#[derive(Debug, Clone, Default)]
pub struct GitSidebarSnapshot {
    /// Directory the probe ran in; used to drop results for a stale workspace.
    pub cwd: PathBuf,
    /// Repository top level, or `None` when `cwd` is not inside a repository.
    pub repo_root: Option<PathBuf>,
    pub repo_name: String,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub merge_files: Vec<GitFileStatus>,
    pub staged_files: Vec<GitFileStatus>,
    pub unstaged_files: Vec<GitFileStatus>,
    pub untracked_files: Vec<GitFileStatus>,
    pub commits: Vec<GitCommitEntry>,
    /// Branch list, or `None` when this probe skipped the expensive refs walk.
    pub branches: Option<Vec<GitBranchEntry>>,
    /// Stash stack, or `None` when this probe skipped it.
    pub stashes: Option<Vec<GitStashEntry>>,
    /// Remote names, or `None` when this probe skipped the refs walk.
    pub remotes: Option<Vec<String>>,
    pub rebase_in_progress: bool,
    pub merge_in_progress: bool,
    pub cherry_pick_in_progress: bool,
    pub error: Option<String>,
}

/// State of the source-control panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSidebarState {
    /// Working directory the current contents describe.
    pub cwd: Option<PathBuf>,
    /// Repository top level; `None` means "not a git repository".
    pub repo_root: Option<PathBuf>,
    pub repo_name: String,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,

    pub merge_files: Vec<GitFileStatus>,
    pub staged_files: Vec<GitFileStatus>,
    pub unstaged_files: Vec<GitFileStatus>,
    pub untracked_files: Vec<GitFileStatus>,
    pub commits: Vec<GitCommitEntry>,
    pub branches: Vec<GitBranchEntry>,
    pub stashes: Vec<GitStashEntry>,
    /// Remote to publish an unpublished branch to; `origin` unless the
    /// repository has no remote by that name.
    pub default_remote: String,
    /// A `git rebase` is stopped part-way through.
    pub rebase_in_progress: bool,
    /// A `git merge` is stopped on conflicts.
    pub merge_in_progress: bool,
    /// A `git cherry-pick` is stopped on conflicts.
    pub cherry_pick_in_progress: bool,

    /// Open dropdown, if any. While this is set it takes the keyboard.
    pub menu: Option<GitMenuState>,
    /// Open question, if any. Replaces the commit box while it is set.
    pub prompt: Option<GitPrompt>,

    pub commit_message: String,
    /// Byte offset of the caret inside `commit_message`.
    pub commit_cursor: usize,

    pub collapsed_merge: bool,
    pub collapsed_staged: bool,
    pub collapsed_changes: bool,
    pub collapsed_untracked: bool,
    pub collapsed_commits: bool,

    pub focus: GitSidebarFocus,
    /// Index into [`GitSidebarState::rows`].
    pub selected: usize,
    /// First visible row index.
    pub scroll: usize,
    /// Set when the cursor moves; consumed by `compute_view` to scroll the
    /// selection back into view exactly once.
    pub follow_selection: bool,

    /// A `git` probe is running.
    pub is_refreshing: bool,
    /// A mutating `git` command is running.
    pub action_in_flight: bool,
    /// Refresh on the next tick regardless of the poll interval.
    pub needs_force_refresh: bool,
    /// Make the next probe a *full* one: branches, stashes, and remotes as well
    /// as status. Cleared once that probe is spawned.
    pub needs_full_refresh: bool,
    /// Path awaiting a second discard keypress.
    pub pending_discard: Option<PathBuf>,

    /// Last `git` failure, shown in place of the list.
    pub error_message: Option<String>,
    /// Transient confirmation line ("committed", "staged all", ...).
    pub status_message: Option<String>,
    pub last_refresh: Option<Instant>,
    /// Whether a refresh has ever completed; drives the initial "Loading" line.
    pub loaded: bool,
}

impl Default for GitSidebarState {
    fn default() -> Self {
        Self {
            cwd: None,
            repo_root: None,
            repo_name: String::new(),
            branch: String::new(),
            upstream: None,
            ahead: 0,
            behind: 0,
            merge_files: Vec::new(),
            staged_files: Vec::new(),
            unstaged_files: Vec::new(),
            untracked_files: Vec::new(),
            commits: Vec::new(),
            branches: Vec::new(),
            stashes: Vec::new(),
            default_remote: DEFAULT_REMOTE.to_string(),
            rebase_in_progress: false,
            merge_in_progress: false,
            cherry_pick_in_progress: false,
            menu: None,
            prompt: None,
            commit_message: String::new(),
            commit_cursor: 0,
            collapsed_merge: false,
            collapsed_staged: false,
            collapsed_changes: false,
            collapsed_untracked: false,
            collapsed_commits: true,
            focus: GitSidebarFocus::List,
            selected: 0,
            scroll: 0,
            follow_selection: true,
            is_refreshing: false,
            action_in_flight: false,
            needs_force_refresh: true,
            needs_full_refresh: true,
            pending_discard: None,
            error_message: None,
            status_message: None,
            last_refresh: None,
            loaded: false,
        }
    }
}

impl GitSidebarState {
    pub fn files(&self, section: GitSection) -> &[GitFileStatus] {
        match section {
            GitSection::Merge => &self.merge_files,
            GitSection::Staged => &self.staged_files,
            GitSection::Changes => &self.unstaged_files,
            GitSection::Untracked => &self.untracked_files,
            GitSection::Commits => &[],
        }
    }

    pub fn file_at(&self, section: GitSection, index: usize) -> Option<&GitFileStatus> {
        self.files(section).get(index)
    }

    pub fn section_len(&self, section: GitSection) -> usize {
        match section {
            GitSection::Commits => self.commits.len(),
            other => self.files(other).len(),
        }
    }

    pub fn is_collapsed(&self, section: GitSection) -> bool {
        match section {
            GitSection::Merge => self.collapsed_merge,
            GitSection::Staged => self.collapsed_staged,
            GitSection::Changes => self.collapsed_changes,
            GitSection::Untracked => self.collapsed_untracked,
            GitSection::Commits => self.collapsed_commits,
        }
    }

    pub fn set_collapsed(&mut self, section: GitSection, collapsed: bool) {
        match section {
            GitSection::Merge => self.collapsed_merge = collapsed,
            GitSection::Staged => self.collapsed_staged = collapsed,
            GitSection::Changes => self.collapsed_changes = collapsed,
            GitSection::Untracked => self.collapsed_untracked = collapsed,
            GitSection::Commits => self.collapsed_commits = collapsed,
        }
    }

    pub fn toggle_collapsed(&mut self, section: GitSection) {
        let collapsed = self.is_collapsed(section);
        self.set_collapsed(section, !collapsed);
    }

    /// Total changed paths across every file group.
    pub fn change_count(&self) -> usize {
        self.merge_files.len()
            + self.staged_files.len()
            + self.unstaged_files.len()
            + self.untracked_files.len()
    }

    pub fn has_repo(&self) -> bool {
        self.repo_root.is_some()
    }

    /// The list of lines the panel shows, in order.
    ///
    /// Empty groups are omitted entirely, as in VS Code. When nothing at all is
    /// present a single [`GitSidebarRow::Placeholder`] carries the empty state.
    pub fn rows(&self) -> Vec<GitSidebarRow> {
        let mut rows = Vec::new();
        if !self.has_repo() {
            rows.push(GitSidebarRow::Placeholder);
            return rows;
        }

        for section in [
            GitSection::Merge,
            GitSection::Staged,
            GitSection::Changes,
            GitSection::Untracked,
        ] {
            let files = self.files(section);
            if files.is_empty() {
                continue;
            }
            rows.push(GitSidebarRow::SectionHeader(section));
            if !self.is_collapsed(section) {
                for index in 0..files.len() {
                    rows.push(GitSidebarRow::File { section, index });
                }
            }
        }

        if self.change_count() == 0 {
            rows.push(GitSidebarRow::Placeholder);
        }

        if !self.commits.is_empty() {
            rows.push(GitSidebarRow::SectionHeader(GitSection::Commits));
            if !self.collapsed_commits {
                for index in 0..self.commits.len() {
                    rows.push(GitSidebarRow::Commit { index });
                }
            }
        }

        rows
    }

    pub fn selected_row(&self) -> Option<GitSidebarRow> {
        self.rows().get(self.selected).copied()
    }

    /// File under the cursor, with the group it belongs to.
    pub fn selected_file(&self) -> Option<(GitSection, &GitFileStatus)> {
        match self.selected_row()? {
            GitSidebarRow::File { section, index } => {
                self.file_at(section, index).map(|file| (section, file))
            }
            _ => None,
        }
    }

    /// Snap `selected` onto the nearest selectable row, preferring later rows.
    pub fn clamp_selection(&mut self) {
        let rows = self.rows();
        if rows.is_empty() {
            self.selected = 0;
            return;
        }
        let last = rows.len() - 1;
        if self.selected > last {
            self.selected = last;
        }
        if rows[self.selected].is_selectable() {
            return;
        }
        if let Some(next) = (self.selected..rows.len()).find(|i| rows[*i].is_selectable()) {
            self.selected = next;
            return;
        }
        if let Some(prev) = (0..=self.selected).rev().find(|i| rows[*i].is_selectable()) {
            self.selected = prev;
            return;
        }
        self.selected = 0;
    }

    /// Move the cursor by `delta` selectable rows.
    pub fn move_selection(&mut self, delta: isize) {
        self.follow_selection = true;
        let rows = self.rows();
        if rows.is_empty() {
            self.selected = 0;
            return;
        }
        let selectable: Vec<usize> = (0..rows.len())
            .filter(|i| rows[*i].is_selectable())
            .collect();
        if selectable.is_empty() {
            self.selected = 0;
            return;
        }
        let current = selectable
            .iter()
            .position(|i| *i >= self.selected)
            .unwrap_or(selectable.len() - 1);
        let next = (current as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
        self.selected = selectable[next];
    }

    pub fn select_first(&mut self) {
        self.follow_selection = true;
        self.selected = 0;
        self.clamp_selection();
    }

    pub fn select_last(&mut self) {
        self.follow_selection = true;
        self.selected = self.rows().len().saturating_sub(1);
        self.clamp_selection();
    }

    /// Select the row at `index`, snapping onto the nearest selectable row.
    pub fn select_index(&mut self, index: usize) {
        self.follow_selection = true;
        self.selected = index;
        self.clamp_selection();
    }

    /// Scroll the list by `delta` rows without moving the cursor.
    pub fn scroll_by(&mut self, delta: isize, viewport_rows: usize) {
        let total = self.rows().len();
        let max_scroll = total.saturating_sub(viewport_rows) as isize;
        let next = (self.scroll as isize + delta).clamp(0, max_scroll.max(0));
        self.scroll = next as usize;
    }

    /// Keep the cursor inside a `viewport_rows`-high window.
    pub fn scroll_selection_into_view(&mut self, viewport_rows: usize) {
        let total = self.rows().len();
        if viewport_rows == 0 || total == 0 {
            self.scroll = 0;
            return;
        }
        let max_scroll = total.saturating_sub(viewport_rows);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + viewport_rows {
            self.scroll = self.selected + 1 - viewport_rows;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Clamp the scroll offset after the row count changes.
    pub fn clamp_scroll(&mut self, viewport_rows: usize) {
        let total = self.rows().len();
        let max_scroll = total.saturating_sub(viewport_rows);
        self.scroll = self.scroll.min(max_scroll);
    }

    // -- commit message editing ------------------------------------------------

    pub fn insert_commit_text(&mut self, text: &str) {
        insert_text(&mut self.commit_message, &mut self.commit_cursor, text);
    }

    pub fn backspace_commit(&mut self) {
        backspace_text(&mut self.commit_message, &mut self.commit_cursor);
    }

    pub fn delete_commit_char(&mut self) {
        delete_text_char(&mut self.commit_message, &mut self.commit_cursor);
    }

    pub fn move_commit_cursor(&mut self, delta: isize) {
        move_text_cursor(&self.commit_message, &mut self.commit_cursor, delta);
    }

    pub fn clear_commit_message(&mut self) {
        self.commit_message.clear();
        self.commit_cursor = 0;
    }

    /// Character offset of the caret, for placing it on screen.
    pub fn commit_cursor_display_col(&self) -> usize {
        text_cursor_display_col(&self.commit_message, self.commit_cursor)
    }

    // -- refresh results -------------------------------------------------------

    /// Adopt a completed background probe.
    pub fn apply_snapshot(&mut self, snapshot: GitSidebarSnapshot) {
        self.cwd = Some(snapshot.cwd);
        self.repo_root = snapshot.repo_root;
        self.repo_name = snapshot.repo_name;
        self.branch = snapshot.branch;
        self.upstream = snapshot.upstream;
        self.ahead = snapshot.ahead;
        self.behind = snapshot.behind;
        self.merge_files = snapshot.merge_files;
        self.staged_files = snapshot.staged_files;
        self.unstaged_files = snapshot.unstaged_files;
        self.untracked_files = snapshot.untracked_files;
        self.commits = snapshot.commits;
        // `None` means "this probe did not look"; keep what the last full probe
        // found rather than blanking the branch and stash menus every poll.
        if let Some(branches) = snapshot.branches {
            self.branches = branches;
        }
        if let Some(stashes) = snapshot.stashes {
            self.stashes = stashes;
        }
        if let Some(remotes) = snapshot.remotes {
            self.default_remote = preferred_remote(&remotes);
        }
        self.rebase_in_progress = snapshot.rebase_in_progress;
        self.merge_in_progress = snapshot.merge_in_progress;
        self.cherry_pick_in_progress = snapshot.cherry_pick_in_progress;
        self.error_message = snapshot.error;
        self.is_refreshing = false;
        self.loaded = true;
        self.clamp_selection();
    }

    /// Reset everything that describes a repository, keeping user edits to the
    /// commit message out of the way of a workspace switch.
    pub fn reset_for_new_repo(&mut self) {
        self.repo_root = None;
        self.repo_name = String::new();
        self.branch = String::new();
        self.upstream = None;
        self.ahead = 0;
        self.behind = 0;
        self.merge_files.clear();
        self.staged_files.clear();
        self.unstaged_files.clear();
        self.untracked_files.clear();
        self.commits.clear();
        self.branches.clear();
        self.stashes.clear();
        self.default_remote = DEFAULT_REMOTE.to_string();
        self.rebase_in_progress = false;
        self.merge_in_progress = false;
        self.cherry_pick_in_progress = false;
        self.menu = None;
        self.prompt = None;
        self.clear_commit_message();
        self.selected = 0;
        self.scroll = 0;
        self.follow_selection = true;
        self.error_message = None;
        self.status_message = None;
        self.pending_discard = None;
        self.loaded = false;
        self.needs_full_refresh = true;
    }

    /// Label for the operation that is paused mid-way and needs continuing or
    /// aborting, shown at the end of the branch line.
    pub fn operation_label(&self) -> Option<&'static str> {
        if self.rebase_in_progress {
            Some("rebasing")
        } else if self.merge_in_progress {
            Some("merging")
        } else if self.cherry_pick_in_progress {
            Some("cherry-picking")
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Single-line text editing
//
// Shared by the commit box and the prompt box so both stay UTF-8 safe and
// caret arithmetic lives in exactly one place.
// ---------------------------------------------------------------------------

fn clamp_text_cursor(text: &str, cursor: &mut usize) {
    let len = text.len();
    if *cursor > len {
        *cursor = len;
        return;
    }
    while *cursor < len && !text.is_char_boundary(*cursor) {
        *cursor += 1;
    }
}

pub(crate) fn insert_text(text: &mut String, cursor: &mut usize, value: &str) {
    clamp_text_cursor(text, cursor);
    // These boxes are one logical line; newlines would break caret arithmetic.
    let sanitized: String = value.chars().filter(|c| !c.is_control()).collect();
    if sanitized.is_empty() {
        return;
    }
    let at = *cursor;
    text.insert_str(at, &sanitized);
    *cursor = at + sanitized.len();
}

pub(crate) fn backspace_text(text: &mut String, cursor: &mut usize) {
    clamp_text_cursor(text, cursor);
    if *cursor == 0 {
        return;
    }
    let prev = text[..*cursor]
        .char_indices()
        .next_back()
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    text.replace_range(prev..*cursor, "");
    *cursor = prev;
}

pub(crate) fn delete_text_char(text: &mut String, cursor: &mut usize) {
    clamp_text_cursor(text, cursor);
    if *cursor >= text.len() {
        return;
    }
    let next = text[*cursor..]
        .char_indices()
        .nth(1)
        .map(|(idx, _)| *cursor + idx)
        .unwrap_or(text.len());
    text.replace_range(*cursor..next, "");
}

pub(crate) fn move_text_cursor(text: &str, cursor: &mut usize, delta: isize) {
    clamp_text_cursor(text, cursor);
    if delta < 0 {
        for _ in 0..(-delta) {
            if *cursor == 0 {
                break;
            }
            *cursor = text[..*cursor]
                .char_indices()
                .next_back()
                .map(|(idx, _)| idx)
                .unwrap_or(0);
        }
    } else {
        for _ in 0..delta {
            if *cursor >= text.len() {
                break;
            }
            *cursor = text[*cursor..]
                .char_indices()
                .nth(1)
                .map(|(idx, _)| *cursor + idx)
                .unwrap_or(text.len());
        }
    }
}

pub(crate) fn text_cursor_display_col(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())].chars().count()
}

// ---------------------------------------------------------------------------
// Background git probes
// ---------------------------------------------------------------------------

/// Build a `git` invocation that can never block on a prompt or a pager, never
/// takes the index lock for a read-only query, and never flashes a console
/// window on Windows.
///
/// Uses `-C` rather than `current_dir` so the child never depends on being able
/// to chdir, matching how the rest of Herdr shells out to git.
fn git_command(repo_root: &Path) -> Command {
    let mut command = crate::noninteractive_process::command("git");
    command
        .arg("-C")
        .arg(repo_root)
        .arg("--no-pager")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_ASKPASS", "echo")
        // `commit --amend`, `rebase --continue` and `revert` would otherwise
        // launch $EDITOR and hang a worker thread forever with no terminal
        // attached to answer it.
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .stdin(Stdio::null());
    command
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = git_command(repo_root)
        .args(args)
        .output()
        .map_err(|err| format!("git: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(output.stdout)
}

/// Parse `git status --porcelain=v1 --branch -z` output.
///
/// `-z` means NUL-terminated records and unquoted paths, so a path containing
/// spaces, quotes, or non-UTF-8 bytes survives intact. Rename and copy entries
/// occupy two records: the new path, then the original.
pub(crate) fn parse_status_z(stdout: &[u8], snapshot: &mut GitSidebarSnapshot) {
    // Some git versions still terminate the `--branch` header with LF even
    // under `-z`, so a header record can carry the first entry with it.
    let mut records: Vec<&[u8]> = Vec::new();
    for chunk in stdout.split(|byte| *byte == 0) {
        if chunk.is_empty() {
            continue;
        }
        if chunk.starts_with(b"## ") {
            // The header comes first; anything after the LF is the next entry.
            for part in chunk.splitn(2, |byte| *byte == b'\n') {
                if !part.is_empty() {
                    records.push(part);
                }
            }
        } else {
            records.push(chunk);
        }
    }

    // Not a `for` loop: a rename entry pulls the following record itself.
    let mut records = records.into_iter();
    loop {
        let Some(record) = records.next() else {
            break;
        };
        let text = String::from_utf8_lossy(record);
        if let Some(header) = text.strip_prefix("## ") {
            parse_branch_header(header.trim_end_matches('\r'), snapshot);
            continue;
        }
        if text.len() < 4 {
            continue;
        }
        let mut chars = text.chars();
        let index_code = chars.next().unwrap_or(' ');
        let tree_code = chars.next().unwrap_or(' ');
        // Byte 2 is the separating space; the path starts at byte 3.
        let path = PathBuf::from(&text[3..]);

        // Rename/copy entries are followed by the original path.
        let renamed = matches!(index_code, 'R' | 'C') || matches!(tree_code, 'R' | 'C');
        let original_path = if renamed {
            records
                .next()
                .map(|orig| PathBuf::from(String::from_utf8_lossy(orig).into_owned()))
        } else {
            None
        };

        if is_unmerged(index_code, tree_code) {
            snapshot.merge_files.push(GitFileStatus {
                path,
                original_path,
                status: GitFileStatusKind::Conflicted,
            });
            continue;
        }

        if index_code == '?' && tree_code == '?' {
            snapshot.untracked_files.push(GitFileStatus {
                path,
                original_path,
                status: GitFileStatusKind::Untracked,
            });
            continue;
        }

        if index_code == '!' && tree_code == '!' {
            continue;
        }

        if index_code != ' ' && index_code != '?' {
            snapshot.staged_files.push(GitFileStatus {
                path: path.clone(),
                original_path: original_path.clone(),
                status: GitFileStatusKind::from_porcelain(index_code),
            });
        }
        if tree_code != ' ' && tree_code != '?' {
            snapshot.unstaged_files.push(GitFileStatus {
                path,
                original_path,
                status: GitFileStatusKind::from_porcelain(tree_code),
            });
        }
    }
}

/// The porcelain codes git uses for unmerged paths.
fn is_unmerged(index_code: char, tree_code: char) -> bool {
    matches!(
        (index_code, tree_code),
        ('D', 'D')
            | ('A', 'U')
            | ('U', 'D')
            | ('U', 'A')
            | ('D', 'U')
            | ('A', 'A')
            | ('U', 'U')
    )
}

/// Parse the `## <branch>...<upstream> [ahead N, behind M]` header line.
pub(crate) fn parse_branch_header(header: &str, snapshot: &mut GitSidebarSnapshot) {
    let (refs, tracking) = match header.split_once(" [") {
        Some((refs, rest)) => (refs, rest.trim_end_matches(']')),
        None => (header, ""),
    };

    let (branch, upstream) = match refs.split_once("...") {
        Some((branch, upstream)) => (branch, Some(upstream.trim().to_string())),
        None => (refs, None),
    };
    let branch = branch.trim();
    // An empty repository reports `## No commits yet on <branch>`.
    let branch = branch.strip_prefix("No commits yet on ").unwrap_or(branch);
    snapshot.branch = if branch.ends_with("(no branch)") {
        // Detached HEAD: the short hash is filled in by the caller.
        String::new()
    } else {
        branch.trim().to_string()
    };
    snapshot.upstream = upstream;

    for part in tracking.split(',') {
        let part = part.trim();
        if let Some(count) = part.strip_prefix("ahead ") {
            snapshot.ahead = count.trim().parse().unwrap_or(0);
        } else if let Some(count) = part.strip_prefix("behind ") {
            snapshot.behind = count.trim().parse().unwrap_or(0);
        }
    }
}

/// Parse `git log --pretty=format:%h<US>%s<US>%an<US>%cr`.
pub(crate) fn parse_log(stdout: &str) -> Vec<GitCommitEntry> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let mut fields = line.split(LOG_FIELD_SEP);
            let hash = fields.next()?.trim().to_string();
            if hash.is_empty() {
                return None;
            }
            Some(GitCommitEntry {
                hash,
                subject: fields.next().unwrap_or_default().to_string(),
                author: fields.next().unwrap_or_default().to_string(),
                relative_time: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// Format handed to `git for-each-ref` by [`collect_refs`].
///
/// The separator is spaced on purpose: a ref name may not contain a space, so
/// ` | ` cannot occur inside any field and no escaping is needed.
const REF_FORMAT: &str = "%(refname) | %(refname:short) | %(upstream:short) | %(HEAD)";

/// Parse [`REF_FORMAT`] output into the branch list and the remotes it implies.
pub(crate) fn parse_refs(stdout: &str) -> (Vec<GitBranchEntry>, Vec<String>) {
    let mut branches = Vec::new();
    let mut remotes: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let mut fields = line.split(" | ");
        let Some(full) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(name) = fields
            .next()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let upstream = fields
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let is_current = fields.next().map(str::trim) == Some("*");
        let is_remote = full.starts_with("refs/remotes/");

        if is_remote {
            if let Some((remote, rest)) = name.split_once('/') {
                if !remotes.iter().any(|known| known == remote) {
                    remotes.push(remote.to_string());
                }
                // `origin/HEAD` is a symbolic pointer, not a branch to check out.
                if rest == "HEAD" {
                    continue;
                }
            }
        }

        branches.push(GitBranchEntry {
            name: name.to_string(),
            upstream,
            is_remote,
            is_current,
        });
    }

    (branches, remotes)
}

/// Parse `git stash list --pretty=format:%gd<US>%gs`.
pub(crate) fn parse_stash_list(stdout: &str) -> Vec<GitStashEntry> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(fallback_index, line)| {
            let mut fields = line.split(LOG_FIELD_SEP);
            let selector = fields.next().unwrap_or_default().trim().to_string();
            let subject = fields.next().unwrap_or_default().trim().to_string();
            // `stash@{3}` -> 3; the reflog selector is authoritative, but a
            // detached or rewritten reflog can still fall back to line order.
            let index = selector
                .split_once('{')
                .and_then(|(_, rest)| rest.strip_suffix('}'))
                .and_then(|digits| digits.parse::<usize>().ok())
                .unwrap_or(fallback_index);
            // Subjects read `WIP on <branch>: <hash> <subject>` or `On <branch>: <message>`.
            let (branch, message) = match subject
                .strip_prefix("WIP on ")
                .or_else(|| subject.strip_prefix("On "))
                .and_then(|rest| rest.split_once(": "))
            {
                Some((branch, rest)) => (Some(branch.to_string()), rest.to_string()),
                None => (None, subject.clone()),
            };
            GitStashEntry {
                index,
                message: if message.is_empty() { subject } else { message },
                branch,
            }
        })
        .collect()
}

/// The remote to publish to: `origin` when it exists, otherwise the first one.
pub(crate) fn preferred_remote(remotes: &[String]) -> String {
    if remotes.iter().any(|remote| remote == DEFAULT_REMOTE) {
        return DEFAULT_REMOTE.to_string();
    }
    remotes
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_REMOTE.to_string())
}

fn collect_refs(repo_root: &Path) -> (Vec<GitBranchEntry>, Vec<String>) {
    let limit = BRANCH_LIMIT.to_string();
    match run_git(
        repo_root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--count",
            &limit,
            &format!("--format={REF_FORMAT}"),
            "refs/heads",
            "refs/remotes",
        ],
    ) {
        Ok(stdout) => parse_refs(&String::from_utf8_lossy(&stdout)),
        Err(_) => (Vec::new(), Vec::new()),
    }
}

fn collect_stashes(repo_root: &Path) -> Vec<GitStashEntry> {
    let format = format!("--pretty=format:%gd{LOG_FIELD_SEP}%gs");
    match run_git(repo_root, &["stash", "list", &format]) {
        Ok(stdout) => parse_stash_list(&String::from_utf8_lossy(&stdout)),
        Err(_) => Vec::new(),
    }
}

/// Probe `cwd`'s repository on a worker thread and report back one snapshot.
///
/// Runs off the main loop because `git status` in a large repository is
/// unbounded work and this path is reachable from every render tick.
/// `full` adds the refs walk and the stash list, which the three-second poll
/// skips: they only change when the user does something, and each one is
/// another process spawn on every tick.
pub fn spawn_refresh(cwd: PathBuf, full: bool, event_tx: tokio::sync::mpsc::Sender<AppEvent>) {
    std::thread::spawn(move || {
        let snapshot = collect_snapshot(cwd, full);
        let _ = event_tx.blocking_send(AppEvent::GitSidebarRefreshComplete(Box::new(snapshot)));
    });
}

fn collect_snapshot(cwd: PathBuf, full: bool) -> GitSidebarSnapshot {
    let mut snapshot = GitSidebarSnapshot {
        cwd: cwd.clone(),
        ..GitSidebarSnapshot::default()
    };

    // Discover the repository the same way the workspace list does: walk up for
    // `.git`, no subprocess. That keeps this panel's idea of "which repo" identical
    // to the branch shown under the workspace name, resolves linked worktrees to
    // their own checkout, and reports "not a repository" accurately even when the
    // git binary is missing.
    let Some(info) = crate::workspace::git_worktree_info(&cwd) else {
        return snapshot;
    };
    let space = crate::workspace::git_space_metadata_from_info(&info);

    let repo_root = space.repo_root;
    snapshot.repo_name = space.repo_name;
    snapshot.repo_root = Some(repo_root.clone());

    // A paused rebase/merge/cherry-pick is a marker file in the worktree's own
    // git dir. Reading the directory beats another `git` process per poll.
    snapshot.rebase_in_progress =
        info.git_dir.join("rebase-merge").exists() || info.git_dir.join("rebase-apply").exists();
    snapshot.merge_in_progress = info.git_dir.join("MERGE_HEAD").exists();
    snapshot.cherry_pick_in_progress = info.git_dir.join("CHERRY_PICK_HEAD").exists();

    match run_git(&repo_root, &["status", "--porcelain=v1", "--branch", "-z"]) {
        Ok(stdout) => parse_status_z(&stdout, &mut snapshot),
        Err(err) => snapshot.error = Some(err),
    }

    if snapshot.branch.is_empty() {
        // Detached HEAD: identify it by short hash. Fails (staying empty) in a
        // repository with no commits yet, where the header already told us so.
        snapshot.branch = run_git(&repo_root, &["rev-parse", "--short", "HEAD"])
            .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
            .unwrap_or_default();
    }

    let log_format = format!("format:%h{LOG_FIELD_SEP}%s{LOG_FIELD_SEP}%an{LOG_FIELD_SEP}%cr");
    let limit = RECENT_COMMIT_LIMIT.to_string();
    if let Ok(stdout) = run_git(
        &repo_root,
        &[
            "log",
            "-n",
            &limit,
            &format!("--pretty={log_format}"),
            "--abbrev-commit",
        ],
    ) {
        snapshot.commits = parse_log(&String::from_utf8_lossy(&stdout));
    }

    if full {
        let (branches, remotes) = collect_refs(&repo_root);
        snapshot.branches = Some(branches);
        snapshot.remotes = Some(remotes);
        snapshot.stashes = Some(collect_stashes(&repo_root));
    }

    snapshot
}

/// A mutating git command the panel can run.
///
/// Every variant carries everything the worker thread needs, so running one
/// never reaches back into `App` from off the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSidebarAction {
    Stage(PathBuf),
    Unstage(PathBuf),
    StageAll,
    UnstageAll,
    /// Restore one path from the index, leaving staged content alone.
    Discard(PathBuf),
    /// Restore every tracked path from the index.
    DiscardAll,
    /// Delete an untracked path from disk. Not recoverable through git.
    Clean(PathBuf),
    Commit(String),
    /// Replace the previous commit. An empty message keeps the old one.
    CommitAmend(String),
    Fetch,
    FetchPruneAll,
    Pull,
    PullRebase,
    Push,
    PushForceWithLease,
    /// `push --set-upstream <remote> HEAD` for a branch with no upstream.
    PushSetUpstream(String),
    /// Pull with rebase, then push. Stops at the first failure.
    Sync,
    Checkout(String),
    CreateBranch(String),
    DeleteBranch(String),
    Merge(String),
    RebaseOnto(String),
    RebaseContinue,
    RebaseSkip,
    RebaseAbort,
    MergeAbort,
    CherryPickContinue,
    CherryPickAbort,
    StashPush {
        include_untracked: bool,
        message: Option<String>,
    },
    StashPop(usize),
    StashApply(usize),
    StashDrop(usize),
    /// `reset --soft <rev>`; keeps the worktree and the index.
    ResetSoft(String),
    /// `reset --hard <rev>`; throws the worktree away.
    ResetHard(String),
    CherryPick(String),
    RevertCommit(String),
}

impl GitSidebarAction {
    /// Confirmation line shown when the command succeeds.
    fn success_message(&self) -> String {
        match self {
            GitSidebarAction::Stage(path) => format!("staged {}", short_path(path)),
            GitSidebarAction::Unstage(path) => format!("unstaged {}", short_path(path)),
            GitSidebarAction::StageAll => "staged all changes".to_string(),
            GitSidebarAction::UnstageAll => "unstaged all changes".to_string(),
            GitSidebarAction::Discard(path) => format!("discarded {}", short_path(path)),
            GitSidebarAction::DiscardAll => "discarded all changes".to_string(),
            GitSidebarAction::Clean(path) => format!("deleted {}", short_path(path)),
            GitSidebarAction::Commit(_) => "committed".to_string(),
            GitSidebarAction::CommitAmend(_) => "amended the last commit".to_string(),
            GitSidebarAction::Fetch => "fetched".to_string(),
            GitSidebarAction::FetchPruneAll => "fetched all remotes".to_string(),
            GitSidebarAction::Pull => "pulled".to_string(),
            GitSidebarAction::PullRebase => "pulled (rebase)".to_string(),
            GitSidebarAction::Push => "pushed".to_string(),
            GitSidebarAction::PushForceWithLease => "force-pushed".to_string(),
            GitSidebarAction::PushSetUpstream(remote) => format!("published to {remote}"),
            GitSidebarAction::Sync => "synced".to_string(),
            GitSidebarAction::Checkout(name) => format!("switched to {name}"),
            GitSidebarAction::CreateBranch(name) => format!("created {name}"),
            GitSidebarAction::DeleteBranch(name) => format!("deleted branch {name}"),
            GitSidebarAction::Merge(name) => format!("merged {name}"),
            GitSidebarAction::RebaseOnto(name) => format!("rebased onto {name}"),
            GitSidebarAction::RebaseContinue => "rebase continued".to_string(),
            GitSidebarAction::RebaseSkip => "rebase skipped a commit".to_string(),
            GitSidebarAction::RebaseAbort => "rebase aborted".to_string(),
            GitSidebarAction::MergeAbort => "merge aborted".to_string(),
            GitSidebarAction::CherryPickContinue => "cherry-pick continued".to_string(),
            GitSidebarAction::CherryPickAbort => "cherry-pick aborted".to_string(),
            GitSidebarAction::StashPush { .. } => "stashed changes".to_string(),
            GitSidebarAction::StashPop(index) => format!("popped stash@{{{index}}}"),
            GitSidebarAction::StashApply(index) => format!("applied stash@{{{index}}}"),
            GitSidebarAction::StashDrop(index) => format!("dropped stash@{{{index}}}"),
            GitSidebarAction::ResetSoft(rev) => format!("reset --soft to {rev}"),
            GitSidebarAction::ResetHard(rev) => format!("reset --hard to {rev}"),
            GitSidebarAction::CherryPick(hash) => format!("cherry-picked {hash}"),
            GitSidebarAction::RevertCommit(hash) => format!("reverted {hash}"),
        }
    }

    /// Whether the command talks to a remote, so the panel can say it is
    /// waiting on the network rather than looking stuck.
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            GitSidebarAction::Fetch
                | GitSidebarAction::FetchPruneAll
                | GitSidebarAction::Pull
                | GitSidebarAction::PullRebase
                | GitSidebarAction::Push
                | GitSidebarAction::PushForceWithLease
                | GitSidebarAction::PushSetUpstream(_)
                | GitSidebarAction::Sync
        )
    }
}

/// Reject anything `git` would take as an option or refuse as a ref name.
///
/// The panel builds argv directly, so there is no shell to inject into; this
/// guards against a leading `-` being read as a flag and against names git
/// would reject anyway.
pub fn validate_branch_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("branch name is empty".to_string());
    }
    if name.starts_with('-') {
        return Err("branch name may not start with '-'".to_string());
    }
    if name.ends_with('/') || name.ends_with(".lock") || name.starts_with('/') {
        return Err("invalid branch name".to_string());
    }
    if name.contains("..") || name.contains("@{") {
        return Err("invalid branch name".to_string());
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
    {
        return Err("invalid branch name".to_string());
    }
    Ok(())
}

fn short_path(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Run one mutating git command on a worker thread.
///
/// `git add`/`commit` take the index lock and can run hooks, so they never run
/// on the main loop. Completion reports back so the panel can refresh.
pub fn spawn_action(
    repo_root: PathBuf,
    action: GitSidebarAction,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
) {
    std::thread::spawn(move || {
        let result = run_action(&repo_root, &action);
        let event = match result {
            Ok(()) => AppEvent::GitSidebarActionComplete {
                error: None,
                message: Some(action.success_message()),
            },
            Err(error) => AppEvent::GitSidebarActionComplete {
                error: Some(error),
                message: None,
            },
        };
        let _ = event_tx.blocking_send(event);
    });
}

/// Run a built command and turn a non-zero exit into the message git printed.
fn finish(mut command: Command) -> Result<(), String> {
    let output = command.output().map_err(|err| format!("git: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "git command failed".to_string()
    })
}

fn run_git_args(repo_root: &Path, args: &[&str]) -> Result<(), String> {
    let mut command = git_command(repo_root);
    command.args(args);
    finish(command)
}

fn run_action(repo_root: &Path, action: &GitSidebarAction) -> Result<(), String> {
    // Sync is two commands; the push only runs if the rebase landed cleanly.
    if matches!(action, GitSidebarAction::Sync) {
        run_git_args(repo_root, &["pull", "--rebase"])?;
        return run_git_args(repo_root, &["push"]);
    }

    let mut command = git_command(repo_root);
    match action {
        GitSidebarAction::Sync => unreachable!("handled above"),
        GitSidebarAction::Stage(path) => {
            command.args(["add", "--"]).arg(path);
        }
        GitSidebarAction::Unstage(path) => {
            command.args(["restore", "--staged", "--"]).arg(path);
        }
        GitSidebarAction::StageAll => {
            command.args(["add", "--all", "--", "."]);
        }
        GitSidebarAction::UnstageAll => {
            command.args(["reset", "--quiet", "HEAD", "--", "."]);
        }
        GitSidebarAction::Discard(path) => {
            // Worktree-only restore: staged content is left alone, matching
            // VS Code's "Discard Changes" on an unstaged entry.
            command.args(["restore", "--worktree", "--"]).arg(path);
        }
        GitSidebarAction::DiscardAll => {
            command.args(["restore", "--worktree", "--", "."]);
        }
        GitSidebarAction::Clean(path) => {
            // `-d` so an untracked directory goes too; git reports these as a
            // single `dir/` entry.
            command.args(["clean", "-f", "-d", "--"]).arg(path);
        }
        GitSidebarAction::Commit(message) => {
            command.args(["commit", "-m"]).arg(message);
        }
        GitSidebarAction::CommitAmend(message) => {
            command.args(["commit", "--amend"]);
            if message.trim().is_empty() {
                command.arg("--no-edit");
            } else {
                command.arg("-m").arg(message);
            }
        }
        GitSidebarAction::Fetch => {
            command.arg("fetch");
        }
        GitSidebarAction::FetchPruneAll => {
            command.args(["fetch", "--all", "--prune"]);
        }
        GitSidebarAction::Pull => {
            command.arg("pull");
        }
        GitSidebarAction::PullRebase => {
            command.args(["pull", "--rebase"]);
        }
        GitSidebarAction::Push => {
            command.arg("push");
        }
        GitSidebarAction::PushForceWithLease => {
            command.args(["push", "--force-with-lease"]);
        }
        GitSidebarAction::PushSetUpstream(remote) => {
            command.args(["push", "--set-upstream"]).arg(remote).arg("HEAD");
        }
        GitSidebarAction::Checkout(name) => {
            // The trailing `--` keeps a branch named like a path unambiguous.
            command.arg("checkout").arg(name).arg("--");
        }
        GitSidebarAction::CreateBranch(name) => {
            command.args(["checkout", "-b"]).arg(name);
        }
        GitSidebarAction::DeleteBranch(name) => {
            // `-d` refuses to drop unmerged work; git's own error says so.
            command.args(["branch", "-d"]).arg(name);
        }
        GitSidebarAction::Merge(name) => {
            command.args(["merge", "--no-edit"]).arg(name);
        }
        GitSidebarAction::RebaseOnto(name) => {
            command.arg("rebase").arg(name);
        }
        GitSidebarAction::RebaseContinue => {
            command.args(["rebase", "--continue"]);
        }
        GitSidebarAction::RebaseSkip => {
            command.args(["rebase", "--skip"]);
        }
        GitSidebarAction::RebaseAbort => {
            command.args(["rebase", "--abort"]);
        }
        GitSidebarAction::MergeAbort => {
            command.args(["merge", "--abort"]);
        }
        GitSidebarAction::CherryPickContinue => {
            command.args(["cherry-pick", "--continue"]);
        }
        GitSidebarAction::CherryPickAbort => {
            command.args(["cherry-pick", "--abort"]);
        }
        GitSidebarAction::StashPush {
            include_untracked,
            message,
        } => {
            command.args(["stash", "push"]);
            if *include_untracked {
                command.arg("--include-untracked");
            }
            if let Some(message) = message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                command.arg("-m").arg(message);
            }
        }
        GitSidebarAction::StashPop(index) => {
            command.args(["stash", "pop"]).arg(format!("stash@{{{index}}}"));
        }
        GitSidebarAction::StashApply(index) => {
            command
                .args(["stash", "apply"])
                .arg(format!("stash@{{{index}}}"));
        }
        GitSidebarAction::StashDrop(index) => {
            command
                .args(["stash", "drop"])
                .arg(format!("stash@{{{index}}}"));
        }
        GitSidebarAction::ResetSoft(rev) => {
            command.args(["reset", "--soft"]).arg(rev);
        }
        GitSidebarAction::ResetHard(rev) => {
            command.args(["reset", "--hard"]).arg(rev);
        }
        GitSidebarAction::CherryPick(hash) => {
            command.arg("cherry-pick").arg(hash);
        }
        GitSidebarAction::RevertCommit(hash) => {
            command.args(["revert", "--no-edit"]).arg(hash);
        }
    }

    finish(command)
}

/// The command that shows a file's diff in a popup pane.
pub fn diff_command(section: GitSection, file: &GitFileStatus) -> String {
    let path = quote_arg(&file.display_path());
    match section {
        GitSection::Staged => format!("git diff --cached -- {path}"),
        GitSection::Untracked => format!("git diff --no-index -- /dev/null {path}"),
        _ => format!("git diff -- {path}"),
    }
}

/// The command that shows a commit in a popup pane.
pub fn show_command(commit: &GitCommitEntry) -> String {
    format!("git show --stat --patch {}", commit.hash)
}

/// The command that shows a stash entry in a popup pane.
pub fn stash_show_command(stash: &GitStashEntry) -> String {
    format!("git stash show --stat --patch {}", quote_arg(&stash.reference()))
}

/// Single-quote a path for the popup shell, escaping embedded quotes.
fn quote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Dropdown contents
//
// Pure functions of the panel's state, so a menu is always consistent with the
// repository as the last probe described it.
// ---------------------------------------------------------------------------

/// Build the dropdown for `kind` against the current state.
pub fn build_git_menu(kind: GitMenuKind, state: &GitSidebarState) -> GitMenuState {
    let (title, items, filterable, parent) = match kind {
        GitMenuKind::More => ("actions".to_string(), more_menu_items(state), false, None),
        GitMenuKind::Branch => ("branch".to_string(), branch_menu_items(state), false, None),
        GitMenuKind::Branches(purpose) => (
            purpose.title().to_string(),
            branch_list_items(state, purpose),
            true,
            Some(GitMenuKind::Branch),
        ),
        GitMenuKind::Stash => ("stash".to_string(), stash_menu_items(state), false, None),
        GitMenuKind::StashEntry(index) => (
            state
                .stashes
                .iter()
                .find(|stash| stash.index == index)
                .map(GitStashEntry::reference)
                .unwrap_or_else(|| "stash".to_string()),
            stash_entry_items(state, index),
            false,
            Some(GitMenuKind::Stash),
        ),
        GitMenuKind::Commit(index) => (
            state
                .commits
                .get(index)
                .map(|commit| commit.hash.clone())
                .unwrap_or_else(|| "commit".to_string()),
            commit_menu_items(state, index),
            false,
            None,
        ),
    };

    let mut menu = GitMenuState {
        kind,
        title,
        items,
        filterable,
        filter: String::new(),
        selected: 0,
        scroll: 0,
        parent,
        armed: None,
    };
    menu.clamp_selection();
    menu
}

/// Continue/skip/abort rows for whatever operation is paused, newest concern
/// first. Empty when nothing is in progress.
fn in_progress_items(state: &GitSidebarState) -> Vec<GitMenuItem> {
    let mut items = Vec::new();
    if state.rebase_in_progress {
        items.push(GitMenuItem::run(
            "Rebase: continue",
            GitSidebarAction::RebaseContinue,
        ));
        items.push(GitMenuItem::run(
            "Rebase: skip this commit",
            GitSidebarAction::RebaseSkip,
        ));
        items.push(GitMenuItem::run("Rebase: abort", GitSidebarAction::RebaseAbort).dangerous());
    }
    if state.merge_in_progress {
        items.push(GitMenuItem::run("Merge: abort", GitSidebarAction::MergeAbort).dangerous());
    }
    if state.cherry_pick_in_progress {
        items.push(GitMenuItem::run(
            "Cherry-pick: continue",
            GitSidebarAction::CherryPickContinue,
        ));
        items.push(
            GitMenuItem::run("Cherry-pick: abort", GitSidebarAction::CherryPickAbort).dangerous(),
        );
    }
    items
}

fn push_in_progress_group(state: &GitSidebarState, items: &mut Vec<GitMenuItem>) {
    let in_progress = in_progress_items(state);
    if in_progress.is_empty() {
        return;
    }
    items.extend(in_progress);
    items.push(GitMenuItem::rule());
}

fn more_menu_items(state: &GitSidebarState) -> Vec<GitMenuItem> {
    let mut items = Vec::new();
    push_in_progress_group(state, &mut items);

    items.push(GitMenuItem::new("Commit", GitMenuAction::FocusCommitMessage));
    items.push(GitMenuItem::run(
        "Commit (amend last)",
        GitSidebarAction::CommitAmend(String::new()),
    ));
    items.push(
        GitMenuItem::run(
            "Undo last commit",
            GitSidebarAction::ResetSoft("HEAD~1".to_string()),
        )
        .with_detail("reset --soft"),
    );

    items.push(GitMenuItem::rule());
    items.push(GitMenuItem::run(
        "Stage all changes",
        GitSidebarAction::StageAll,
    ));
    items.push(GitMenuItem::run(
        "Unstage all changes",
        GitSidebarAction::UnstageAll,
    ));
    items.push(GitMenuItem::run("Discard all changes", GitSidebarAction::DiscardAll).dangerous());

    items.push(GitMenuItem::rule());
    items.push(GitMenuItem::run("Fetch", GitSidebarAction::Fetch));
    items.push(GitMenuItem::run(
        "Fetch (all remotes, prune)",
        GitSidebarAction::FetchPruneAll,
    ));
    items.push(
        GitMenuItem::run("Pull", GitSidebarAction::Pull)
            .with_detail(if state.behind > 0 {
                format!("{} behind", state.behind)
            } else {
                String::new()
            }),
    );
    items.push(GitMenuItem::run(
        "Pull (rebase)",
        GitSidebarAction::PullRebase,
    ));

    if state.upstream.is_some() {
        items.push(
            GitMenuItem::run("Push", GitSidebarAction::Push).with_detail(if state.ahead > 0 {
                format!("{} ahead", state.ahead)
            } else {
                String::new()
            }),
        );
        items.push(
            GitMenuItem::run(
                "Push (force with lease)",
                GitSidebarAction::PushForceWithLease,
            )
            .dangerous(),
        );
        items.push(GitMenuItem::run(
            "Sync (pull rebase, then push)",
            GitSidebarAction::Sync,
        ));
    } else {
        // No upstream: the only push that can work is the one that sets one.
        items.push(
            GitMenuItem::run(
                "Publish branch",
                GitSidebarAction::PushSetUpstream(state.default_remote.clone()),
            )
            .with_detail(state.default_remote.clone()),
        );
    }

    items.push(GitMenuItem::rule());
    items.push(GitMenuItem::new("Refresh", GitMenuAction::Refresh));
    items
}

fn branch_menu_items(state: &GitSidebarState) -> Vec<GitMenuItem> {
    let mut items = Vec::new();
    push_in_progress_group(state, &mut items);

    items.push(GitMenuItem::new(
        "Create branch...",
        GitMenuAction::Ask(GitPromptKind::NewBranch),
    ));
    items.push(GitMenuItem::new(
        "Checkout branch...",
        GitMenuAction::Open(GitMenuKind::Branches(BranchPurpose::Checkout)),
    ));

    items.push(GitMenuItem::rule());
    items.push(GitMenuItem::new(
        "Merge branch into HEAD...",
        GitMenuAction::Open(GitMenuKind::Branches(BranchPurpose::Merge)),
    ));
    items.push(GitMenuItem::new(
        "Rebase HEAD onto...",
        GitMenuAction::Open(GitMenuKind::Branches(BranchPurpose::Rebase)),
    ));

    items.push(GitMenuItem::rule());
    items.push(GitMenuItem::new(
        "Delete branch...",
        GitMenuAction::Open(GitMenuKind::Branches(BranchPurpose::Delete)),
    ));
    items
}

fn branch_list_items(state: &GitSidebarState, purpose: BranchPurpose) -> Vec<GitMenuItem> {
    let mut items: Vec<GitMenuItem> = state
        .branches
        .iter()
        // Checking out, merging or rebasing onto the current branch is a no-op,
        // and only a local branch can be deleted with `git branch -d`.
        .filter(|branch| !branch.is_current)
        .filter(|branch| !(matches!(purpose, BranchPurpose::Delete) && branch.is_remote))
        .map(|branch| {
            let action = match purpose {
                BranchPurpose::Checkout => {
                    GitSidebarAction::Checkout(branch.checkout_name().to_string())
                }
                BranchPurpose::Merge => GitSidebarAction::Merge(branch.name.clone()),
                BranchPurpose::Rebase => GitSidebarAction::RebaseOnto(branch.name.clone()),
                BranchPurpose::Delete => GitSidebarAction::DeleteBranch(branch.name.clone()),
            };
            let item = GitMenuItem::run(branch.name.clone(), action);
            let item = if matches!(purpose, BranchPurpose::Delete) {
                item.dangerous()
            } else {
                item
            };
            item.with_detail(if branch.is_remote {
                "remote".to_string()
            } else {
                branch.upstream.clone().unwrap_or_default()
            })
        })
        .collect();

    if items.is_empty() {
        items.push(GitMenuItem::notice(if state.branches.is_empty() {
            "no branches loaded yet"
        } else {
            "no other branches"
        }));
    }
    items
}

fn stash_menu_items(state: &GitSidebarState) -> Vec<GitMenuItem> {
    let mut items = vec![
        GitMenuItem::new(
            "Stash changes...",
            GitMenuAction::Ask(GitPromptKind::StashMessage {
                include_untracked: false,
            }),
        ),
        GitMenuItem::new(
            "Stash changes (include untracked)...",
            GitMenuAction::Ask(GitPromptKind::StashMessage {
                include_untracked: true,
            }),
        ),
    ];

    if state.stashes.is_empty() {
        items.push(GitMenuItem::rule());
        items.push(GitMenuItem::notice("stash is empty"));
        return items;
    }

    let latest = state.stashes[0].index;
    items.push(GitMenuItem::rule());
    items.push(GitMenuItem::run(
        "Pop latest stash",
        GitSidebarAction::StashPop(latest),
    ));
    items.push(GitMenuItem::run(
        "Apply latest stash",
        GitSidebarAction::StashApply(latest),
    ));

    items.push(GitMenuItem::rule());
    for stash in &state.stashes {
        items.push(
            GitMenuItem::new(
                format!("{} {}", stash.reference(), stash.message),
                GitMenuAction::Open(GitMenuKind::StashEntry(stash.index)),
            )
            .with_detail(stash.branch.clone().unwrap_or_default()),
        );
    }
    items
}

fn stash_entry_items(state: &GitSidebarState, index: usize) -> Vec<GitMenuItem> {
    let Some(stash) = state.stashes.iter().find(|stash| stash.index == index) else {
        return vec![GitMenuItem::notice("stash entry is gone")];
    };
    vec![
        GitMenuItem::new("Show", GitMenuAction::Popup(stash_show_command(stash))),
        GitMenuItem::run("Apply", GitSidebarAction::StashApply(index)),
        GitMenuItem::run("Pop (apply and drop)", GitSidebarAction::StashPop(index)),
        GitMenuItem::rule(),
        GitMenuItem::run("Drop", GitSidebarAction::StashDrop(index)).dangerous(),
    ]
}

fn commit_menu_items(state: &GitSidebarState, index: usize) -> Vec<GitMenuItem> {
    let Some(commit) = state.commits.get(index) else {
        return vec![GitMenuItem::notice("commit is gone")];
    };
    let hash = commit.hash.clone();
    vec![
        GitMenuItem::new("Show", GitMenuAction::Popup(show_command(commit)))
            .with_detail(commit.relative_time.clone()),
        GitMenuItem::rule(),
        GitMenuItem::run("Cherry-pick onto HEAD", GitSidebarAction::CherryPick(hash.clone())),
        GitMenuItem::run("Revert", GitSidebarAction::RevertCommit(hash.clone())),
        GitMenuItem::rule(),
        GitMenuItem::run("Reset here (soft)", GitSidebarAction::ResetSoft(hash.clone()))
            .with_detail("keep changes")
            .dangerous(),
        GitMenuItem::run("Reset here (hard)", GitSidebarAction::ResetHard(hash))
            .with_detail("discard changes")
            .dangerous(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_from(status: &str) -> GitSidebarSnapshot {
        let mut snapshot = GitSidebarSnapshot::default();
        parse_status_z(status.as_bytes(), &mut snapshot);
        snapshot
    }

    #[test]
    fn parses_branch_header_with_ahead_and_behind() {
        let snapshot = snapshot_from("## main...origin/main [ahead 2, behind 3]\0");
        assert_eq!(snapshot.branch, "main");
        assert_eq!(snapshot.upstream.as_deref(), Some("origin/main"));
        assert_eq!(snapshot.ahead, 2);
        assert_eq!(snapshot.behind, 3);
    }

    #[test]
    fn parses_branch_header_without_upstream() {
        let snapshot = snapshot_from("## feat-git-sidebar\0");
        assert_eq!(snapshot.branch, "feat-git-sidebar");
        assert_eq!(snapshot.upstream, None);
        assert_eq!(snapshot.ahead, 0);
    }

    #[test]
    fn splits_index_and_worktree_changes_into_separate_groups() {
        let snapshot = snapshot_from("## main\0MM src/app.rs\0");
        assert_eq!(snapshot.staged_files.len(), 1);
        assert_eq!(snapshot.unstaged_files.len(), 1);
        assert_eq!(
            snapshot.staged_files[0].status,
            GitFileStatusKind::Modified
        );
    }

    #[test]
    fn groups_untracked_and_conflicts_separately() {
        let snapshot = snapshot_from("## main\0?? new file.txt\0UU merged.rs\0");
        assert_eq!(snapshot.untracked_files.len(), 1);
        assert_eq!(
            snapshot.untracked_files[0].path,
            PathBuf::from("new file.txt")
        );
        assert_eq!(snapshot.merge_files.len(), 1);
        assert!(snapshot.staged_files.is_empty());
        assert!(snapshot.unstaged_files.is_empty());
    }

    #[test]
    fn rename_entry_consumes_its_original_path_record() {
        let snapshot = snapshot_from("## main\0R  new.rs\0old.rs\0?? after.txt\0");
        assert_eq!(snapshot.staged_files.len(), 1);
        assert_eq!(snapshot.staged_files[0].path, PathBuf::from("new.rs"));
        assert_eq!(
            snapshot.staged_files[0].original_path,
            Some(PathBuf::from("old.rs"))
        );
        // The original-path record must not be mistaken for another entry.
        assert_eq!(snapshot.untracked_files.len(), 1);
        assert_eq!(snapshot.untracked_files[0].path, PathBuf::from("after.txt"));
    }

    #[test]
    fn log_parser_keeps_pipes_in_subjects() {
        let line = format!("abc1234{LOG_FIELD_SEP}fix: a|b thing{LOG_FIELD_SEP}Ada{LOG_FIELD_SEP}2 days ago");
        let commits = parse_log(&line);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "fix: a|b thing");
        assert_eq!(commits[0].author, "Ada");
        assert_eq!(commits[0].relative_time, "2 days ago");
    }

    fn modified(path: &str) -> GitFileStatus {
        GitFileStatus {
            path: PathBuf::from(path),
            original_path: None,
            status: GitFileStatusKind::Modified,
        }
    }

    fn empty_repo_state() -> GitSidebarState {
        GitSidebarState {
            repo_root: Some(PathBuf::from("/repo")),
            ..GitSidebarState::default()
        }
    }

    fn state_with_changes() -> GitSidebarState {
        GitSidebarState {
            staged_files: vec![modified("a.rs")],
            unstaged_files: vec![modified("b.rs"), modified("c.rs")],
            ..empty_repo_state()
        }
    }

    #[test]
    fn rows_omit_empty_sections_and_respect_collapse() {
        let mut state = state_with_changes();
        let rows = state.rows();
        // staged header + 1 file + changes header + 2 files
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], GitSidebarRow::SectionHeader(GitSection::Staged));

        state.collapsed_changes = true;
        let rows = state.rows();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn empty_repo_shows_a_single_placeholder() {
        let mut state = empty_repo_state();
        assert_eq!(state.rows(), vec![GitSidebarRow::Placeholder]);
        state.move_selection(1);
        assert_eq!(state.selected, 0);
        assert_eq!(state.selected_file(), None);
    }

    #[test]
    fn selection_moves_over_selectable_rows_only() {
        let mut state = state_with_changes();
        state.selected = 0;
        state.move_selection(1);
        assert_eq!(state.selected, 1);
        state.move_selection(10);
        assert_eq!(state.selected, 4);
        state.move_selection(-100);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn selection_clamps_after_files_disappear() {
        let mut state = state_with_changes();
        state.selected = 4;
        state.unstaged_files.clear();
        state.clamp_selection();
        assert_eq!(state.selected, 1);
        assert!(state.selected_file().is_some());
    }

    #[test]
    fn commit_message_editing_is_utf8_safe() {
        let mut state = GitSidebarState::default();
        state.insert_commit_text("héllo");
        assert_eq!(state.commit_cursor, state.commit_message.len());
        state.move_commit_cursor(-1);
        state.backspace_commit();
        assert_eq!(state.commit_message, "hélo");
        // Control characters are dropped: the box is one logical line.
        state.insert_commit_text("\nx\t");
        assert_eq!(state.commit_message, "hélxo");
    }

    #[test]
    fn scroll_follows_the_cursor() {
        let mut state = state_with_changes();
        state.selected = 4;
        state.scroll_selection_into_view(2);
        assert_eq!(state.scroll, 3);
        state.selected = 0;
        state.scroll_selection_into_view(2);
        assert_eq!(state.scroll, 0);
    }

    fn menu_labels(menu: &GitMenuState) -> Vec<String> {
        menu.visible()
            .into_iter()
            .map(|index| menu.items[index].label.clone())
            .collect()
    }

    #[test]
    fn ref_parser_separates_locals_remotes_and_head() {
        let stdout = [
            "refs/heads/main | main |  | *",
            "refs/heads/dev | dev | origin/dev |  ",
            "refs/remotes/origin/HEAD | origin/HEAD |  |  ",
            "refs/remotes/origin/main | origin/main |  |  ",
        ]
        .join("\n");

        let (branches, remotes) = parse_refs(&stdout);
        assert_eq!(remotes, vec!["origin".to_string()]);
        // `origin/HEAD` is a symbolic pointer and is dropped.
        assert_eq!(
            branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            vec!["main", "dev", "origin/main"]
        );
        assert!(branches[0].is_current);
        assert!(!branches[0].is_remote);
        assert_eq!(branches[1].upstream.as_deref(), Some("origin/dev"));
        assert!(branches[2].is_remote);
        // A remote branch is checked out by its short name so git's DWIM
        // creates the local tracking branch.
        assert_eq!(branches[2].checkout_name(), "main");
        assert_eq!(branches[1].checkout_name(), "dev");
    }

    #[test]
    fn stash_parser_reads_the_selector_and_strips_the_wip_prefix() {
        let stdout = format!(
            "stash@{{0}}{LOG_FIELD_SEP}WIP on main: abc1234 earlier commit\n\
             stash@{{1}}{LOG_FIELD_SEP}On feature: hand written label"
        );
        let stashes = parse_stash_list(&stdout);
        assert_eq!(stashes.len(), 2);
        assert_eq!(stashes[0].index, 0);
        assert_eq!(stashes[0].branch.as_deref(), Some("main"));
        assert_eq!(stashes[0].message, "abc1234 earlier commit");
        assert_eq!(stashes[1].reference(), "stash@{1}");
        assert_eq!(stashes[1].branch.as_deref(), Some("feature"));
        assert_eq!(stashes[1].message, "hand written label");
    }

    #[test]
    fn preferred_remote_favours_origin() {
        assert_eq!(preferred_remote(&[]), "origin");
        assert_eq!(
            preferred_remote(&["upstream".to_string(), "origin".to_string()]),
            "origin"
        );
        assert_eq!(preferred_remote(&["fork".to_string()]), "fork");
    }

    #[test]
    fn branch_names_that_git_would_reject_are_refused() {
        assert!(validate_branch_name("feat/thing").is_ok());
        assert!(validate_branch_name("").is_err());
        // A leading dash would be read as an option, not a name.
        assert!(validate_branch_name("--force").is_err());
        assert!(validate_branch_name("has space").is_err());
        assert!(validate_branch_name("a..b").is_err());
        assert!(validate_branch_name("caret^").is_err());
    }

    #[test]
    fn more_menu_offers_publish_only_without_an_upstream() {
        let unpublished = GitSidebarState {
            repo_root: Some(PathBuf::from("/repo")),
            branch: "feature".to_string(),
            ..GitSidebarState::default()
        };
        let labels = menu_labels(&build_git_menu(GitMenuKind::More, &unpublished));
        assert!(labels.iter().any(|label| label == "Publish branch"));
        assert!(!labels.iter().any(|label| label == "Push"));

        let published = GitSidebarState {
            upstream: Some("origin/feature".to_string()),
            ..unpublished
        };
        let labels = menu_labels(&build_git_menu(GitMenuKind::More, &published));
        assert!(labels.iter().any(|label| label == "Push"));
        assert!(!labels.iter().any(|label| label == "Publish branch"));
    }

    #[test]
    fn paused_rebase_puts_continue_and_abort_at_the_top() {
        let state = GitSidebarState {
            repo_root: Some(PathBuf::from("/repo")),
            rebase_in_progress: true,
            ..GitSidebarState::default()
        };
        assert_eq!(state.operation_label(), Some("rebasing"));
        let menu = build_git_menu(GitMenuKind::More, &state);
        assert_eq!(menu_labels(&menu)[0], "Rebase: continue");
        // The cursor never opens on a rule or a disabled row.
        assert!(menu.selected_item().is_some_and(|item| item.is_selectable()));
    }

    #[test]
    fn branch_list_filters_and_skips_the_current_branch() {
        let branch = |name: &str, current: bool, remote: bool| GitBranchEntry {
            name: name.to_string(),
            upstream: None,
            is_remote: remote,
            is_current: current,
        };
        let state = GitSidebarState {
            repo_root: Some(PathBuf::from("/repo")),
            branches: vec![
                branch("main", true, false),
                branch("feature/login", false, false),
                branch("origin/main", false, true),
            ],
            ..GitSidebarState::default()
        };

        let mut menu = build_git_menu(
            GitMenuKind::Branches(BranchPurpose::Checkout),
            &state,
        );
        assert_eq!(menu_labels(&menu), vec!["feature/login", "origin/main"]);

        menu.push_filter('l');
        menu.push_filter('o');
        assert_eq!(menu_labels(&menu), vec!["feature/login"]);
        assert!(menu.pop_filter());

        // `git branch -d` only takes a local branch.
        let delete = build_git_menu(GitMenuKind::Branches(BranchPurpose::Delete), &state);
        assert_eq!(menu_labels(&delete), vec!["feature/login"]);
        assert!(delete.items[0].danger);
    }

    #[test]
    fn reselecting_a_menu_row_keeps_it_armed() {
        let mut menu = build_git_menu(
            GitMenuKind::More,
            &GitSidebarState {
                repo_root: Some(PathBuf::from("/repo")),
                ..GitSidebarState::default()
            },
        );
        menu.armed = Some(menu.selected_item_index().expect("a selectable row"));
        let selected = menu.selected;
        menu.select_visible(selected);
        assert!(menu.armed.is_some());
        menu.move_selection(1);
        assert!(menu.armed.is_none());
    }

    #[test]
    fn diff_command_picks_the_right_git_invocation() {
        let file = GitFileStatus {
            path: PathBuf::from("src/a b.rs"),
            original_path: None,
            status: GitFileStatusKind::Modified,
        };
        assert_eq!(
            diff_command(GitSection::Staged, &file),
            "git diff --cached -- 'src/a b.rs'"
        );
        assert_eq!(
            diff_command(GitSection::Changes, &file),
            "git diff -- 'src/a b.rs'"
        );
        assert!(diff_command(GitSection::Untracked, &file).contains("--no-index"));
    }
}
