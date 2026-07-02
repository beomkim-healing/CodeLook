//! Project-wide content search ("Find in Files").
//!
//! A persistent worker thread owns an in-memory snapshot of every searchable
//! text file. Each query revalidates the snapshot by mtime+size — only new or
//! changed files touch the disk — then scans it in parallel with a
//! SIMD-accelerated (memchr) matcher. Steady state, a keystroke costs memory
//! scans instead of a full walk + read of the whole tree, which is what keeps
//! search-as-you-type interactive on large projects.
//!
//! Case folding is ASCII-only: non-ASCII text (Korean/CJK — caseless anyway)
//! is matched exactly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ignore::{WalkBuilder, WalkState};

#[derive(Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: usize, // 0-based
    pub col: usize,  // byte offset of the match within the line
    pub text: String,
}

/// A finished search: the query that produced it, its hits, and whether the
/// result set was truncated at `MAX_HITS`.
pub type Reply = (String, Vec<SearchHit>, bool);

const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_HITS: usize = 2000;
const MAX_LINE_LEN: usize = 2000;
/// Total file content kept resident in the snapshot. Files beyond this budget
/// are still searched, but by streaming reads instead of from memory.
const MAX_INDEX_BYTES: usize = 512 * 1024 * 1024;

/// Directories never worth searching (build output, VCS, deps, caches).
const PRUNE_DIRS: &[&str] = &[
    ".git",
    ".gradle",
    ".idea",
    ".vscode",
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    ".next",
    "__pycache__",
];

/// Extensions that are always binary — excluded during the walk so they are
/// never read or re-stat'd as search candidates.
const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "icns", "bmp", "tiff", "heic", "mp4", "mov",
    "mp3", "wav", "ogg", "zip", "gz", "tgz", "bz2", "xz", "7z", "rar", "jar", "war", "class",
    "so", "dylib", "dll", "a", "o", "bin", "exe", "pdf", "woff", "woff2", "ttf", "otf", "eot",
    "db", "sqlite", "pyc", "wasm", "der", "p12", "jks", "keystore",
];

fn is_binary_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTS.iter().any(|b| e.eq_ignore_ascii_case(b)))
        .unwrap_or(false)
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 16)
}

// ---- Snapshot ---------------------------------------------------------------

enum Body {
    /// Cached text content.
    Text(Arc<str>),
    /// Valid text file over the memory budget — re-read on every search.
    Uncached,
    /// Binary or unreadable — ignored until its mtime/size changes.
    Skip,
}

struct FileEntry {
    mtime: Option<SystemTime>,
    size: u64,
    body: Body,
}

#[derive(Default)]
struct Index {
    files: HashMap<PathBuf, FileEntry>,
}

/// Read a file as text; `None` for binary (NUL byte in the head) or unreadable.
fn load_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let head = &bytes[..bytes.len().min(8192)];
    if memchr::memchr(0, head).is_some() {
        return None;
    }
    Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Bring the snapshot up to date with the tree on disk. Unchanged files
