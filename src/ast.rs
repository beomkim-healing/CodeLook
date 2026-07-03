//! A tiny in-process "language service" backed by real tree-sitter ASTs.
//!
//! Rather than running an external LSP server per language, we parse each file
//! into a concrete syntax tree and walk it to extract definitions. That powers
//! the Structure outline (document symbols) and AST-accurate go-to-definition.

use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::symbols::{SymbolIndex, SymbolLoc};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Java,
    Kotlin,
    Go,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Object,
    Module,
    Const,
    Type,
    Macro,
}

impl SymKind {
    pub fn glyph(&self) -> &'static str {
        match self {
            SymKind::Function => "ƒ",
            SymKind::Method => "m",
            SymKind::Class => "C",
            SymKind::Struct => "S",
            SymKind::Enum => "E",
            SymKind::Trait => "T",
            SymKind::Interface => "I",
            SymKind::Object => "O",
            SymKind::Module => "M",
            SymKind::Const => "c",
            SymKind::Type => "t",
            SymKind::Macro => "!",
        }
    }

    /// Accent color (r,g,b) for the outline glyph.
    pub fn rgb(&self) -> (u8, u8, u8) {
        match self {
            SymKind::Function | SymKind::Method => (0xc5, 0x95, 0xff), // purple
            SymKind::Class | SymKind::Object => (0xe5, 0xc0, 0x7b),    // yellow
            SymKind::Struct | SymKind::Enum => (0x7e, 0xc6, 0x99),     // green
            SymKind::Trait | SymKind::Interface => (0x61, 0xaf, 0xef), // blue
            SymKind::Module => (0xe0, 0x6c, 0x75),                     // red
            SymKind::Const => (0xd1, 0x9a, 0x66),                      // orange
            SymKind::Type => (0x56, 0xb6, 0xc2),                       // cyan
            SymKind::Macro => (0xab, 0xb2, 0xbf),                      // gray
        }
    }
}

#[derive(Clone)]
pub struct DocSymbol {
    pub name: String,
    pub kind: SymKind,
    pub line: usize, // 0-based
    pub depth: usize,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        Some(match ext {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "java" => Lang::Java,
            "kt" | "kts" => Lang::Kotlin,
            "go" => Lang::Go,
            "json" => Lang::Json,
            _ => return None,
        })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::Python => "Python",
            Lang::JavaScript => "JavaScript",
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::Java => "Java",
            Lang::Kotlin => "Kotlin",
            Lang::Go => "Go",
            Lang::Json => "JSON",
        }
    }

    fn ts(&self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Json => tree_sitter_json::LANGUAGE.into(),
        }
    }
}

/// Parse `src` and return its definitions in source order (with nesting depth).
pub fn document_symbols(lang: Lang, src: &str) -> Vec<DocSymbol> {
    if matches!(lang, Lang::Json) {
        return Vec::new();
    }
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts()).is_err() {
        return Vec::new();
    }
    let tree = match parser.parse(src, None) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    walk(tree.root_node(), lang, src.as_bytes(), 0, &mut out);
    out
}

/// Add a file's AST-derived definitions to the global go-to-definition index.
pub fn index_file(lang: Lang, path: &Path, src: &str, index: &mut SymbolIndex) {
    for s in document_symbols(lang, src) {
        index.entry(s.name).or_default().push(SymbolLoc {
            path: path.to_path_buf(),
            line: s.line,
        });
    }
}

fn walk(node: Node, lang: Lang, src: &[u8], depth: usize, out: &mut Vec<DocSymbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let mut next_depth = depth;
        if let Some((kind, name)) = def_of(&child, lang, src) {
            out.push(DocSymbol {
                name,
                kind,
                line: child.start_position().row,
                depth,
            });
            next_depth = depth + 1;
        }
        walk(child, lang, src, next_depth, out);
    }
}

fn name_of(node: &Node, src: &[u8]) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        if let Ok(t) = n.utf8_text(src) {
            return Some(t.to_string());
        }
    }
    // Fallback: first identifier-ish direct child (e.g. Go type_spec, anon nodes).
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find_map(|c| {
        let k = c.kind();
        if k.ends_with("identifier") || k == "type_identifier" {
            c.utf8_text(src).ok().map(|s| s.to_string())
        } else if k == "type_spec" {
            c.child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.to_string())
        } else {
            None
        }
    });
    found
}

