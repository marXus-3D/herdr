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

    fn clamp_cursor(&mut self) {
        let len = self.commit_message.len();
        if self.commit_cursor > len {
            self.commit_cursor = len;
            return;
        }
        while self.commit_cursor < len && !self.commit_message.is_char_boundary(self.commit_cursor) {
            self.commit_cursor += 1;
        }
    }

    pub fn insert_commit_text(&mut self, text: &str) {
        self.clamp_cursor();
        // The box is one logical line; newlines would break caret arithmetic.
        let sanitized: String = text.chars().filter(|c| !c.is_control()).collect();
        if sanitized.is_empty() {
            return;
        }
        let at = self.commit_cursor;
        self.commit_message.insert_str(at, &sanitized);
        self.commit_cursor = at + sanitized.len();
    }

    pub fn backspace_commit(&mut self) {
        self.clamp_cursor();
        if self.commit_cursor == 0 {
            return;
        }
        let prev = self.commit_message[..self.commit_cursor]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.commit_message.replace_range(prev..self.commit_cursor, "");
        self.commit_cursor = prev;
    }

    pub fn delete_commit_char(&mut self) {
        self.clamp_cursor();
        if self.commit_cursor >= self.commit_message.len() {
            return;
        }
        let next = self.commit_message[self.commit_cursor..]
            .char_indices()
            .nth(1)
            .map(|(idx, _)| self.commit_cursor + idx)
            .unwrap_or(self.commit_message.len());
        self.commit_message
            .replace_range(self.commit_cursor..next, "");
    }

    pub fn move_commit_cursor(&mut self, delta: isize) {
        self.clamp_cursor();
        if delta < 0 {
            for _ in 0..(-delta) {
                if self.commit_cursor == 0 {
                    break;
                }
                self.commit_cursor = self.commit_message[..self.commit_cursor]
                    .char_indices()
                    .next_back()
                    .map(|(idx, _)| idx)
                    .unwrap_or(0);
            }
        } else {
            for _ in 0..delta {
                if self.commit_cursor >= self.commit_message.len() {
                    break;
                }
                self.commit_cursor = self.commit_message[self.commit_cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(idx, _)| self.commit_cursor + idx)
                    .unwrap_or(self.commit_message.len());
            }
        }
    }

    pub fn clear_commit_message(&mut self) {
        self.commit_message.clear();
        self.commit_cursor = 0;
    }

    /// Character offset of the caret, for placing it on screen.
    pub fn commit_cursor_display_col(&self) -> usize {
        self.commit_message[..self.commit_cursor.min(self.commit_message.len())]
            .chars()
            .count()
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
        self.clear_commit_message();
        self.selected = 0;
        self.scroll = 0;
        self.follow_selection = true;
        self.error_message = None;
        self.status_message = None;
        self.pending_discard = None;
        self.loaded = false;
    }
}

// ---------------------------------------------------------------------------
// Background git probes
// ---------------------------------------------------------------------------

/// Build a `git` invocation that can never block on a prompt or a pager and
/// never takes the index lock for a read-only query.
fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_ASKPASS", "echo")
        .stdin(Stdio::null());
    command
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = git_command(cwd)
        .arg("--no-pager")
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

/// Probe `cwd`'s repository on a worker thread and report back one snapshot.
///
/// Runs off the main loop because `git status` in a large repository is
/// unbounded work and this path is reachable from every render tick.
pub fn spawn_refresh(cwd: PathBuf, event_tx: tokio::sync::mpsc::Sender<AppEvent>) {
    std::thread::spawn(move || {
        let snapshot = collect_snapshot(cwd);
        let _ = event_tx.blocking_send(AppEvent::GitSidebarRefreshComplete(Box::new(snapshot)));
    });
}

fn collect_snapshot(cwd: PathBuf) -> GitSidebarSnapshot {
    let mut snapshot = GitSidebarSnapshot {
        cwd: cwd.clone(),
        ..GitSidebarSnapshot::default()
    };

    let toplevel = match run_git(&cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(stdout) => {
            let text = String::from_utf8_lossy(&stdout).trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(PathBuf::from(text))
            }
        }
        Err(_) => None,
    };

    let Some(repo_root) = toplevel else {
        // Not a repository (or no git binary): a clean empty state, not an error.
        return snapshot;
    };

    snapshot.repo_name = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());
    snapshot.repo_root = Some(repo_root.clone());

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

    snapshot
}

/// A mutating git command the panel can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSidebarAction {
    Stage(PathBuf),
    Unstage(PathBuf),
    StageAll,
    UnstageAll,
    Discard(PathBuf),
    Commit(String),
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
            GitSidebarAction::Commit(_) => "committed".to_string(),
        }
    }
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

fn run_action(repo_root: &Path, action: &GitSidebarAction) -> Result<(), String> {
    let mut command = git_command(repo_root);
    command.arg("--no-pager");
    match action {
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
            command
                .args(["restore", "--worktree", "--"])
                .arg(path);
        }
        GitSidebarAction::Commit(message) => {
            command.args(["commit", "-m"]).arg(message);
        }
    }

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

/// Single-quote a path for the popup shell, escaping embedded quotes.
fn quote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