/// (same mtime + size) are reused; only new/changed ones are re-read, in
/// parallel, smallest first so the memory budget caches as many files as
/// possible. Cancelling mid-refresh leaves a partial snapshot that the next
/// refresh completes. Returns whether any content actually changed (new,
/// modified or deleted files) — callers use this to skip a redundant
/// re-search.
fn refresh(root: &Path, index: &mut Index, cancel: &AtomicBool) -> bool {
    // Pass 1: parallel walk collecting (path, mtime, size) of candidates.
    let found: Mutex<Vec<(PathBuf, Option<SystemTime>, u64)>> = Mutex::new(Vec::new());
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .parents(true)
        .filter_entry(|e| {
            // Prune known build/VCS/dependency directories (matches the tree).
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = e.file_name().to_str() {
                    return !PRUNE_DIRS.contains(&name);
                }
            }
            true
        })
        .threads(threads())
        .build_parallel();
    walker.run(|| {
        Box::new(|result| {
            if cancel.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let entry = match result {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }
            if is_binary_ext(entry.path()) {
                return WalkState::Continue;
            }
            let md = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return WalkState::Continue,
            };
            if md.len() > MAX_FILE_BYTES {
                return WalkState::Continue;
            }
            found
                .lock()
                .unwrap()
                .push((entry.into_path(), md.modified().ok(), md.len()));
            WalkState::Continue
        })
    });
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    let found = found.into_inner().unwrap();

    // Pass 2: keep unchanged cached entries. Unchanged `Uncached` ones
    // re-enter the budget decision below (they may fit now); new/changed
    // files always need a read.
    let mut files = HashMap::with_capacity(found.len());
    let mut candidates: Vec<(PathBuf, Option<SystemTime>, u64, bool)> = Vec::new(); // .3 = changed
    for (path, mtime, size) in found {
        match index.files.remove(&path) {
            Some(e) if mtime.is_some() && e.mtime == mtime && e.size == size => match e.body {
                Body::Uncached => candidates.push((path, mtime, size, false)),
                _ => {
                    files.insert(path, e);
                }
            },
            _ => candidates.push((path, mtime, size, true)),
        }
    }
    let changed =
        !index.files.is_empty() || candidates.iter().any(|(_, _, _, changed)| *changed);

    // Pass 3: split candidates by the memory budget — smallest first, decided
    // on stat size so over-budget files are never read here — then read the
    // cacheable ones in parallel.
    let resident_base: usize = files
        .values()
        .map(|e| match &e.body {
            Body::Text(s) => s.len(),
            _ => 0,
        })
        .sum();
    candidates.sort_unstable_by_key(|(_, _, size, _)| *size);
    let mut budget = MAX_INDEX_BYTES.saturating_sub(resident_base);
    let mut split = candidates.len();
    for (i, (_, _, size, _)) in candidates.iter().enumerate() {
        match budget.checked_sub(*size as usize) {
            Some(rest) => budget = rest,
            None => {
                split = i;
                break;
            }
        }
    }
    for (path, mtime, size, _) in &candidates[split..] {
        files.insert(
            path.clone(),
            FileEntry {
                mtime: *mtime,
                size: *size,
                body: Body::Uncached,
            },
        );
    }
    let to_read = &candidates[..split];

    let next = AtomicUsize::new(0);
    let read: Mutex<Vec<(PathBuf, FileEntry)>> = Mutex::new(Vec::with_capacity(to_read.len()));
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                let mut local = Vec::new();
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((path, mtime, size, _)) = to_read.get(i) else {
                        break;
                    };
                    let body = match load_text(path) {
                        Some(text) => Body::Text(text.into()),
                        None => Body::Skip,
                    };
                    local.push((
                        path.clone(),
                        FileEntry {
                            mtime: *mtime,
                            size: *size,
                            body,
                        },
                    ));
                }
                read.lock().unwrap().extend(local);
            });
        }
    });
    for (p, e) in read.into_inner().unwrap() {
        files.insert(p, e);
    }
    index.files = files; // deleted files drop out naturally
    changed
}

// ---- Matching ---------------------------------------------------------------

/// Byte offset of the next occurrence of `needle` in `hay[from..]`, matching
/// ASCII letters case-insensitively (non-ASCII bytes must match exactly).
/// Returned offsets always fall on char boundaries when both sides are UTF-8.
pub fn find_ci(hay: &[u8], needle: &[u8], mut from: usize) -> Option<usize> {
    let n = needle.len();
    if n == 0 {
        return None;
    }
    let lo = needle[0].to_ascii_lowercase();
    let up = needle[0].to_ascii_uppercase();
    while from + n <= hay.len() {
        let rel = if lo == up {
            memchr::memchr(lo, &hay[from..])
        } else {
            memchr::memchr2(lo, up, &hay[from..])
        }?;
        let p = from + rel;
        if p + n > hay.len() {
            return None;
        }
        if hay[p..p + n].eq_ignore_ascii_case(needle) {
            return Some(p);
        }
        from = p + 1;
    }
    None
}

/// All non-overlapping occurrences of `needle` in `hay` (ASCII
/// case-insensitive), capped at `max`. Used by the UI to place match markers.
pub fn find_all_ci(hay: &[u8], needle: &[u8], max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while out.len() < max {
        match find_ci(hay, needle, from) {
            Some(p) => {
                out.push(p);
                from = p + needle.len();
            }
            None => break,
        }
    }
    out
}

/// Scan one file's content, emitting at most one hit per line (the leftmost
/// match). Line numbers are counted incrementally with memchr, so a file with
/// no matches costs a single SIMD pass.
fn scan(path: &Path, content: &str, needle: &[u8], out: &mut Vec<SearchHit>, budget: &AtomicUsize) {
    let hay = content.as_bytes();
    let mut pos = 0usize;
    let mut line = 0usize;
    let mut counted_to = 0usize;
    while let Some(p) = find_ci(hay, needle, pos) {
        line += memchr::memchr_iter(b'\n', &hay[counted_to..p]).count();
        counted_to = p;
        let ls = memchr::memrchr(b'\n', &hay[..p]).map_or(0, |i| i + 1);
        let le = memchr::memchr(b'\n', &hay[p..]).map_or(hay.len(), |i| p + i);
        pos = le + 1; // one hit per line: continue on the next line
        if le - ls > MAX_LINE_LEN {
            continue;
        }
        out.push(SearchHit {
            path: path.to_path_buf(),
            line,
            col: p - ls,
            text: content[ls..le].trim_end().to_string(),
        });
        if budget.fetch_add(1, Ordering::Relaxed) + 1 >= MAX_HITS {
            return;
        }
    }
}

