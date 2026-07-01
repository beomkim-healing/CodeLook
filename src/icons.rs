//! File-type icons drawn with the egui painter (no emoji / icon font needed),
//! plus the disclosure chevron and folder glyph. Keeps the tree looking like a
//! real IDE instead of falling back to tofu boxes for missing glyphs.

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, Vec2};

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
