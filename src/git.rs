//! Read-only git integration via libgit2 (`git2`): working-tree file status,
//! per-file line changes vs HEAD (for the editor gutter), and the current
//! branch. All functions open the repository on demand so they can run on a
//! background thread (the returned data is plain `Send` values).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use git2::{DiffOptions, Repository, Status};

/// Working-tree status of a file, relative to HEAD / the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Modified,
    Added,     // staged new
    Untracked, // new, not staged
    Deleted,
    Renamed,
    Conflicted,
}

/// How a line in the current file differs from its HEAD version.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineChange {
    Added,
    Modified,
}

/// Line-level change set for one file (new-file line indices, 0-based).
#[derive(Clone, Default)]
pub struct FileDiff {
    pub changed: Vec<(usize, LineChange)>,
    /// New-file line index that has deleted content immediately above it.
    pub deleted_before: Vec<usize>,
}

/// One entry in the commit log.
#[derive(Clone)]
pub struct CommitInfo {
    pub id: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    pub time: i64, // unix seconds
}

/// A file changed by a commit.
#[derive(Clone)]
pub struct FileChange {
    pub path: String,
    pub status: FileStatus,
}

/// A single line of a unified diff.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Context,
    Add,
    Del,
    Hunk,
}

#[derive(Clone)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

/// Most recent commits reachable from HEAD (newest first), up to `limit`.
pub fn commit_log(root: &Path, limit: usize) -> Vec<CommitInfo> {
    let mut out = Vec::new();
    let Ok(repo) = Repository::discover(root) else {
        return out;
    };
    let Ok(mut walk) = repo.revwalk() else {
        return out;
    };
    if walk.push_head().is_err() {
        return out;
    }
    let _ = walk.set_sorting(git2::Sort::TIME);
    for oid in walk.flatten().take(limit) {
        let Ok(c) = repo.find_commit(oid) else { continue };
        let id = oid.to_string();
        out.push(CommitInfo {
            short: id.chars().take(7).collect(),
            id,
            summary: c.summary().unwrap_or("").to_string(),
            author: c.author().name().unwrap_or("").to_string(),
            time: c.time().seconds(),
        });
    }
    out
}

fn delta_status(s: git2::Delta) -> FileStatus {
    use git2::Delta;
    match s {
        Delta::Added | Delta::Copied => FileStatus::Added,
        Delta::Deleted => FileStatus::Deleted,
        Delta::Renamed => FileStatus::Renamed,
        Delta::Conflicted => FileStatus::Conflicted,
        _ => FileStatus::Modified,
    }
}

