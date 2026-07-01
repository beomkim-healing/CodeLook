//! Lightweight, language-agnostic symbol index built by scanning files with a
//! handful of definition regexes. Powers ⌘+Click "go to definition".

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::Regex;

#[derive(Clone)]
pub struct SymbolLoc {
    pub path: PathBuf,
    pub line: usize, // 0-based
}

pub type SymbolIndex = HashMap<String, Vec<SymbolLoc>>;

const MAX_FILE_BYTES: u64 = 2_000_000;

fn definition_patterns() -> Vec<Regex> {
    // Capture group 1 is always the symbol name. These are heuristics applied
    // across all languages — good enough to jump to a definition.
    [
        r"\b(?:fn|func|def|function)\s+([A-Za-z_][A-Za-z0-9_]*)",
        r"\b(?:class|struct|enum|trait|interface|object|protocol|record)\s+([A-Za-z_][A-Za-z0-9_]*)",
        r"\b(?:type|typealias)\s+([A-Za-z_][A-Za-z0-9_]*)",
        r"\b(?:const|val|var|let|static)\s+([A-Za-z_][A-Za-z0-9_]*)",
        r"\bmacro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
}

pub fn build_index(root: &Path) -> SymbolIndex {
    let patterns = definition_patterns();
    let mut index: SymbolIndex = HashMap::new();

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .parents(true)
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
        if path.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary / non-utf8
        };
        // Prefer AST-based extraction; fall back to regex for languages without
        // a tree-sitter grammar wired up.
        match crate::ast::Lang::from_path(path) {
            Some(lang) => crate::ast::index_file(lang, path, &content, &mut index),
            None => scan(path, &content, &patterns, &mut index),
        }
    }
    index
}

fn scan(path: &Path, content: &str, patterns: &[Regex], index: &mut SymbolIndex) {
    for (lineno, line) in content.lines().enumerate() {
        // Cheap skip for obvious noise.
        if line.len() > 400 {
            continue;
        }
        for re in patterns {
            if let Some(c) = re.captures(line) {
                if let Some(m) = c.get(1) {
                    index
                        .entry(m.as_str().to_string())
                        .or_default()
                        .push(SymbolLoc {
                            path: path.to_path_buf(),
                            line: lineno,
                        });
                }
            }
        }
    }
}
