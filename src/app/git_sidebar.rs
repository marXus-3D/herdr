use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSidebarState {
    pub staged_files: Vec<GitFileStatus>,
    pub unstaged_files: Vec<GitFileStatus>,
    pub recent_commits: Vec<GitCommitGraphEntry>,
    pub commit_message: String,
    pub commit_cursor: usize,
    pub section_collapsed_staged: bool,
    pub section_collapsed_changes: bool,
    pub section_collapsed_commits: bool,
    pub selected_index: usize,
    pub scroll_changes: usize,
    pub scroll_graph: usize,
    pub is_refreshing: bool,
    pub needs_force_refresh: bool,
    pub error_message: Option<String>,
    pub repo_name: String,
    pub branch: String,
    pub last_refresh: Option<std::time::Instant>,
}

impl Default for GitSidebarState {
    fn default() -> Self {
        Self {
            staged_files: Vec::new(),
            unstaged_files: Vec::new(),
            recent_commits: Vec::new(),
            commit_message: String::new(),
            commit_cursor: 0,
            section_collapsed_staged: false,
            section_collapsed_changes: false,
            section_collapsed_commits: false,
            selected_index: 0,
            scroll_changes: 0,
            scroll_graph: 0,
            is_refreshing: false,
            needs_force_refresh: true, // IMPORTANT: force refresh on startup
            error_message: None,
            repo_name: String::new(),
            branch: String::new(),
            last_refresh: None,
        }
    }
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



impl GitSidebarState {
    pub fn spawn_refresh(
        cwd: std::path::PathBuf,
        event_tx: tokio::sync::mpsc::Sender<crate::events::AppEvent>,
    ) {
        std::thread::spawn(move || {
            let status_cmd = std::process::Command::new("git")
                .args(&["status", "--porcelain"])
                .current_dir(&cwd)
                .output();

            let log_cmd = std::process::Command::new("git")
                .args(&["log", "-n", "50", "--graph", "--pretty=format:%h|%s|%an|%cr", "--abbrev-commit"])
                .current_dir(&cwd)
                .output();

            let mut staged = Vec::new();
            let mut unstaged = Vec::new();
            let mut error = None;

            match status_cmd {
                Ok(out) if out.status.success() => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    for line in stdout.lines() {
                        if line.len() < 4 { continue; }
                        let index_status = line.chars().nth(0).unwrap_or(' ');
                        let work_tree_status = line.chars().nth(1).unwrap_or(' ');
                        let path = std::path::PathBuf::from(line[3..].to_string());

                        if index_status != ' ' && index_status != '?' {
                            let kind = match index_status {
                                'A' => GitFileStatusKind::Added,
                                'D' => GitFileStatusKind::Deleted,
                                'R' => GitFileStatusKind::Renamed,
                                _ => GitFileStatusKind::Modified,
                            };
                            staged.push(GitFileStatus { path: path.clone(), status: kind });
                        }
                        if work_tree_status != ' ' {
                            let kind = match work_tree_status {
                                '?' => GitFileStatusKind::Untracked,
                                'D' => GitFileStatusKind::Deleted,
                                _ => GitFileStatusKind::Modified,
                            };
                            unstaged.push(GitFileStatus { path, status: kind });
                        }
                    }
                }
                Ok(out) => {
                    error = Some(String::from_utf8_lossy(&out.stderr).into_owned());
                }
                Err(e) => {
                    error = Some(e.to_string());
                }
            }

            let mut commits = Vec::new();
            if let Ok(out) = log_cmd {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    for line in stdout.lines() {
                        // parse graph and commit data
                        // simple approximation: everything before the first hex/pipe is graph
                        let mut parts = line.rsplitn(4, '|');
                        let cr = parts.next().unwrap_or("").trim().to_string();
                        let an = parts.next().unwrap_or("").trim().to_string();
                        let s = parts.next().unwrap_or("").trim().to_string();
                        let remainder = parts.next().unwrap_or("").trim_end();
                        
                        let hash_start = remainder.rfind(' ').map(|i| i + 1).unwrap_or(0);
                        let hash = remainder[hash_start..].to_string();
                        let graph = remainder[..hash_start].chars().collect();
                        
                        if !hash.is_empty() {
                            commits.push(GitCommitGraphEntry { hash, subject: s, author: an, relative_time: cr, graph_columns: graph });
                        } else {
                            commits.push(GitCommitGraphEntry { hash: "".to_string(), subject: "".to_string(), author: "".to_string(), relative_time: "".to_string(), graph_columns: remainder.chars().collect() });
                        }
                    }
                }
            }

            let mut branch = String::new();
            if let Ok(b) = std::process::Command::new("git").args(&["branch", "--show-current"]).current_dir(&cwd).output() {
                branch = String::from_utf8_lossy(&b.stdout).trim().to_string();
            }
            let repo_name = cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

            let _ = event_tx.blocking_send(crate::events::AppEvent::GitSidebarRefreshComplete {
                staged,
                unstaged,
                commits,
                error,
                branch,
                repo_name,
            });
        });
    }
}
