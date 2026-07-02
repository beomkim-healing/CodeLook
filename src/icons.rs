//! File / folder / symbol icons. The primary set is JetBrains' IntelliJ
//! "New UI" (expUI) icons — bundled as pre-rasterized 64px PNGs (sources in
//! `assets/icons/svg/`, Apache-2.0, see THIRD_PARTY_NOTICES.md) and painted
//! as textures. File types without an official icon fall back to the
//! vector-drawn badge below, so nothing ever renders as a tofu box.

use std::collections::HashMap;

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, TextureHandle, Vec2};

use crate::ast::SymKind;

macro_rules! icon {
    ($n:literal) => {
        ($n, include_bytes!(concat!("../assets/icons/png/", $n, ".png")) as &[u8])
    };
}

/// All bundled expUI icons (64×64 RGBA PNG).
const ICONS: &[(&str, &[u8])] = &[
    icon!("archive"),
    icon!("c"),
    icon!("config"),
    icon!("cpp"),
    icon!("csharp"),
    icon!("css"),
    icon!("csv"),
    icon!("docker"),
    icon!("editorconfig"),
    icon!("exclude_root"),
    icon!("folder"),
    icon!("gitignore"),
    icon!("go"),
    icon!("gradle"),
    icon!("groovy"),
    icon!("h"),
    icon!("html"),
    icon!("http"),
    icon!("java"),
    icon!("javascript"),
    icon!("json"),
    icon!("jupyter"),
    icon!("kotlin"),
    icon!("kotlin_gradle"),
    icon!("kotlin_script"),
    icon!("markdown"),
    icon!("package"),
    icon!("patch"),
    icon!("perl"),
    icon!("properties"),
    icon!("python"),
    icon!("resources_root"),
    icon!("rst"),
    icon!("ruby"),
    icon!("rust"),
    icon!("shell"),
    icon!("source_root"),
    icon!("sql"),
    icon!("swift"),
    icon!("sym_annotation"),
    icon!("sym_class"),
    icon!("sym_class_abstract"),
    icon!("sym_constant"),
    icon!("sym_constructor"),
    icon!("sym_enum"),
    icon!("sym_field"),
    icon!("sym_function"),
    icon!("sym_interface"),
    icon!("sym_lambda"),
    icon!("sym_method"),
    icon!("sym_method_abstract"),
    icon!("sym_parameter"),
    icon!("sym_property"),
    icon!("sym_record"),
    icon!("sym_type"),
    icon!("sym_variable"),
    icon!("terraform"),
    icon!("test_resources_root"),
    icon!("test_root"),
    icon!("text"),
    icon!("toml"),
    icon!("typescript"),
    icon!("unknown"),
    icon!("vue"),
    icon!("xml"),
    icon!("yaml"),
];

fn decode_png(bytes: &[u8]) -> Option<egui::ColorImage> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let size = [info.width as usize, info.height as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        &buf[..info.buffer_size()],
    ))
}

/// GPU-resident icon textures, loaded once at startup.
pub struct IconSet {
    map: HashMap<&'static str, TextureHandle>,
}

impl IconSet {
    pub fn new(ctx: &egui::Context) -> Self {
        let mut map = HashMap::with_capacity(ICONS.len());
        for (name, bytes) in ICONS {
            if let Some(img) = decode_png(bytes) {
                let tex = ctx.load_texture(format!("icon:{name}"), img, egui::TextureOptions::LINEAR);
                map.insert(*name, tex);
            }
        }
        Self { map }
    }

