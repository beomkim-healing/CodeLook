//! Git integration. Reads (status, diffs, branches, log) go through libgit2
//! (`git2`); mutations (checkout / pull / fetch) shell out to the system
//! `git` CLI so the user's existing auth (ssh-agent, credential helpers) and
//! config apply unchanged. All functions open the repository on demand so
//! they can run on a background thread (the returned data is plain `Send`).

use std::collections::{HashMap, HashSet};
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

pub(crate) fn delta_status(s: git2::Delta) -> FileStatus {
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
    let Ok(repo) = Repository::discover(root) else {
        return Vec::new();
    };
    let Ok(oid) = git2::Oid::from_str(commit_id) else {
        return Vec::new();
    };
    let Ok(commit) = repo.find_commit(oid) else {
        return Vec::new();
    };
    let tree = commit.tree().ok();
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = DiffOptions::new();
    opts.pathspec(file);
    opts.context_lines(3);
    let Ok(diff) =
        repo.diff_tree_to_tree(parent_tree.as_ref(), tree.as_ref(), Some(&mut opts))
    else {
        return Vec::new();
    };
    collect_diff_lines(&diff)
}

/// Unified diff of one `file` between two arbitrary commits (PR review:
/// merge-base vs head).
pub fn range_file_diff(root: &Path, base_id: &str, head_id: &str, file: &str) -> Vec<DiffLine> {
    let Ok(repo) = Repository::discover(root) else {
        return Vec::new();
    };
    let tree_of = |id: &str| {
        git2::Oid::from_str(id)
            .ok()
            .and_then(|oid| repo.find_commit(oid).ok())
            .and_then(|c| c.tree().ok())
    };
    let (Some(base), Some(head)) = (tree_of(base_id), tree_of(head_id)) else {
        return Vec::new();
    };
    let mut opts = DiffOptions::new();
    opts.pathspec(file);
    opts.context_lines(3);
    let Ok(diff) = repo.diff_tree_to_tree(Some(&base), Some(&head), Some(&mut opts)) else {
        return Vec::new();
    };
    collect_diff_lines(&diff)
}

/// Unified diff of one file's uncommitted changes: HEAD vs the working tree,
/// staged + unstaged combined; untracked files render as all-added.
pub fn working_file_diff(root: &Path, file: &str) -> Vec<DiffLine> {
    let Ok(repo) = Repository::discover(root) else {
        return Vec::new();
    };
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let mut opts = DiffOptions::new();
    opts.pathspec(file);
    opts.context_lines(3);
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let Ok(diff) = repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut opts))
    else {
        return Vec::new();
    };
    collect_diff_lines(&diff)
}

/// Flatten a git2 diff into displayable lines via its foreach callbacks.
fn collect_diff_lines(diff: &git2::Diff<'_>) -> Vec<DiffLine> {
    let sink = std::cell::RefCell::new(Vec::new());
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

/// One entry in the branch switcher.
#[derive(Clone)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    /// Remote-only branch (no local counterpart); `name` has the remote
    /// prefix stripped so `git checkout <name>` creates a tracking branch.
    pub is_remote: bool,
}

/// Local + remote-only branches: current first, each group alphabetical.
pub fn branches(root: &Path) -> Vec<BranchInfo> {
    let Ok(repo) = Repository::discover(root) else {
        return Vec::new();
    };
    let head = current_branch(root);
    let mut local = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Ok(iter) = repo.branches(Some(git2::BranchType::Local)) {
        for (b, _) in iter.flatten() {
            if let Ok(Some(n)) = b.name() {
                seen.insert(n.to_string());
                local.push(BranchInfo {
                    name: n.to_string(),
                    is_current: head.as_deref() == Some(n),
                    is_remote: false,
                });
            }
        }
    }
    let mut remote = Vec::new();
    if let Ok(iter) = repo.branches(Some(git2::BranchType::Remote)) {
        for (b, _) in iter.flatten() {
            if let Ok(Some(full)) = b.name() {
                // "origin/feature/x" → "feature/x"; skip HEAD pointers and
                // branches that already exist locally.
                let Some((_, name)) = full.split_once('/') else {
                    continue;
                };
                if name == "HEAD" || seen.contains(name) {
                    continue;
                }
                seen.insert(name.to_string());
                remote.push(BranchInfo {
                    name: name.to_string(),
                    is_current: false,
                    is_remote: true,
                });
            }
        }
    }
    local.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then_with(|| a.name.cmp(&b.name))
    });
    remote.sort_by(|a, b| a.name.cmp(&b.name));
    local.extend(remote);
    local
}

/// Run a git subcommand in `root` via the system CLI, returning
/// (success, combined stdout+stderr).
pub fn run_git(root: &Path, args: &[String]) -> (bool, String) {
    match std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
    {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if !err.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            (o.status.success(), s)
        }
        Err(e) => (false, format!("git 실행 실패: {e}")),
    }
}

/// (ahead, behind) commit counts of HEAD vs its upstream, if it has one.
/// Cheap and local — reflects the last fetch, so pair with a background
/// fetch to detect new remote commits.
pub fn ahead_behind(root: &Path) -> Option<(usize, usize)> {
    let repo = Repository::discover(root).ok()?;
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    let local = head.target()?;
    let branch = git2::Branch::wrap(head);
    let upstream = branch.upstream().ok()?;
    let up = upstream.get().target()?;
    repo.graph_ahead_behind(local, up).ok()
}

/// Root of the MAIN working tree for `root`'s repository. For a linked
/// worktree (e.g. a CodeLook PR-review worktree) this is the original
/// checkout; for the main tree it's just its own root.
pub fn main_worktree_root(root: &Path) -> Option<PathBuf> {
    // --git-common-dir is the main repo's .git even from a linked worktree
    // (relative ".git" from the main tree itself).
    let (ok, out) = run_git(root, &["rev-parse".into(), "--git-common-dir".into()]);
    if !ok {
        return None;
    }
    let p = PathBuf::from(out.lines().next()?.trim());
    let p = if p.is_absolute() { p } else { root.join(p) };
    p.canonicalize().ok()?.parent().map(|x| x.to_path_buf())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch repo with two branches; verifies listing and CLI checkout.
    #[test]
    fn branches_and_checkout_roundtrip() {
        let dir = std::env::temp_dir().join(format!("codelook_git_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let (ok, out) = run_git(&dir, &owned);
            assert!(ok, "git {args:?} failed: {out}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        git(&["branch", "feature/x"]);

        let list = branches(&dir);
        let names: Vec<_> = list.iter().map(|b| b.name.as_str()).collect();
        assert!(names.contains(&"main") && names.contains(&"feature/x"), "{names:?}");
        assert!(list.iter().any(|b| b.name == "main" && b.is_current));
        assert_eq!(list[0].name, "main", "current branch sorts first");

        git(&["checkout", "-q", "feature/x"]);
        assert_eq!(current_branch(&dir).as_deref(), Some("feature/x"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
