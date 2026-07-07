//! Pull-request review sessions: review a PR against the WHOLE codebase, not
//! a clipped diff. The PR head is fetched (never checked out), its files are
//! read from the head commit's blobs, and the base…head changes become a
//! per-line overlay the editor paints on top of the full file.

use std::path::Path;
use std::process::Command;

use git2::Repository;

use crate::git::{FileDiff, FileStatus, LineChange};

pub struct PrMeta {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub base_branch: String,
    pub head_branch: String,
}

/// One file the PR touches.
pub struct ReviewFile {
    /// Repo-relative path (the new path for renames).
    pub path: String,
    pub status: FileStatus,
    pub added: usize,
    pub deleted: usize,
    /// Base…head line changes mapped onto the head file (editor overlay).
    pub overlay: FileDiff,
    /// Pure-deletion hunks: (head-line insertion anchor = number of head
    /// lines above the gap, deleted lines). Rendered as inline ghost rows.
    pub deleted_text: Vec<(usize, Vec<String>)>,
    pub viewed: bool,
}

/// A line comment queued locally; submitted together with the review.
#[derive(Clone)]
pub struct PendingComment {
    pub path: String,
    /// 1-based line in the PR head file (what GitHub's API expects).
    pub line: usize,
    /// 0-based display line in the ghost-merged editor doc (for markers).
    pub disp_line: usize,
    pub body: String,
}

pub struct ReviewSession {
    pub pr: PrMeta,
    /// merge-base(base, head) — what the PR's changes are measured against.
    pub base_id: String,
    pub head_id: String,
    /// The working tree is checked out at the PR head, so files on disk ARE
    /// the review content (⌘+Click and search all line up).
    pub head_is_workdir: bool,
    pub files: Vec<ReviewFile>,
    /// Line comments queued for submission.
    pub pending: Vec<PendingComment>,
}

impl ReviewSession {
    pub fn viewed_count(&self) -> usize {
        self.files.iter().filter(|f| f.viewed).count()
    }
}