    fn paint(&self, p: &egui::Painter, rect: Rect, key: &str) -> bool {
        let Some(tex) = self.map.get(key) else {
            return false;
        };
        let side = rect.height().min(rect.width()).min(16.0);
        let sq = Rect::from_center_size(rect.center(), Vec2::splat(side));
        p.image(
            tex.id(),
            sq,
            Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        true
    }

    /// File-type icon by file name; vector badge fallback for unmapped types.
    pub fn file(&self, p: &egui::Painter, rect: Rect, name: &str) {
        if !self.paint(p, rect, file_icon_key(name)) {
            draw_file_icon(p, rect, name);
        }
    }

    /// Folder icon by directory name (source/test/resources/excluded roots
    /// get their IntelliJ variants).
    pub fn folder(&self, p: &egui::Painter, rect: Rect, name: &str, expanded: bool) {
        if !self.paint(p, rect, folder_icon_key(name)) {
            draw_folder_icon(p, rect, expanded);
        }
    }

    /// Structure-panel symbol icon; returns false when the caller should
    /// fall back to the badge.
    pub fn symbol(&self, p: &egui::Painter, rect: Rect, kind: SymKind) -> bool {
        self.paint(p, rect, symbol_icon_key(kind))
    }
}

/// expUI icon key for a file name (special names first, then extension).
fn file_icon_key(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower == "dockerfile" || lower.starts_with("dockerfile.") || lower.starts_with("docker-compose") {
        return "docker";
    }
    if lower == ".gitignore" || lower == ".gitattributes" || lower == ".gitmodules" {
        return "gitignore";
    }
    if lower.ends_with(".gradle.kts") {
        return "kotlin_gradle";
    }
    if lower.ends_with(".gradle") || lower == "gradlew" || lower == "gradlew.bat" {
        return "gradle";
    }
    if lower == ".editorconfig" {
        return "editorconfig";
    }
    if lower == "license" || lower.starts_with("license.") || lower == "notice" {
        return "text";
    }
    match lower.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "kt" => "kotlin",
        "kts" => "kotlin_script",
        "java" => "java",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "go" => "go",
        "rb" | "rake" | "gemspec" => "ruby",
        "c" => "c",
        "h" => "h",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "pl" | "pm" => "perl",
        "groovy" => "groovy",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "properties" => "properties",
        "ini" | "cfg" | "conf" => "config",
        "xml" | "xsd" | "wsdl" | "plist" | "svg" => "xml",
        "html" | "htm" | "xhtml" | "jsp" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "csv" | "tsv" => "csv",
        "tf" | "tfvars" => "terraform",
        "http" | "rest" => "http",
        "ipynb" => "jupyter",
        "rst" => "rst",
        "patch" | "diff" => "patch",
        "vue" => "vue",
        "zip" | "tar" | "gz" | "tgz" | "jar" | "war" | "7z" | "rar" => "archive",
        "txt" | "log" | "lock" | "text" => "text",
        _ => "text",
    }
}

/// expUI folder icon key by directory name (IntelliJ root-type heuristics).
fn folder_icon_key(name: &str) -> &'static str {
    match name {
        "src" => "source_root",
        "test" | "tests" | "__tests__" | "testFixtures" | "androidTest" => "test_root",
        "resources" | "res" => "resources_root",
        "testResources" => "test_resources_root",
        "build" | "target" | "dist" | "out" | "node_modules" | ".gradle" | ".idea" => {
            "exclude_root"
        }
        _ => "folder",
    }
}

/// expUI node icon key for a structure symbol kind.
fn symbol_icon_key(kind: SymKind) -> &'static str {
    match kind {
        SymKind::Function => "sym_function",
        SymKind::Method => "sym_method",
        SymKind::Class | SymKind::Object => "sym_class",
        SymKind::Struct => "sym_record",
        SymKind::Enum => "sym_enum",
        SymKind::Trait | SymKind::Interface => "sym_interface",
        SymKind::Module => "package",
        SymKind::Const => "sym_constant",
        SymKind::Type => "sym_type",
        SymKind::Macro => "sym_lambda",
    }
}