/// Search the snapshot in parallel. Hits are sorted by (path, line) so the
/// result list groups per file regardless of scan order.
fn search_index(index: &Index, query: &str, cancel: &AtomicBool) -> (Vec<SearchHit>, bool) {
    let needle = query.as_bytes();
    if needle.is_empty() {
        return (Vec::new(), false);
    }
    let mut entries: Vec<(&Path, &FileEntry)> =
        index.files.iter().map(|(p, e)| (p.as_path(), e)).collect();
    entries.sort_unstable_by_key(|(p, _)| *p);

    let budget = AtomicUsize::new(0);
    let next = AtomicUsize::new(0);
    let all: Mutex<Vec<SearchHit>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                let mut local = Vec::new();
                loop {
                    if cancel.load(Ordering::Relaxed) || budget.load(Ordering::Relaxed) >= MAX_HITS
                    {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((path, entry)) = entries.get(i) else {
                        break;
                    };
                    match &entry.body {
                        Body::Text(content) => scan(path, content, needle, &mut local, &budget),
                        Body::Uncached => {
                            if let Some(content) = load_text(path) {
                                scan(path, &content, needle, &mut local, &budget);
                            }
                        }
                        Body::Skip => {}
                    }
                }
                all.lock().unwrap().extend(local);
            });
        }
    });
    let mut hits = all.into_inner().unwrap();
    hits.sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    let truncated = hits.len() >= MAX_HITS;
    hits.truncate(MAX_HITS);
    (hits, truncated)
}

// ---- Worker -----------------------------------------------------------------

struct Job {
    query: String,
    cancel: Arc<AtomicBool>,
}

/// Handle to the persistent search worker. Dropping it shuts the worker down
/// (cancel the in-flight job first via the flag returned by [`Worker::submit`]).
pub struct Worker {
    tx: Sender<Job>,
}

impl Worker {
    /// Spawn the worker for `root`. `notify` is called after each reply is
    /// queued (used to request a UI repaint). Replies arrive on the returned
    /// channel tagged with their query, so stale ones are cheap to drop.
    ///
    /// A query is answered from the existing snapshot immediately; if the
    /// snapshot is older than `REFRESH_TTL` it is then revalidated in the
    /// background and a corrected reply is sent only when the tree actually
    /// changed. This keeps typing bursts free of per-keystroke disk walks.
    pub fn new(root: PathBuf, notify: impl Fn() + Send + 'static) -> (Self, Receiver<Reply>) {
        const REFRESH_TTL: std::time::Duration = std::time::Duration::from_secs(3);
        let (jtx, jrx) = mpsc::channel::<Job>();
        let (rtx, rrx) = mpsc::channel::<Reply>();
        std::thread::spawn(move || {
            let mut index = Index::default();
            let mut refreshed_at: Option<std::time::Instant> = None;
            while let Ok(mut job) = jrx.recv() {
                // Coalesce typing bursts: only the newest queued query matters.
                while let Ok(newer) = jrx.try_recv() {
                    job = newer;
                }
                if job.cancel.load(Ordering::Relaxed) {
                    continue;
                }
                // Instant reply from the current snapshot, if there is one.
                let mut replied = false;
                if !index.files.is_empty() && !job.query.is_empty() {
                    let (hits, truncated) = search_index(&index, &job.query, &job.cancel);
                    if job.cancel.load(Ordering::Relaxed) {
                        continue;
                    }
                    if rtx.send((job.query.clone(), hits, truncated)).is_err() {
                        break;
                    }
                    notify();
                    replied = true;
                }
                // Revalidate the snapshot when it is missing or stale.
                let fresh = refreshed_at.is_some_and(|t| t.elapsed() < REFRESH_TTL);
                if !fresh {
                    let changed = refresh(&root, &mut index, &job.cancel);
                    if job.cancel.load(Ordering::Relaxed) {
                        continue; // partial refresh: leave refreshed_at unset
                    }
                    refreshed_at = Some(std::time::Instant::now());
                    if !job.query.is_empty() && (changed || !replied) {
                        let (hits, truncated) = search_index(&index, &job.query, &job.cancel);
                        if job.cancel.load(Ordering::Relaxed) {
                            continue;
                        }
                        if rtx.send((job.query, hits, truncated)).is_err() {
                            break;
                        }
                        notify();
                    }
                }
            }
        });
        (Self { tx: jtx }, rrx)
    }

