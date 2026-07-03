//! Syntax highlighting.
//!
//! Languages with a tree-sitter grammar (Rust, Kotlin, Java, Python, Go, JS/TS)
//! are highlighted from their AST — the same approach IntelliJ uses — so even
//! grammars that syntect doesn't ship (e.g. Kotlin) get full coloring. Every
//! other file type falls back to syntect's TextMate grammars.

use std::cell::RefCell;
use std::collections::HashMap;

use eframe::egui::{text::LayoutJob, Color32, FontId, TextFormat};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use tree_sitter_highlight::{
    Highlight, HighlightConfiguration, HighlightEvent, Highlighter as TsHighlighter,
};

use crate::ast::Lang;

const DEFAULT_FG: Color32 = Color32::from_rgb(0xa9, 0xb7, 0xc6);

/// Editor line height, used for BOTH the code and the line-number gutter so the
/// two columns always align. ~1.35x is tighter than egui's default and reads
/// like an IDE without clipping JetBrains Mono ascenders/descenders.
pub fn line_height(font_size: f32) -> f32 {
    (font_size * 1.35).ceil()
}

/// Capture names we ask each grammar to map onto. `configure()` returns indices
/// into this slice, which `color_for` then turns into Darcula colors.
const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "comment.documentation",
    "constant",
    "constant.builtin",
    "constructor",
    "escape",
    "field",
    "function",
    "function.builtin",
    "function.method",
    "function.macro",
    "keyword",
    "label",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.member",
    "variable.parameter",
];

/// Hand-written highlights query for Kotlin (the crate ships none).
const KOTLIN_HIGHLIGHTS: &str = r#"
(line_comment) @comment
(block_comment) @comment

(string_literal) @string
(multiline_string_literal) @string
(character_literal) @string

(number_literal) @number
(float_literal) @number

(annotation) @attribute
(file_annotation) @attribute

(visibility_modifier) @keyword
(function_modifier) @keyword
(class_modifier) @keyword
(inheritance_modifier) @keyword
(member_modifier) @keyword
(property_modifier) @keyword
(parameter_modifier) @keyword
(platform_modifier) @keyword

(function_declaration (identifier) @function)
(call_expression (identifier) @function)
(class_declaration (identifier) @type)
(object_declaration (identifier) @type)

[
  "abstract" "actual" "annotation" "as" "by" "catch" "class" "companion"
  "const" "constructor" "crossinline" "data" "delegate" "do" "dynamic" "else"
  "enum" "expect" "external" "final" "finally" "for" "fun" "if"
  "import" "in" "infix" "init" "inline" "inner" "interface" "internal"
  "is" "lateinit" "noinline" "object" "open" "operator" "out" "override"
  "package" "private" "property" "protected" "public" "return" "sealed"
  "super" "suspend" "tailrec" "this" "throw" "try" "typealias" "val"
  "value" "var" "vararg" "when" "where" "while"
] @keyword
"#;

fn color_for(name: &str) -> (Color32, bool) {
    let c = Color32::from_rgb;
    match name {
        "comment" => (c(0x7a, 0x7e, 0x85), true),
        "comment.documentation" => (c(0x73, 0x8a, 0x6e), true),
        "variable.builtin" => (c(0xcc, 0x78, 0x32), true), // self, super, …
        "keyword" | "label" | "constant.builtin" | "escape" | "string.escape"
        | "string.special" | "type.builtin" => (c(0xcc, 0x78, 0x32), false),
        "string" => (c(0x6a, 0x87, 0x59), false),
        "number" => (c(0x68, 0x97, 0xbb), false),
        "function" | "function.builtin" | "function.method" | "function.macro"
        | "constructor" => (c(0xff, 0xc6, 0x6d), false),
        "constant" | "property" => (c(0x98, 0x76, 0xaa), false),
        // Variables & parameters: a light periwinkle so identifiers read as
        // "colored" (they previously fell through to the near-neutral default).
        "variable" => (c(0xac, 0xb8, 0xe6), false),
        "variable.parameter" => (c(0x93, 0xc9, 0xd6), false), // params a touch cyan
        "variable.member" | "field" => (c(0x98, 0x76, 0xaa), false),
        "attribute" => (c(0xbb, 0xb5, 0x29), false),
        "tag" => (c(0xe8, 0xbf, 0x6a), false),
        _ => (DEFAULT_FG, false),
    }
}