/// PR title/author/branches via the `gh` CLI (uses the user's existing auth).
/// Absent or failing `gh` is fine — the session falls back to bare git data.
fn gh_meta(root: &Path, number: u64) -> Option<(String, String, String, String)> {
    let out = Command::new("gh")
        .current_dir(root)
        .args([
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "title,author,baseRefName,headRefName",
            "--template",
            "{{.title}}\n{{.author.login}}\n{{.baseRefName}}\n{{.headRefName}}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    let mut it = s.lines();
    Some((
        it.next()?.trim().to_string(),
        it.next()?.trim().to_string(),
        it.next()?.trim().to_string(),
        it.next()?.trim().to_string(),
    ))
}

/// Load a review session for GitHub PR `number`: fetch its head (a ref-only
/// fetch — the working tree is untouched), refresh the base branch, and diff
/// merge-base…head into per-file overlays.
pub fn load(root: &Path, number: u64) -> Result<ReviewSession, String> {
    let meta = gh_meta(root, number);

    let (ok, msg) = crate::git::run_git(
        root,
        &["fetch".into(), "origin".into(), format!("pull/{number}/head")],
    );
    if !ok {
        return Err(format!("PR fetch 실패: {msg}"));
    }
    let repo = Repository::discover(root).map_err(|e| e.to_string())?;
    let head_oid = repo
        .revparse_single("FETCH_HEAD")
        .map_err(|e| format!("PR head 확인 실패: {e}"))?
        .id();

    let base_branch = meta
        .as_ref()
        .map(|m| m.2.clone())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "HEAD".to_string());
    // Refresh the base ref so merge-base reflects the remote (FETCH_HEAD was
    // already resolved above, so this fetch overwriting it is harmless).
    let _ = crate::git::run_git(root, &["fetch".into(), "origin".into(), base_branch.clone()]);

    let pr = PrMeta {
        number,
        title: meta
            .as_ref()
            .map(|m| m.0.clone())
            .unwrap_or_else(|| format!("PR #{number}")),
        author: meta.as_ref().map(|m| m.1.clone()).unwrap_or_default(),
        base_branch: base_branch.clone(),
        head_branch: meta.as_ref().map(|m| m.3.clone()).unwrap_or_default(),
    };
    build_session(root, &format!("origin/{base_branch}"), &head_oid.to_string(), pr)
}

/// Build a session from two resolvable revspecs. Split out of `load` so tests
/// can exercise the diff/overlay logic on a local scratch repo without GitHub.
pub fn build_session(
    root: &Path,
    base_spec: &str,
    head_spec: &str,
    pr: PrMeta,
) -> Result<ReviewSession, String> {
    let repo = Repository::discover(root).map_err(|e| e.to_string())?;
    let head_oid = repo
        .revparse_single(head_spec)
        .and_then(|o| o.peel_to_commit())
        .map_err(|e| format!("head({head_spec}) 확인 실패: {e}"))?
        .id();
    let base_oid = repo
        .revparse_single(base_spec)
        .or_else(|_| repo.revparse_single("origin/HEAD"))
        .and_then(|o| o.peel_to_commit())
        .map_err(|e| format!("base({base_spec}) 확인 실패: {e}"))?
        .id();
    let merge_base = repo
        .merge_base(base_oid, head_oid)
        .map_err(|e| format!("merge-base 계산 실패: {e}"))?;

    let base_tree = repo
        .find_commit(merge_base)
        .and_then(|c| c.tree())
        .map_err(|e| e.to_string())?;
    let head_tree = repo
        .find_commit(head_oid)
        .and_then(|c| c.tree())
        .map_err(|e| e.to_string())?;

    // Context 0: every hunk maps exactly onto changed head lines, which is
    // what the editor overlay needs.
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(0);
    let diff = repo
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut opts))
        .map_err(|e| e.to_string())?;

    let files = std::cell::RefCell::new(Vec::<ReviewFile>::new());
    // Whether the current hunk is a pure deletion (its '-' lines become
    // inline ghost rows rather than replaced-by-new-code lines).
    let pure_del = std::cell::Cell::new(false);
    diff.foreach(
        &mut |delta, _| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if !path.is_empty() {
                files.borrow_mut().push(ReviewFile {
                    path,
                    status: crate::git::delta_status(delta.status()),
                    added: 0,
                    deleted: 0,
                    overlay: FileDiff::default(),
                    deleted_text: Vec::new(),
                    viewed: false,
                });
            }
            true
        },
        None,
        // Deltas arrive file-by-file, so the hunk/line callbacks always
        // target the most recently pushed file.
        Some(&mut |_d, hunk| {
            let mut fs = files.borrow_mut();
            let Some(f) = fs.last_mut() else { return true };
            let ns = hunk.new_start() as usize; // 1-based
            let nl = hunk.new_lines() as usize;
            let ol = hunk.old_lines() as usize;
            if nl == 0 {
                f.overlay.deleted_before.push(ns.saturating_sub(1));
                f.deleted_text.push((ns, Vec::new()));
                pure_del.set(true);
            } else {
                pure_del.set(false);
                let kind = if ol == 0 {
                    LineChange::Added
                } else {
                    LineChange::Modified
                };
                for l in 0..nl {
                    f.overlay.changed.push((ns - 1 + l, kind));
                }
            }
            true
        }),
        Some(&mut |_d, _h, line| {
            let mut fs = files.borrow_mut();
            let Some(f) = fs.last_mut() else { return true };
            match line.origin() {
                '+' => f.added += 1,
                '-' => {
                    f.deleted += 1;
                    if pure_del.get() {
                        if let Some((_, texts)) = f.deleted_text.last_mut() {
                            texts.push(
                                String::from_utf8_lossy(line.content())
                                    .trim_end_matches('\n')
                                    .to_string(),
                            );
                        }
                    }
                }
                _ => {}
            }
            true
        }),
    )
    .map_err(|e| e.to_string())?;

    let mut files = files.into_inner();
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let head_is_workdir = repo.head().ok().and_then(|h| h.target()) == Some(head_oid);
    Ok(ReviewSession {
        pr,
        base_id: merge_base.to_string(),
        head_id: head_oid.to_string(),
        head_is_workdir,
        files,
        pending: Vec::new(),
    })
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn gh_api_review(root: &Path, number: u64, payload: &str) -> Result<(), String> {
    let file = std::env::temp_dir().join(format!("codelook_review_{}.json", std::process::id()));
    std::fs::write(&file, payload).map_err(|e| e.to_string())?;
    let out = Command::new("gh")
        .current_dir(root)
        .args([
            "api",
            &format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews"),
            "--input",
        ])
        .arg(&file)
        .output()
        .map_err(|e| format!("gh 실행 실패: {e}"))?;
    std::fs::remove_file(&file).ok();
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(if err.is_empty() { body } else { err })
    }
}

