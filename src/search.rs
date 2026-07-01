//! Project-wide content search ("Find in Files"). Walks the project respecting
//! .gitignore, skips binaries / oversized files, and returns line matches.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

#[derive(Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: usize, // 0-based
    pub col: usize,  // byte offset of the match within the line
    pub text: String,
}

const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_HITS: usize = 2000;
const MAX_LINE_LEN: usize = 2000;

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

/// Case-insensitive substring search across the project. Returns the hits and
/// whether the result set was truncated at `MAX_HITS`.
pub fn global_search(root: &Path, query: &str) -> (Vec<SearchHit>, bool) {
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    if needle.is_empty() {
        return (hits, false);
    }

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
        .build();

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if path
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary / non-utf8
        };
        for (lineno, line) in content.lines().enumerate() {
            if line.len() > MAX_LINE_LEN {
                continue;
            }
            if let Some(col) = line.to_lowercase().find(&needle) {
                hits.push(SearchHit {
                    path: path.to_path_buf(),
                    line: lineno,
                    col,
                    text: line.trim_end().to_string(),
                });
                if hits.len() >= MAX_HITS {
                    return (hits, true);
                }
            }
        }
    }
    (hits, false)
}