/// Accent color for a file name based on its extension (tree dot / tab marker).
pub fn file_accent(name: &str) -> Color32 {
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");

    if lower.ends_with(".gradle.kts") || lower == "build.gradle" {
        return Color32::from_rgb(0x3d, 0xdc, 0x84);
    }

    match ext {
        "rs" => Color32::from_rgb(0xde, 0x8a, 0x4e),
        "kt" | "kts" => Color32::from_rgb(0xa9, 0x7b, 0xff),
        "java" => Color32::from_rgb(0xe7, 0x6f, 0x51),
        "py" => Color32::from_rgb(0x4b, 0x8b, 0xbe),
        "js" | "mjs" | "cjs" => Color32::from_rgb(0xf0, 0xdb, 0x4f),
        "ts" | "tsx" => Color32::from_rgb(0x31, 0x78, 0xc6),
        "jsx" => Color32::from_rgb(0x61, 0xda, 0xfb),
        "go" => Color32::from_rgb(0x00, 0xad, 0xd8),
        "rb" => Color32::from_rgb(0xcc, 0x34, 0x2d),
        "c" | "h" => Color32::from_rgb(0x55, 0x5b, 0xc4),
        "cpp" | "cc" | "hpp" | "cxx" => Color32::from_rgb(0x00, 0x59, 0x9c),
        "cs" => Color32::from_rgb(0x68, 0x21, 0x7a),
        "swift" => Color32::from_rgb(0xfa, 0x73, 0x43),
        "php" => Color32::from_rgb(0x77, 0x7b, 0xb3),
        "sh" | "bash" | "zsh" => Color32::from_rgb(0x4e, 0xaa, 0x25),
        "json" => Color32::from_rgb(0xcb, 0xa6, 0x4a),
        "yaml" | "yml" => Color32::from_rgb(0xcb, 0x60, 0x4a),
        "toml" | "ini" | "cfg" | "conf" => Color32::from_rgb(0x9c, 0x9c, 0x6e),
        "xml" | "html" | "htm" => Color32::from_rgb(0xe4, 0x4d, 0x26),
        "css" | "scss" | "sass" => Color32::from_rgb(0x26, 0x4d, 0xe4),
        "md" | "markdown" => Color32::from_rgb(0x6c, 0x9c, 0xd2),
        "sql" => Color32::from_rgb(0xe3, 0x8c, 0x00),
        "lua" => Color32::from_rgb(0x42, 0x6d, 0xc6),
        "gradle" => Color32::from_rgb(0x3d, 0xdc, 0x84),
        "lock" => Color32::from_rgb(0x88, 0x88, 0x88),
        "txt" | "log" => Color32::from_rgb(0xaa, 0xaa, 0xaa),
        _ => Color32::from_rgb(0x9a, 0xa0, 0xa6),
    }
}

/// Short uppercase-ish tag drawn inside the file badge. <= 3 glyphs.
pub fn file_tag(name: &str) -> &'static str {
    let lower = name.to_lowercase();
    if lower.ends_with(".gradle.kts") || lower == "build.gradle" || lower.ends_with(".gradle") {
        return "G";
    }
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rs",
        "kt" | "kts" => "kt",
        "java" => "J",
        "py" | "pyi" => "py",
        "js" | "mjs" | "cjs" => "js",
        "ts" => "ts",
        "tsx" => "ts",
        "jsx" => "js",
        "go" => "go",
        "rb" => "rb",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "cxx" => "c+",
        "cs" => "c#",
        "swift" => "sw",
        "php" => "ph",
        "sh" | "bash" | "zsh" => "$_",
        "json" => "{}",
        "yaml" | "yml" => "yml",
        "toml" => "tml",
        "ini" | "cfg" | "conf" => "cfg",
        "xml" => "<>",
        "html" | "htm" => "<>",
        "css" | "scss" | "sass" => "css",
        "md" | "markdown" => "M↓",
        "sql" => "sql",
        "lock" => "lk",
        "txt" | "log" => "≡",
        _ => "•",
    }
}

