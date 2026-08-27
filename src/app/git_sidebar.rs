use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitSidebarState {
    pub staged_files: Vec<GitFileStatus>,
    pub unstaged_files: Vec<GitFileStatus>,
    pub recent_commits: Vec<GitCommitGraphEntry>,
    pub commit_message: String,
    pub commit_cursor: usize,
    pub section_collapsed_staged: bool,
    pub section_collapsed_changes: bool,
    pub section_collapsed_commits: bool,
    pub scroll_changes: usize,
    pub scroll_graph: usize,
    pub selected_item: Option<GitSidebarItem>,
    pub is_refreshing: bool,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFileStatus {
    pub path: PathBuf,
    pub status: GitFileStatusKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitFileStatusKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitGraphEntry {
    pub hash: String,
    pub subject: String,
    pub author: String,
    pub relative_time: String,
    pub graph_columns: Vec<char>, // ascii graph column representations
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSidebarItem {
    StagedFile(usize),
    UnstagedFile(usize),
    Commit(usize),
}