    /// Queue a search and return its cancellation flag. An empty query only
    /// warms the file snapshot (no reply is sent).
    pub fn submit(&self, query: String) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        let _ = self.tx.send(Job {
            query,
            cancel: Arc::clone(&cancel),
        });
        cancel
    }
}

/// One-shot synchronous search (capture mode). Builds a throwaway snapshot.
pub fn global_search(root: &Path, query: &str) -> (Vec<SearchHit>, bool) {
    let mut index = Index::default();
    let cancel = AtomicBool::new(false);
    refresh(root, &mut index, &cancel);
    search_index(&index, query, &cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_ci_ascii_case_insensitive() {
        let hay = b"Foo bar FOO baz foo";
        assert_eq!(find_ci(hay, b"foo", 0), Some(0));
        assert_eq!(find_ci(hay, b"foo", 1), Some(8));
        assert_eq!(find_ci(hay, b"FOO", 9), Some(16));
        assert_eq!(find_ci(hay, b"qux", 0), None);
        assert_eq!(find_all_ci(hay, b"foo", 10), vec![0, 8, 16]);
    }

    #[test]
    fn find_ci_non_ascii_exact() {
        let hay = "한글 검색 테스트 검색".as_bytes();
        let needle = "검색".as_bytes();
        assert_eq!(find_all_ci(hay, needle, 10).len(), 2);
        // Offsets fall on char boundaries.
        let p = find_ci(hay, needle, 0).unwrap();
        assert_eq!(&hay[p..p + needle.len()], needle);
    }

    #[test]
    fn scan_lines_and_cols() {
        let content = "alpha\nbeta ALPHA alpha\n\tALPHA\n";
        let mut hits = Vec::new();
        let budget = AtomicUsize::new(0);
        scan(Path::new("x"), content, b"alpha", &mut hits, &budget);
        // One hit per line, leftmost match, correct line/col.
        assert_eq!(hits.len(), 3);
        assert_eq!((hits[0].line, hits[0].col), (0, 0));
        assert_eq!((hits[1].line, hits[1].col), (1, 5));
        assert_eq!((hits[2].line, hits[2].col), (2, 1));
        assert_eq!(hits[1].text, "beta ALPHA alpha");
    }

    /// Perf harness (not run by default):
    /// `BENCH_DIR=/path/to/repo cargo test --release bench_search -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_search() {
        let root = PathBuf::from(std::env::var("BENCH_DIR").expect("set BENCH_DIR"));
        let query = std::env::var("BENCH_QUERY").unwrap_or_else(|_| "TODO".into());
        let cancel = AtomicBool::new(false);
        let mut index = Index::default();

        let t = std::time::Instant::now();
        refresh(&root, &mut index, &cancel);
        let t_cold = t.elapsed();

        let (mut n_text, mut n_uncached, mut n_skip, mut resident) = (0usize, 0usize, 0usize, 0usize);
        for e in index.files.values() {
            match &e.body {
                Body::Text(s) => {
                    n_text += 1;
                    resident += s.len();
                }
                Body::Uncached => n_uncached += 1,
                Body::Skip => n_skip += 1,
            }
        }
        println!(
            "snapshot: files={} text={} uncached={} skip={} resident={}MB (cold build {t_cold:?})",
            index.files.len(),
            n_text,
            n_uncached,
            n_skip,
            resident / (1024 * 1024)
        );

        let t = std::time::Instant::now();
        refresh(&root, &mut index, &cancel); // mtime revalidation only
        let t_reval = t.elapsed();
        let t = std::time::Instant::now();
        let (hits, truncated) = search_index(&index, &query, &cancel);
        let t_scan = t.elapsed();
        let t = std::time::Instant::now();
        search_index(&index, "qzxqzxnohit", &cancel); // no early exit
        let t_scan_miss = t.elapsed();
        println!(
            "query={query:?} hits={} trunc={truncated} | scan={t_scan:?} scan_nohit={t_scan_miss:?} revalidate={t_reval:?}",
            hits.len()
        );
    }

    #[test]
    fn global_search_end_to_end() {
        let dir = std::env::temp_dir().join(format!("codelook_search_test_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello world\nHELLO again\n").unwrap();
        std::fs::write(dir.join("sub/b.rs"), "fn hello() {}\n").unwrap();
        std::fs::write(dir.join("bin.dat"), b"\x00\x01hello\x00").unwrap(); // binary → skipped
        let (hits, truncated) = global_search(&dir, "hello");
        assert!(!truncated);
        assert_eq!(hits.len(), 3);
        // Sorted by path: a.txt lines 0,1 then sub/b.rs.
        assert!(hits[0].path.ends_with("a.txt") && hits[0].line == 0);
        assert!(hits[1].path.ends_with("a.txt") && hits[1].line == 1);
        assert!(hits[2].path.ends_with("b.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