/// Draw a rounded file-type badge (IntelliJ-ish) centered in `rect`.
pub fn draw_file_icon(painter: &egui::Painter, rect: Rect, name: &str) {
    let accent = desaturate(file_accent(name), 0.20).gamma_multiply(0.92);
    // Badge sits in a square inside the row; keep a little vertical inset.
    let side = rect.height().min(rect.width()).min(16.0);
    let sq = Rect::from_center_size(rect.center(), Vec2::splat(side));
    let r = 3.0;
    painter.rect_filled(sq, r, accent);

    let tag = file_tag(name);
    let on_accent = if luminance(accent) > 0.6 {
        Color32::from_rgb(0x20, 0x22, 0x26)
    } else {
        Color32::from_rgb(0xf2, 0xf3, 0xf5)
    };
    let fsize = match tag.chars().count() {
        1 => side * 0.62,
        2 => side * 0.52,
        _ => side * 0.40,
    };
    painter.text(
        sq.center(),
        egui::Align2::CENTER_CENTER,
        tag,
        FontId::proportional(fsize),
        on_accent,
    );
}

/// Draw a folder glyph in `rect`.
pub fn draw_folder_icon(painter: &egui::Painter, rect: Rect, open: bool) {
    let c = Color32::from_rgb(0x9d, 0xa0, 0xa8);
    let side = rect.height().min(rect.width());
    let f = Rect::from_center_size(rect.center(), Vec2::new(side, side * 0.82));
    let r = side * 0.14;
    // Folder tab.
    let tab = Rect::from_min_size(
        Pos2::new(f.left(), f.top()),
        Vec2::new(side * 0.46, side * 0.2),
    );
    painter.rect_filled(tab, r * 0.6, c.gamma_multiply(0.85));
    // Body.
    let body = Rect::from_min_max(
        Pos2::new(f.left(), f.top() + side * 0.12),
        Pos2::new(f.right(), f.bottom()),
    );
    let fill = if open { c } else { c.gamma_multiply(0.78) };
    painter.rect_filled(body, r, fill);
    if open {
        // Lighter inner flap to suggest an open folder.
        let flap = Rect::from_min_max(
            Pos2::new(body.left() + side * 0.06, body.top() + side * 0.2),
            Pos2::new(body.right() - side * 0.02, body.bottom()),
        );
        painter.rect_filled(flap, r, c.gamma_multiply(1.25));
    }
}

/// Draw a disclosure chevron (▸ collapsed / ▾ expanded) in `rect`.
pub fn draw_chevron(painter: &egui::Painter, rect: Rect, expanded: bool, hovered: bool) {
    let c = if hovered {
        Color32::from_rgb(0xd0, 0xd2, 0xd6)
    } else {
        Color32::from_rgb(0x7a, 0x7e, 0x85)
    };
    let s = rect.height() * 0.26;
    let ctr = rect.center();
    let pts = if expanded {
        // pointing down
        vec![
            Pos2::new(ctr.x - s, ctr.y - s * 0.5),
            Pos2::new(ctr.x + s, ctr.y - s * 0.5),
            Pos2::new(ctr.x, ctr.y + s * 0.7),
        ]
    } else {
        // pointing right
        vec![
            Pos2::new(ctr.x - s * 0.5, ctr.y - s),
            Pos2::new(ctr.x - s * 0.5, ctr.y + s),
            Pos2::new(ctr.x + s * 0.7, ctr.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, c, Stroke::NONE));
}

fn luminance(c: Color32) -> f32 {
    let [r, g, b, _] = c.to_array();
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// Pull a color `t` (0..1) of the way toward its own gray luminance — softer,
/// less neon badge fills, closer to IntelliJ's muted chips.
fn desaturate(c: Color32, t: f32) -> Color32 {
    let [r, g, b, a] = c.to_array();
    let l = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32).round();
    let mix = |x: u8| (x as f32 * (1.0 - t) + l * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_unmultiplied(mix(r), mix(g), mix(b), a)
}