/// Build a tree-sitter highlight configuration for a language, if supported.
pub fn ts_config(lang: Lang) -> Option<HighlightConfiguration> {
    // TS/TSX highlight queries "inherit" the JavaScript ones upstream, so the
    // JS (and JSX) queries are prepended manually — tree-sitter-highlight
    // doesn't process `; inherits:` comments.
    let ts_query = || {
        format!(
            "{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        )
    };
    let tsx_query = || {
        format!(
            "{}\n{}\n{}",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        )
    };
    let (language, query): (tree_sitter::Language, std::borrow::Cow<'_, str>) = match lang {
        Lang::Rust => (
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::Python => (
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::JavaScript => (
            tree_sitter_javascript::LANGUAGE.into(),
            tree_sitter_javascript::HIGHLIGHT_QUERY.into(),
        ),
        Lang::TypeScript => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ts_query().into(),
        ),
        Lang::Tsx => (tree_sitter_typescript::LANGUAGE_TSX.into(), tsx_query().into()),
        Lang::Java => (
            tree_sitter_java::LANGUAGE.into(),
            tree_sitter_java::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::Go => (
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::HIGHLIGHTS_QUERY.into(),
        ),
        Lang::Kotlin => (tree_sitter_kotlin_ng::LANGUAGE.into(), KOTLIN_HIGHLIGHTS.into()),
        Lang::Json => return None, // syntect handles JSON well
    };
    let mut cfg = HighlightConfiguration::new(language, lang.label(), &query, "", "").ok()?;
    cfg.configure(HIGHLIGHT_NAMES);
    Some(cfg)
}

pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
    ts_configs: RefCell<HashMap<Lang, Option<HighlightConfiguration>>>,
    ts: RefCell<TsHighlighter>,
}

impl Highlighter {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        const DARCULA: &str = include_str!("../assets/darcula.tmTheme");
        let theme = ThemeSet::load_from_reader(&mut std::io::Cursor::new(DARCULA))
            .unwrap_or_else(|_| ThemeSet::load_defaults().themes["base16-ocean.dark"].clone());
        Self {
            syntax_set,
            theme,
            ts_configs: RefCell::new(HashMap::new()),
            ts: RefCell::new(TsHighlighter::new()),
        }
    }

    #[allow(dead_code)] // kept for callers that want the syntax theme's own bg
    pub fn background(&self) -> Color32 {
        self.theme
            .settings
            .background
            .map(|c| Color32::from_rgb(c.r, c.g, c.b))
            .unwrap_or(Color32::from_rgb(0x2b, 0x2b, 0x2b))
    }

    /// Highlight a file, preferring AST-based coloring when a grammar exists.
    pub fn highlight(&self, path: &str, code: &str, font_size: f32) -> LayoutJob {
        if let Some(lang) = Lang::from_path(std::path::Path::new(path)) {
            if let Some(job) = self.highlight_ts(lang, code, font_size) {
                return job;
            }
        }
        self.highlight_syntect(path, code, font_size)
    }

    fn highlight_ts(&self, lang: Lang, code: &str, font_size: f32) -> Option<LayoutJob> {
        let mut configs = self.ts_configs.borrow_mut();
        let cfg = configs.entry(lang).or_insert_with(|| ts_config(lang));
        let cfg = cfg.as_ref()?;

        let mut ts = self.ts.borrow_mut();
        let events = ts.highlight(cfg, code.as_bytes(), None, |_| None).ok()?;

        let font = FontId::monospace(font_size);
        let line_h = line_height(font_size);
        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let mut stack: Vec<usize> = Vec::new();

        for event in events {
            match event.ok()? {
                HighlightEvent::HighlightStart(Highlight(i)) => stack.push(i),
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let (color, italic) = stack
                        .last()
                        .map(|i| color_for(HIGHLIGHT_NAMES[*i]))
                        .unwrap_or((DEFAULT_FG, false));
                    let mut fmt = TextFormat::simple(font.clone(), color);
                    fmt.italics = italic;
                    fmt.line_height = Some(line_h);
                    job.append(&code[start..end], 0.0, fmt);
                }
            }
        }
        Some(job)
    }

    fn syntax_for(&self, path: &str) -> &SyntaxReference {
        let p = std::path::Path::new(path);
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        let name = p.file_name().and_then(|e| e.to_str()).unwrap_or("");
        let by_ext = match name {
            "Dockerfile" => self.syntax_set.find_syntax_by_name("Dockerfile"),
            _ => self.syntax_set.find_syntax_by_extension(ext),
        };
        by_ext.unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }

    fn highlight_syntect(&self, path: &str, code: &str, font_size: f32) -> LayoutJob {
        let syntax = self.syntax_for(path);
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let font = FontId::monospace(font_size);
        let line_h = line_height(font_size);

        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        for line in LinesWithEndings::from(code) {
            let ranges = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default();
            for (style, text) in ranges {
                let fg = style.foreground;
                let mut fmt = TextFormat::simple(font.clone(), Color32::from_rgb(fg.r, fg.g, fg.b));
                fmt.italics = style.font_style.contains(FontStyle::ITALIC);
                fmt.line_height = Some(line_h);
                job.append(text, 0.0, fmt);
            }
        }
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn darcula_theme_loads() {
        let h = Highlighter::new();
        assert_eq!(h.background(), Color32::from_rgb(0x2b, 0x2b, 0x2b));
    }

    #[test]
    fn syntect_distinct_colors() {
        let h = Highlighter::new();
        // .properties uses the syntect path.
        let job = h.highlight("app.properties", "# comment\nkey = value\n", 14.0);
        let colors: HashSet<_> = job.sections.iter().map(|s| s.format.color.to_array()).collect();
        assert!(colors.len() >= 2, "got {}", colors.len());
    }

    fn ts_colors(path: &str, code: &str) -> usize {
        let h = Highlighter::new();
        let job = h.highlight(path, code, 14.0);
        job.sections
            .iter()
            .map(|s| s.format.color.to_array())
            .collect::<HashSet<_>>()
            .len()
    }

    #[test]
    fn kotlin_config_compiles() {
        // If the query is malformed, this is None and Kotlin would be plain.
        assert!(ts_config(Lang::Kotlin).is_some(), "Kotlin highlights query failed to compile");
    }

    #[test]
    fn kotlin_is_colored() {
        let code = "package x\n\n// hi\nclass Foo {\n    fun bar(): Int { return 42 }\n    val s = \"hello\"\n}\n";
        let n = ts_colors("Foo.kt", code);
        assert!(n >= 4, "expected several Kotlin token colors, got {n}");
    }

    #[test]
    fn rust_and_java_colored() {
        assert!(ts_colors("a.rs", "fn main() { let n = 42; /* c */ }") >= 4);
        assert!(ts_colors("A.java", "class A { void m() { int n = 42; } }") >= 4);
    }

    #[test]
    fn typescript_and_tsx_colored() {
        // TS-specific syntax (interface / type annotations / generics) must
        // parse — this used to run through the JavaScript grammar and break.
        let ts = "interface Props { id: number }\ntype Alias = string | null\nconst f = (p: Props): Alias => `x${p.id}` // c\n";
        assert!(ts_colors("a.ts", ts) >= 4, "TS tokens not colored");
        let tsx = "interface P { n: number }\nexport function App(p: P) {\n  return <div className=\"a\">{p.n}</div>\n}\n";
        assert!(ts_colors("a.tsx", tsx) >= 4, "TSX tokens not colored");
    }

}