fn def_of(node: &Node, lang: Lang, src: &[u8]) -> Option<(SymKind, String)> {
    use Lang::*;
    use SymKind::*;
    let k = node.kind();
    let kind = match lang {
        Rust => match k {
            "function_item" => Function,
            "struct_item" => Struct,
            "enum_item" => Enum,
            "trait_item" => Trait,
            "mod_item" => Module,
            "type_item" => Type,
            "const_item" | "static_item" => Const,
            "macro_definition" => Macro,
            _ => return None,
        },
        Python => match k {
            "function_definition" => Function,
            "class_definition" => Class,
            _ => return None,
        },
        JavaScript | TypeScript | Tsx => match k {
            "function_declaration" | "generator_function_declaration" => Function,
            "class_declaration" | "abstract_class_declaration" => Class,
            "method_definition" | "method_signature" => Method,
            "interface_declaration" => Interface,
            "enum_declaration" => Enum,
            "type_alias_declaration" => Type,
            // `const App = () => …` / `const f = function …` — the dominant
            // way frontend code defines components and functions.
            "variable_declarator" => {
                let is_fn = node
                    .child_by_field_name("value")
                    .map(|v| {
                        matches!(
                            v.kind(),
                            "arrow_function" | "function_expression" | "generator_function"
                        )
                    })
                    .unwrap_or(false);
                if !is_fn {
                    return None;
                }
                Function
            }
            _ => return None,
        },
        Java => match k {
            "class_declaration" => Class,
            "interface_declaration" => Interface,
            "enum_declaration" => Enum,
            "record_declaration" => Class,
            "method_declaration" => Method,
            "constructor_declaration" => Method,
            _ => return None,
        },
        Kotlin => match k {
            "class_declaration" => Class,
            "object_declaration" => Object,
            "function_declaration" => Function,
            _ => return None,
        },
        Go => match k {
            "function_declaration" => Function,
            "method_declaration" => Method,
            "type_declaration" => Type,
            _ => return None,
        },
        Json => return None,
    };
    let name = name_of(node, src)?;
    if name.is_empty() {
        return None;
    }
    Some((kind, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Lang, src: &str) -> Vec<String> {
        document_symbols(lang, src)
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn rust_symbols() {
        let n = names(
            Lang::Rust,
            "fn foo() {}\nstruct Bar {}\nenum Baz {}\ntrait T {}\nimpl Bar { fn method(&self) {} }",
        );
        for want in ["foo", "Bar", "Baz", "T", "method"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
    }

    #[test]
    fn kotlin_symbols() {
        let n = names(
            Lang::Kotlin,
            "class Foo {\n  fun bar() {}\n}\nobject Singleton {}\nfun topLevel() {}",
        );
        for want in ["Foo", "bar", "Singleton", "topLevel"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
    }

    #[test]
    fn python_symbols() {
        let n = names(
            Lang::Python,
            "def foo():\n    pass\nclass Bar:\n    def method(self):\n        pass",
        );
        for want in ["foo", "Bar", "method"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
    }

    #[test]
    fn java_symbols() {
        let n = names(
            Lang::Java,
            "class Foo {\n  void bar() {}\n}\ninterface I {}\nenum E { A, B }",
        );
        for want in ["Foo", "bar", "I", "E"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
    }

    #[test]
    fn typescript_and_tsx_symbols() {
        let n = names(
            Lang::TypeScript,
            "interface P { n: number }\ntype A = string\nenum E { X }\nfunction f() {}\nconst g = () => 1\nclass C { m() {} }",
        );
        for want in ["P", "A", "E", "f", "g", "C", "m"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
        let n = names(
            Lang::Tsx,
            "const App = (p: {n: number}) => <div>{p.n}</div>\nexport function Page() { return <App n={1} /> }",
        );
        for want in ["App", "Page"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
    }

    #[test]
    fn go_symbols() {
        let n = names(
            Lang::Go,
            "package main\nfunc Foo() {}\ntype Bar struct {}\nfunc (b Bar) Method() {}",
        );
        for want in ["Foo", "Method"] {
            assert!(n.iter().any(|x| x == want), "missing {want} in {n:?}");
        }
    }
}