/// Files changed by `commit_id` (vs its first parent; vs empty for the root).
pub fn commit_files(root: &Path, commit_id: &str) -> Vec<FileChange> {
    let mut out = Vec::new();
    let Ok(repo) = Repository::discover(root) else {
        return out;
    };
    let Ok(oid) = git2::Oid::from_str(commit_id) else {
        return out;
    };
    let Ok(commit) = repo.find_commit(oid) else {
        return out;
    };
    let tree = commit.tree().ok();
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let Ok(diff) =
        repo.diff_tree_to_tree(parent_tree.as_ref(), tree.as_ref(), None)
    else {
        return out;
    };
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        out.push(FileChange {
            path,
            status: delta_status(delta.status()),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Unified diff of one `file` in `commit_id` (vs its first parent).
pub fn commit_file_diff(root: &Path, commit_id: &str, file: &str) -> Vec<DiffLine> {
    let out = Vec::new();
    let Ok(repo) = Repository::discover(root) else {
        return out;
    };
    let Ok(oid) = git2::Oid::from_str(commit_id) else {
        return out;
    };
    let Ok(commit) = repo.find_commit(oid) else {
        return out;
    };
    let tree = commit.tree().ok();
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = DiffOptions::new();
    opts.pathspec(file);
    opts.context_lines(3);
    let Ok(diff) =
        repo.diff_tree_to_tree(parent_tree.as_ref(), tree.as_ref(), Some(&mut opts))
    else {
        return out;
    };

    let sink = std::cell::RefCell::new(out);
    let _ = diff.foreach(
        &mut |_d, _| true,
        None,
        Some(&mut |_d, hunk| {
            sink.borrow_mut().push(DiffLine {
                kind: DiffKind::Hunk,
                old_no: None,
                new_no: None,
                text: String::from_utf8_lossy(hunk.header()).trim_end().to_string(),
            });
            true
        }),
        Some(&mut |_d, _h, line| {
            let kind = match line.origin() {
                '+' => DiffKind::Add,
                '-' => DiffKind::Del,
                _ => DiffKind::Context,
            };
            let text = String::from_utf8_lossy(line.content())
                .trim_end_matches('\n')
                .to_string();
            sink.borrow_mut().push(DiffLine {
                kind,
                old_no: line.old_lineno(),
                new_no: line.new_lineno(),
                text,
            });
            true
        }),
    );
    sink.into_inner()
}

/// Current branch (or short commit hash when detached), if inside a repo.
pub fn current_branch(root: &Path) -> Option<String> {
    let repo = Repository::discover(root).ok()?;
    let head = repo.head().ok()?;
    if let Some(name) = head.shorthand() {
        return Some(name.to_string());
    }
    let oid = head.target()?;
    Some(oid.to_string().chars().take(7).collect())
}

/// Absolute-path → status map for the whole working tree.
pub fn status_map(root: &Path) -> HashMap<PathBuf, FileStatus> {
    let mut out = HashMap::new();
    let repo = match Repository::discover(root) {
        Ok(r) => r,
        Err(_) => return out,
    };
    let workdir = match repo.workdir() {
        Some(w) => w.to_path_buf(),
        None => return out,
    };
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(s) => s,
        Err(_) => return out,
    };
    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        let s = entry.status();
        let kind = if s.is_conflicted() {
            FileStatus::Conflicted
        } else if s.contains(Status::WT_NEW) {
            FileStatus::Untracked
        } else if s.contains(Status::INDEX_NEW) {
            FileStatus::Added
        } else if s.contains(Status::WT_DELETED) || s.contains(Status::INDEX_DELETED) {
            FileStatus::Deleted
        } else if s.contains(Status::WT_RENAMED) || s.contains(Status::INDEX_RENAMED) {
            FileStatus::Renamed
        } else if s.intersects(Status::WT_MODIFIED | Status::INDEX_MODIFIED) {
            FileStatus::Modified
        } else {
            continue;
        };
        out.insert(workdir.join(path), kind);
    }
    out
}

/// Per-line changes of `file` vs its committed (HEAD) version, for the gutter.
pub fn file_line_changes(root: &Path, file: &Path) -> Option<FileDiff> {
    let repo = Repository::discover(root).ok()?;
    let workdir = repo.workdir()?;
    let rel = file.strip_prefix(workdir).ok()?;

    let head_tree = repo.head().ok()?.peel_to_tree().ok()?;
    let mut opts = DiffOptions::new();
    opts.pathspec(rel);
    opts.context_lines(0);
    opts.include_untracked(false);
    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))
        .ok()?;

    let mut result = FileDiff::default();
    diff.foreach(
        &mut |_delta, _| true,
        None,
        Some(&mut |_delta, hunk| {
            let ns = hunk.new_start() as usize; // 1-based
            let nl = hunk.new_lines() as usize;
            let ol = hunk.old_lines() as usize;
            if nl == 0 {
                // pure deletion — mark just above the hunk's new position
                result.deleted_before.push(ns.saturating_sub(1));
            } else {
                let kind = if ol == 0 {
                    LineChange::Added
                } else {
                    LineChange::Modified
                };
                for l in 0..nl {
                    result.changed.push((ns - 1 + l, kind));
                }
            }
            true
        }),
        None,
    )
    .ok()?;

    Some(result)
}