/// Submit the review to GitHub via `gh`: event is "APPROVE" / "COMMENT" /
/// "REQUEST_CHANGES", with the queued line comments attached. GitHub only
/// accepts line comments inside the diff, so on rejection the comments are
/// folded into the review body (`path:line — text`) and submitted again.
pub fn submit_review(
    root: &Path,
    number: u64,
    head_id: &str,
    event: &str,
    body: &str,
    comments: &[PendingComment],
) -> Result<String, String> {
    let base = format!(
        "\"commit_id\":{},\"event\":{},\"body\":",
        json_str(head_id),
        json_str(event)
    );
    if !comments.is_empty() {
        let list = comments
            .iter()
            .map(|c| {
                format!(
                    "{{\"path\":{},\"line\":{},\"side\":\"RIGHT\",\"body\":{}}}",
                    json_str(&c.path),
                    c.line,
                    json_str(&c.body)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let payload = format!("{{{base}{},\"comments\":[{list}]}}", json_str(body));
        match gh_api_review(root, number, &payload) {
            Ok(()) => {
                return Ok(format!("{event} · 라인 코멘트 {}개", comments.len()));
            }
            Err(e) => {
                // Comments outside the diff → fold into the body and retry.
                let mut folded = body.to_string();
                for c in comments {
                    folded.push_str(&format!("\n\n**{}:{}**\n{}", c.path, c.line, c.body));
                }
                let payload = format!("{{{base}{}}}", json_str(folded.trim()));
                gh_api_review(root, number, &payload)
                    .map_err(|e2| format!("{e} / 본문 폴백도 실패: {e2}"))?;
                return Ok(format!(
                    "{event} · 코멘트 {}개는 본문에 병합됨(diff 밖 라인)",
                    comments.len()
                ));
            }
        }
    }
    let payload = format!("{{{base}{}}}", json_str(body));
    gh_api_review(root, number, &payload)?;
    Ok(event.to_string())
}

/// One row of the open-PR picker.
pub struct PrListItem {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_branch: String,
}

/// Open PRs of the repo's GitHub remote, newest first (via `gh`). With
/// `mine`, only PRs where the current user's review is requested (directly
/// or via a team) — the default, since a busy repo's full list is noise.
pub fn list_prs(root: &Path, mine: bool) -> Result<Vec<PrListItem>, String> {
    let mut args = vec![
        "pr".to_string(),
        "list".to_string(),
        "--limit".to_string(),
        "50".to_string(),
        "--json".to_string(),
        "number,author,headRefName,title".to_string(),
        // Title last: it may contain the separator, splitn keeps it whole.
        "--template".to_string(),
        "{{range .}}{{.number}}\t{{.author.login}}\t{{.headRefName}}\t{{.title}}\n{{end}}".to_string(),
    ];
    if mine {
        args.push("--search".to_string());
        args.push("review-requested:@me".to_string());
    }
    let out = Command::new("gh")
        .current_dir(root)
        .args(&args)
        .output()
        .map_err(|e| format!("gh 실행 실패: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "gh pr list 실패".to_string()
        } else {
            err
        });
    }
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    let mut list = Vec::new();
    for line in s.lines() {
        let mut it = line.splitn(4, '\t');
        let (Some(n), Some(author), Some(head), Some(title)) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let Ok(number) = n.trim().parse() else {
            continue;
        };
        list.push(PrListItem {
            number,
            title: title.trim().to_string(),
            author: author.trim().to_string(),
            head_branch: head.trim().to_string(),
        });
    }
    Ok(list)
}

/// Create (or refresh) a dedicated review worktree checked out at the PR
/// head, under `~/.codelook/worktrees/<repo>-pr-<N>`. The main working tree
/// is never touched; the worktree shares the repo's object store, so this is
/// a plain checkout, not a clone.
pub fn ensure_worktree(root: &Path, number: u64) -> Result<std::path::PathBuf, String> {
    let (ok, msg) = crate::git::run_git(
        root,
        &["fetch".into(), "origin".into(), format!("pull/{number}/head")],
    );
    if !ok {
        return Err(format!("PR fetch 실패: {msg}"));
    }
    let repo = Repository::discover(root).map_err(|e| e.to_string())?;
    let head = repo
        .revparse_single("FETCH_HEAD")
        .map_err(|e| format!("PR head 확인 실패: {e}"))?
        .id()
        .to_string();
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let home = std::env::var("HOME").map_err(|_| "HOME을 알 수 없음".to_string())?;
    let dir = std::path::PathBuf::from(home)
        .join(".codelook")
        .join("worktrees")
        .join(format!("{name}-pr-{number}"));

    if dir.join(".git").exists() {
        // Reuse the worktree, moving it to the (possibly newer) PR head.
        let (ok, msg) = crate::git::run_git(
            &dir,
            &["checkout".into(), "-q".into(), "--detach".into(), head],
        );
        if !ok {
            return Err(format!("워크트리 갱신 실패: {msg}"));
        }
        return Ok(dir);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let (ok, msg) = crate::git::run_git(
        root,
        &[
            "worktree".into(),
            "add".into(),
            "--detach".into(),
            dir.to_string_lossy().to_string(),
            head,
        ],
    );
    if !ok {
        return Err(format!("워크트리 생성 실패: {msg}"));
    }
    Ok(dir)
}

/// True when `dir` is a CodeLook-managed review worktree (the only kind we
/// ever delete automatically).
pub fn is_managed_worktree(dir: &Path) -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let base = std::path::PathBuf::from(home).join(".codelook").join("worktrees");
    dir.starts_with(&base) && dir != base
}

/// Remove a managed review worktree (checkout copy only — the repo's
/// objects/branches are untouched). Refuses anything outside
/// `~/.codelook/worktrees`.
pub fn remove_worktree(dir: &Path) -> Result<(), String> {
    if !is_managed_worktree(dir) {
        return Err("관리 대상 워크트리가 아닙니다".to_string());
    }
    let main = crate::git::main_worktree_root(dir).ok_or("원본 레포를 찾을 수 없음")?;
    let dir_s = dir.to_string_lossy().to_string();
    let (ok, _) = crate::git::run_git(
        &main,
        &["worktree".into(), "remove".into(), dir_s.clone()],
    );
    if ok {
        return Ok(());
    }
    // The review copy is disposable by definition — force past stray files.
    let (ok, msg) = crate::git::run_git(
        &main,
        &["worktree".into(), "remove".into(), "--force".into(), dir_s],
    );
    if ok {
        Ok(())
    } else {
        Err(msg)
    }
}

/// Full content of `rel` at `commit_id` — the review editor's file source
/// when the working tree isn't on the PR head.
pub fn file_at(root: &Path, commit_id: &str, rel: &str) -> Option<String> {
    let repo = Repository::discover(root).ok()?;
    let oid = git2::Oid::from_str(commit_id).ok()?;
    let tree = repo.find_commit(oid).ok()?.tree().ok()?;
    let entry = tree.get_path(Path::new(rel)).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    String::from_utf8(blob.content().to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> PrMeta {
        PrMeta {
            number: 1,
            title: "t".into(),
            author: "a".into(),
            base_branch: "main".into(),
            head_branch: "feature".into(),
        }
    }

    /// Scratch repo: main → feature adds a file, edits another, deletes lines.
    #[test]
    fn session_overlay_and_blob_content() {
        let dir = std::env::temp_dir().join(format!("codelook_review_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let (ok, out) = crate::git::run_git(&dir, &owned);
            assert!(ok, "git {args:?} failed: {out}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        git(&["add", "."]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        git(&["checkout", "-q", "-b", "feature"]);
        // Edit line 2, delete line 4, add a new file.
        std::fs::write(dir.join("a.txt"), "one\nTWO!\nthree\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new file\n").unwrap();
        git(&["add", "."]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "change"]);
        git(&["checkout", "-q", "main"]);

        let s = build_session(&dir, "main", "feature", meta()).expect("session");
        assert_eq!(s.files.len(), 2);
        let a = &s.files[0];
        assert_eq!(a.path, "a.txt");
        assert_eq!(a.status, FileStatus::Modified);
        // Line 2 modified (index 1), line 4 deleted after new line 3.
        assert!(a.overlay.changed.contains(&(1, LineChange::Modified)), "{:?}", a.overlay.changed);
        assert!(!a.overlay.deleted_before.is_empty());
        assert_eq!((a.added, a.deleted), (1, 2));
        // The pure-deletion hunk ("four" at end) carries its text for ghost
        // rows, anchored after the 3 remaining head lines.
        assert_eq!(a.deleted_text, vec![(3, vec!["four".to_string()])]);
        let b = &s.files[1];
        assert_eq!((b.path.as_str(), b.status), ("b.txt", FileStatus::Added));
        assert_eq!(b.overlay.changed, vec![(0, LineChange::Added)]);

        // Head-blob content, while the working tree sits on main.
        assert!(!s.head_is_workdir);
        assert_eq!(file_at(&dir, &s.head_id, "a.txt").as_deref(), Some("one\nTWO!\nthree\n"));
        assert_eq!(file_at(&dir, &s.head_id, "b.txt").as_deref(), Some("new file\n"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
