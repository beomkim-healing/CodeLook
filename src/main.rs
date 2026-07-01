#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod ast;
mod git;
mod highlight;
mod icons;
mod search;
mod symbols;

use eframe::egui;
use std::path::PathBuf;

/// Optional capture mode for the design-review loop (compiled only with the
/// `shot` feature). Opens a project, optionally a file, renders a few frames,
/// writes a PNG of the window framebuffer, then quits.
#[derive(Clone, Default)]
pub struct ShotConfig {
    pub out: PathBuf,
    pub open: Option<PathBuf>,
    pub gsearch: Option<String>,
    pub log: bool,
}

fn main() -> eframe::Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let mut initial_path: Option<PathBuf> = None;
    let mut shot: Option<ShotConfig> = None;
    let mut open_file: Option<PathBuf> = None;
    let mut gsearch: Option<String> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--shot" => {
                let out = args.next().map(PathBuf::from).unwrap_or_default();
                shot = Some(ShotConfig { out, open: None, gsearch: None, log: false });
            }
            "--open" => open_file = args.next().map(PathBuf::from),
            "--gsearch" => gsearch = args.next(),
            "--log" => {
                if let Some(s) = shot.as_mut() {
                    s.log = true;
                }
            }
            other => {
                let p = PathBuf::from(other);
                if p.exists() {
                    initial_path = Some(p);
                }
            }
        }
    }
    if let Some(s) = shot.as_mut() {
        s.open = open_file;
        s.gsearch = gsearch;
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("CodeLook"),
        ..Default::default()
    };

    eframe::run_native(
        "CodeLook",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(app::CodeLookApp::new(
                cc,
                initial_path.clone(),
                shot.clone(),
            )))
        }),
    )
}
