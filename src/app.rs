use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CursorIcon, FontId, Key, Rect, Sense, Stroke,
    TextFormat, Vec2,
};

use crate::ast::{self, DocSymbol};
use crate::highlight::Highlighter;
use crate::icons;
use crate::symbols::{self, SymbolIndex};

const IGNORE_DIRS: &[&str] = &[
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

/// A directory/file node in the project tree. Children are loaded lazily.
struct TreeNode {
    path: PathBuf,
    name: String,
    is_dir: bool,
    expanded: bool,
    children: Option<Vec<TreeNode>>,
}

impl TreeNode {
    fn new(path: PathBuf, is_dir: bool) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        Self {
            path,
            name,
            is_dir,
            expanded: false,
            children: None,
        }
    }
}

/// An open file shown in a tab.
struct Tab {
    path: PathBuf,
    content: String,
    /// Buffer the read-only `TextEdit` writes into; reset to `content` on any edit.
    edit_buf: String,
    job: LayoutJob,
    job_font: f32,
    /// Cached render artifacts, rebuilt only when the font size changes (not
    /// every frame): the line-number gutter and the measured code width.
    gutter_job: LayoutJob,
    code_w: f32,
    render_font: f32,
    line_count: usize,
    scroll_to_line: Option<usize>,
    /// Line to keep softly highlighted after a jump (until the next navigation).
    flash_line: Option<usize>,
    lang: Option<ast::Lang>,
    outline: Vec<DocSymbol>,
    /// Per-line changes vs HEAD, for the gutter change bars.
    git_changes: crate::git::FileDiff,
}

impl Tab {
    fn new(
        path: PathBuf,
        content: String,
        job: LayoutJob,
        font: f32,
        lang: Option<ast::Lang>,
        outline: Vec<DocSymbol>,
    ) -> Self {
        let line_count = content.lines().count().max(1);
        Self {
            edit_buf: content.clone(),
            path,
            content,
            job,
            job_font: font,
            gutter_job: LayoutJob::default(),
            code_w: 0.0,
            render_font: -1.0, // sentinel: render cache stale
            line_count,
            scroll_to_line: None,
            flash_line: None,
            lang,
            outline,
            git_changes: crate::git::FileDiff::default(),
        }
    }

    /// Rebuild the gutter LayoutJob and measured code width if the font changed
    /// since they were last built. Cheap no-op on the common (unchanged) path.
    fn ensure_render(&mut self, ctx: &egui::Context, font_size: f32) {
        if (self.render_font - font_size).abs() <= f32::EPSILON {
            return;
        }
        let font = FontId::monospace(font_size);
        let gutter_color = Color32::from_rgb(0x4b, 0x50, 0x58);
        let line_h = crate::highlight::line_height(font_size);
        let digits = self.line_count.to_string().len();
        let mut gutter = LayoutJob::default();
        gutter.wrap.max_width = f32::INFINITY;
        for n in 1..=self.line_count {
            let mut fmt = TextFormat::simple(font.clone(), gutter_color);
            fmt.line_height = Some(line_h);
            gutter.append(&format!("{:>width$}\n", n, width = digits), 0.0, fmt);
        }
        self.gutter_job = gutter;
        self.code_w = ctx.fonts(|f| f.layout_job(self.job.clone())).size().x + 24.0;
        self.render_font = font_size;
    }
}

/// A place the user navigated to: a file and a line within it. Powers the
/// browser-style Back/Forward history.
#[derive(Clone, PartialEq)]
struct NavLoc {
    path: PathBuf,
    line: usize,
}

/// An open unified-diff of one file in one commit.
struct DiffView {
    commit_short: String,
    file: String,
    lines: Vec<crate::git::DiffLine>,
}

pub struct CodeLookApp {
    project_root: Option<PathBuf>,
    tree: Option<TreeNode>,
    tabs: Vec<Tab>,
    active: Option<usize>,
    highlighter: Highlighter,
    symbol_index: Option<SymbolIndex>,
    index_rx: Option<Receiver<SymbolIndex>>,
    indexing: bool,
    font_size: f32,
    status: String,
    // In-file search.
    search_open: bool,
    search_query: String,
    search_matches: Vec<usize>,
    search_cur: usize,
    search_focus: bool,
    // Back/Forward navigation history (browser-style).
    history: Vec<NavLoc>,
    hist_pos: usize,
    // Project-wide "Find in Files" search.
    gsearch_open: bool,
    gsearch_query: String,
    gsearch_results: Vec<crate::search::SearchHit>,
    gsearch_files: usize, // distinct files in gsearch_results (precomputed)
    gsearch_truncated: bool,
    gsearch_sel: usize,
    gsearch_focus: bool,
    gsearch_running: bool,
    gsearch_dirty_at: Option<f64>,
    gsearch_prev: String,
    // Persistent background worker owning the searchable-file snapshot.
    gsearch_worker: Option<crate::search::Worker>,
    gsearch_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    gsearch_rx: Option<Receiver<crate::search::Reply>>,
    // Preview pane for the selected search hit.
    gpreview_path: Option<PathBuf>,
    gpreview_job: LayoutJob,
    gpreview_gutter: LayoutJob,
    gpreview_content: String,
    gpreview_needle: String,
    gpreview_marks: Vec<(usize, usize)>, // (char_index_start, char_len) into content
    gpreview_lines: usize,
    gpreview_line: usize,
    gpreview_focus_ci: usize, // char index of the selected hit's first match
    gpreview_scroll: bool,
    // Git integration (read-only): working-tree status + current branch.
    git_status: HashMap<PathBuf, crate::git::FileStatus>,
    git_branch: Option<String>,
    #[allow(clippy::type_complexity)]
    git_rx: Option<Receiver<(HashMap<PathBuf, crate::git::FileStatus>, Option<String>)>>,
    // Tool-window visibility toggles.
    tree_open: bool,
    structure_open: bool,
    // Commit Log panel + diff viewer.
    log_open: bool,
    commits: Vec<crate::git::CommitInfo>,
    commits_rx: Option<Receiver<Vec<crate::git::CommitInfo>>>,
    commit_sel: usize,
    commit_files: Vec<crate::git::FileChange>,
    commit_files_for: String,
    diff_view: Option<DiffView>,
    // Capture mode (design-review loop); None in normal use.
    shot: Option<crate::ShotConfig>,
    shot_frame: u32,
}

impl CodeLookApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial: Option<PathBuf>,
        shot: Option<crate::ShotConfig>,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);

        let restored = cc
            .storage
            .and_then(|s| s.get_string("last_project"))
            .map(PathBuf::from)
            .filter(|p| p.is_dir());

        let mut app = Self {
            project_root: None,
            tree: None,
            tabs: Vec::new(),
            active: None,
            highlighter: Highlighter::new(),
            symbol_index: None,
            index_rx: None,
            indexing: false,
            font_size: 14.0,
            status: String::new(),
            search_open: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_cur: 0,
            search_focus: false,
            history: Vec::new(),
            hist_pos: 0,
            gsearch_open: false,
            gsearch_query: String::new(),
            gsearch_results: Vec::new(),
            gsearch_files: 0,
            gsearch_truncated: false,
            gsearch_sel: 0,
            gsearch_focus: false,
            gsearch_running: false,
            gsearch_dirty_at: None,
            gsearch_prev: String::new(),
            gsearch_worker: None,
            gsearch_cancel: None,
            gsearch_rx: None,
            gpreview_path: None,
            gpreview_job: LayoutJob::default(),
            gpreview_gutter: LayoutJob::default(),
            gpreview_content: String::new(),
            gpreview_needle: String::new(),
            gpreview_marks: Vec::new(),
            gpreview_lines: 0,
            gpreview_line: 0,
            gpreview_focus_ci: 0,
            gpreview_scroll: false,
            git_status: HashMap::new(),
            git_branch: None,
            git_rx: None,
            tree_open: true,
            structure_open: true,
            log_open: false,
            commits: Vec::new(),
            commits_rx: None,
            commit_sel: 0,
            commit_files: Vec::new(),
            commit_files_for: String::new(),
            diff_view: None,
            shot,
            shot_frame: 0,
        };

        // In capture mode, open the requested project + file deterministically
        // (ignore any restored session).
        if let Some(s) = app.shot.clone() {
            if let Some(parent) = s.out.parent() {
                let _ = parent; // out dir is the caller's concern
            }
            if let Some(open) = &s.open {
                if let Some(root) = open.ancestors().find(|p| p.is_dir() && p.join("src").exists())
                {
                    app.open_project(&cc.egui_ctx, root.to_path_buf());
                } else if let Some(parent) = open.parent() {
                    app.open_project(&cc.egui_ctx, parent.to_path_buf());
                }
                app.open_file(open.clone());
            } else if let Some(p) = initial.clone() {
                app.open_project(&cc.egui_ctx, p);
            }
            // Compute git state synchronously so the screenshot shows it.
            if let Some(root) = app.project_root.clone() {
                app.git_status = crate::git::status_map(&root);
                app.git_branch = crate::git::current_branch(&root);
                if s.log {
                    app.log_open = true;
                    app.commits = crate::git::commit_log(&root, 300);
                    if !app.commits.is_empty() {
                        app.select_commit(0);
                        if let Some(f) = app.commit_files.first().cloned() {
                            app.open_commit_diff(f.path);
                        }
                    }
                }
            }
            // Populate the global-search popup synchronously for the screenshot.
            if let (Some(q), Some(root)) = (&s.gsearch, app.project_root.clone()) {
                let (hits, tr) = crate::search::global_search(&root, q);
                app.gsearch_files = count_hit_files(&hits);
                app.gsearch_results = hits;
                app.gsearch_truncated = tr;
                app.gsearch_query = q.clone();
                app.gsearch_open = true;
            }
            return app;
        }

        if let Some(p) = initial.or(restored) {
            app.open_project(&cc.egui_ctx, p);
        }
        app
    }

    fn open_project(&mut self, ctx: &egui::Context, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        let mut root = TreeNode::new(path.clone(), true);
        root.expanded = true;
        root.children = Some(load_children(&path));
        self.tree = Some(root);
        self.project_root = Some(path.clone());
        self.tabs.clear();
        self.active = None;
        self.symbol_index = None;

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx2 = ctx.clone();
        let index_path = path.clone();
        std::thread::spawn(move || {
            let idx = symbols::build_index(&index_path);
            let _ = tx.send(idx);
            ctx2.request_repaint();
        });
        self.index_rx = Some(rx);
        self.indexing = true;
        self.status = "프로젝트 인덱싱 중…".to_string();

        // Drop the search worker (its snapshot belongs to the old root) and
        // any in-flight query.
        if let Some(c) = &self.gsearch_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.gsearch_worker = None;
        self.gsearch_cancel = None;
        self.gsearch_rx = None;
        self.gsearch_results.clear();
        self.gsearch_files = 0;
        self.gsearch_truncated = false;
        self.gsearch_running = false;
        self.gsearch_sel = 0;
        self.gsearch_open = false;

        // Reset commit-log / diff state for the new project.
        self.commits.clear();
        self.commits_rx = None;
        self.commit_files.clear();
        self.commit_files_for.clear();
        self.commit_sel = 0;
        self.diff_view = None;

        // Git status + branch on a background thread (opens the repo in-thread).
        self.git_status.clear();
        self.git_branch = None;
        let (gtx, grx) = std::sync::mpsc::channel();
        let ctx3 = ctx.clone();
        let gpath = path.clone();
        std::thread::spawn(move || {
            let status = crate::git::status_map(&gpath);
            let branch = crate::git::current_branch(&gpath);
            let _ = gtx.send((status, branch));
            ctx3.request_repaint();
        });
        self.git_rx = Some(grx);

        // For a git repository, open the commit log automatically and load it.
        self.log_open = false;
        if crate::git::current_branch(&path).is_some() {
            self.log_open = true;
            let (ctx4, cpath) = (ctx.clone(), path.clone());
            let (ctx_tx, ctx_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let log = crate::git::commit_log(&cpath, 300);
                let _ = ctx_tx.send(log);
                ctx4.request_repaint();
            });
            self.commits_rx = Some(ctx_rx);
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = Some(i);
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let job =
                    self.highlighter
                        .highlight(path.to_str().unwrap_or(""), &content, self.font_size);
                let lang = ast::Lang::from_path(&path);
                let outline = lang
                    .map(|l| ast::document_symbols(l, &content))
                    .unwrap_or_default();
                let git_changes = self
                    .project_root
                    .as_ref()
                    .and_then(|root| crate::git::file_line_changes(root, &path))
                    .unwrap_or_default();
                let mut tab = Tab::new(path, content, job, self.font_size, lang, outline);
                tab.git_changes = git_changes;
                self.tabs.push(tab);
                self.active = Some(self.tabs.len() - 1);
                self.status.clear();
                self.refresh_search();
            }
            Err(_) => {
                self.status = format!(
                    "열 수 없는 파일(바이너리 또는 권한): {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }

    // ---- Back / Forward navigation -----------------------------------------

    /// Record a visited location, truncating any forward history (browser model).
    fn record_nav(&mut self, path: PathBuf, line: usize) {
        let loc = NavLoc { path, line };
        if self.history.get(self.hist_pos) == Some(&loc) {
            return; // already here
        }
        if !self.history.is_empty() {
            self.history.truncate(self.hist_pos + 1);
        }
        self.history.push(loc);
        self.hist_pos = self.history.len() - 1;
    }

    fn can_back(&self) -> bool {
        !self.history.is_empty() && self.hist_pos > 0
    }

    fn can_forward(&self) -> bool {
        self.hist_pos + 1 < self.history.len()
    }

    fn nav_back(&mut self) {
        if !self.can_back() {
            return;
        }
        self.hist_pos -= 1;
        self.go_to_loc(self.history[self.hist_pos].clone());
    }

    fn nav_forward(&mut self) {
        if !self.can_forward() {
            return;
        }
        self.hist_pos += 1;
        self.go_to_loc(self.history[self.hist_pos].clone());
    }

    /// Navigate to a history location WITHOUT recording (avoids feedback loops).
    fn go_to_loc(&mut self, loc: NavLoc) {
        self.open_file(loc.path.clone());
        if let Some(i) = self.active {
            self.tabs[i].scroll_to_line = Some(loc.line);
            self.tabs[i].flash_line = Some(loc.line);
        }
    }

    // ---- Project-wide search ("Find in Files") -----------------------------

    /// Create the persistent search worker for the current project root.
    fn ensure_gsearch_worker(&mut self, ctx: &egui::Context) {
        if self.gsearch_worker.is_some() {
            return;
        }
        let root = match &self.project_root {
            Some(r) => r.clone(),
            None => return,
        };
        let ctx2 = ctx.clone();
        let (worker, rx) = crate::search::Worker::new(root, move || ctx2.request_repaint());
        self.gsearch_worker = Some(worker);
        self.gsearch_rx = Some(rx);
    }

    /// Pre-build the file snapshot (called when the search popup opens), so
    /// the first real query only pays the scan, not the initial disk walk.
    fn gsearch_warm(&mut self, ctx: &egui::Context) {
        self.ensure_gsearch_worker(ctx);
        if let (Some(w), None) = (&self.gsearch_worker, &self.gsearch_cancel) {
            self.gsearch_cancel = Some(w.submit(String::new()));
        }
    }

    /// Queue the current query on the worker (min 2 chars), cancelling the
    /// previous one.
    fn gsearch_kick(&mut self, ctx: &egui::Context) {
        if let Some(c) = &self.gsearch_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
            self.gsearch_cancel = None;
        }
        let query = self.gsearch_query.clone();
        if query.trim().len() < 2 {
            self.gsearch_results.clear();
            self.gsearch_files = 0;
            self.gsearch_truncated = false;
            self.gsearch_running = false;
            return;
        }
        self.ensure_gsearch_worker(ctx);
        if let Some(w) = &self.gsearch_worker {
            self.gsearch_cancel = Some(w.submit(query));
            self.gsearch_running = true;
        }
    }

    /// Accept background results matching the current query (drop stale).
    fn gsearch_poll(&mut self) {
        let mut accepted = None;
        if let Some(rx) = &self.gsearch_rx {
            while let Ok(reply) = rx.try_recv() {
                if reply.0 == self.gsearch_query {
                    accepted = Some(reply);
                }
            }
        }
        if let Some((_, hits, truncated)) = accepted {
            self.gsearch_files = count_hit_files(&hits);
            self.gsearch_results = hits;
            self.gsearch_truncated = truncated;
            self.gsearch_sel = 0;
            self.gsearch_running = false;
        }
    }

    /// Prepare the preview pane for the currently selected search hit: highlight
    /// the file (cached per path) and mark it to scroll to the matched line.
    fn ensure_gpreview(&mut self) {
        let hit = match self.gsearch_results.get(self.gsearch_sel) {
            Some(h) => h.clone(),
            None => {
                self.gpreview_path = None;
                self.gpreview_job = LayoutJob::default();
                self.gpreview_content.clear();
                self.gpreview_marks.clear();
                self.gpreview_lines = 0;
                return;
            }
        };
        let path_changed = self.gpreview_path.as_deref() != Some(hit.path.as_path());
        if path_changed {
            match std::fs::read_to_string(&hit.path) {
                Ok(content) => {
                    self.gpreview_job = self.highlighter.highlight(
                        hit.path.to_str().unwrap_or(""),
                        &content,
                        13.0,
                    );
                    let lines = content.lines().count().max(1);
                    self.gpreview_lines = lines;
                    // Matching line-number gutter (same line height as the code).
                    let lh = crate::highlight::line_height(13.0);
                    let digits = lines.to_string().len();
                    let mut g = LayoutJob::default();
                    g.wrap.max_width = f32::INFINITY;
                    for i in 1..=lines {
                        let mut fmt = TextFormat::simple(
                            FontId::monospace(13.0),
                            Color32::from_rgb(0x4b, 0x50, 0x58),
                        );
                        fmt.line_height = Some(lh);
                        g.append(&format!("{:>width$}\n", i, width = digits), 0.0, fmt);
                    }
                    self.gpreview_gutter = g;
                    self.gpreview_content = content;
                }
                Err(_) => {
                    self.gpreview_job = LayoutJob::default();
                    self.gpreview_gutter = LayoutJob::default();
                    self.gpreview_content.clear();
                    self.gpreview_lines = 0;
                }
            }
            self.gpreview_path = Some(hit.path.clone());
            self.gpreview_line = hit.line;
            self.gpreview_focus_ci = char_index_of(&self.gpreview_content, hit.line, hit.col);
            self.gpreview_scroll = true;
        } else if self.gpreview_line != hit.line {
            self.gpreview_line = hit.line;
            self.gpreview_focus_ci = char_index_of(&self.gpreview_content, hit.line, hit.col);
            self.gpreview_scroll = true;
        }

        // (Re)compute match markers when the file or the query term changes.
        // Matching is ASCII-case-insensitive (crate::search), so byte offsets
        // are exact and a match's char length equals the query's.
        let needle = self.gsearch_query.clone();
        if path_changed || needle != self.gpreview_needle {
            let mut marks = Vec::new();
            let len_char = needle.chars().count();
            if !needle.is_empty() {
                let content = &self.gpreview_content;
                let mut prev_byte = 0usize;
                let mut char_idx = 0usize;
                for bpos in
                    crate::search::find_all_ci(content.as_bytes(), needle.as_bytes(), 4000)
                {
                    char_idx += content[prev_byte..bpos].chars().count();
                    prev_byte = bpos;
                    marks.push((char_idx, len_char));
                }
            }
            self.gpreview_needle = needle;
            self.gpreview_marks = marks;
        }
    }

    // ---- Commit log / diff -------------------------------------------------

    fn toggle_log(&mut self, ctx: &egui::Context) {
        self.log_open = !self.log_open;
        if self.log_open && self.commits.is_empty() && self.commits_rx.is_none() {
            if let Some(root) = self.project_root.clone() {
                let (tx, rx) = std::sync::mpsc::channel();
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let log = crate::git::commit_log(&root, 300);
                    let _ = tx.send(log);
                    c.request_repaint();
                });
                self.commits_rx = Some(rx);
            }
        }
    }

    /// Select a commit and load its changed-file list (synchronously).
    fn select_commit(&mut self, idx: usize) {
        self.commit_sel = idx;
        if let (Some(root), Some(c)) = (self.project_root.clone(), self.commits.get(idx).cloned()) {
            self.commit_files = crate::git::commit_files(&root, &c.id);
            self.commit_files_for = c.id;
        }
    }

    /// Open the unified diff of `file` in the selected commit.
    fn open_commit_diff(&mut self, file: String) {
        if let (Some(root), Some(c)) =
            (self.project_root.clone(), self.commits.get(self.commit_sel).cloned())
        {
            let lines = crate::git::commit_file_diff(&root, &c.id, &file);
            self.diff_view = Some(DiffView {
                commit_short: c.short.clone(),
                file,
                lines,
            });
        }
    }

    /// Open a file at a line, recording it in the nav history.
    fn navigate_to(&mut self, path: PathBuf, line: usize) {
        self.open_file(path.clone());
        if let Some(i) = self.active {
            self.tabs[i].scroll_to_line = Some(line);
            self.tabs[i].flash_line = Some(line);
        }
        self.record_nav(path, line);
    }

    fn goto_definition(&mut self, name: &str) {
        let cur_path = self.active.and_then(|i| self.tabs.get(i)).map(|t| t.path.clone());
        let cur_line = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.flash_line);

        let matches: Vec<_> = self
            .symbol_index
            .as_ref()
            .and_then(|idx| idx.get(name))
            .cloned()
            .unwrap_or_default();

        if matches.is_empty() {
            self.status = if self.indexing {
                format!("⌘+클릭 ‘{name}’ — 인덱싱 중, 잠시 후 다시 시도")
            } else {
                format!("⌘+클릭 ‘{name}’ — 정의를 찾지 못함")
            };
            return;
        }

        // Prefer a definition in the current file (but not the exact line we're
        // already on); otherwise take the first match.
        let pick = matches
            .iter()
            .find(|l| Some(&l.path) == cur_path.as_ref() && Some(l.line) != cur_line)
            .or_else(|| matches.first())
            .cloned()
            .unwrap();

        self.open_file(pick.path.clone());
        if let Some(i) = self.active {
            self.tabs[i].scroll_to_line = Some(pick.line);
            self.tabs[i].flash_line = Some(pick.line);
        }
        self.record_nav(pick.path.clone(), pick.line);

        let where_ = pick
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.status = if matches.len() > 1 {
            format!(
                "⌘+클릭 ‘{name}’ → {}:{} (정의 {}곳 중 1)",
                where_,
                pick.line + 1,
                matches.len()
            )
        } else {
            format!("⌘+클릭 ‘{name}’ → {}:{}", where_, pick.line + 1)
        };
    }

    fn rehighlight_open_tabs(&mut self) {
        for tab in &mut self.tabs {
            if (tab.job_font - self.font_size).abs() > f32::EPSILON {
                tab.job = self.highlighter.highlight(
                    tab.path.to_str().unwrap_or(""),
                    &tab.content,
                    self.font_size,
                );
                tab.job_font = self.font_size;
            }
        }
    }

    // ---- search -------------------------------------------------------------

    fn refresh_search(&mut self) {
        self.search_matches.clear();
        self.search_cur = 0;
        let q = self.search_query.to_lowercase();
        if q.is_empty() {
            return;
        }
        if let Some(i) = self.active {
            for (n, line) in self.tabs[i].content.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    self.search_matches.push(n);
                }
            }
        }
    }

    /// Capture-mode driver: let a few frames settle, request a screenshot, save
    /// it when the event arrives, then close the window.
    fn drive_capture(&mut self, ctx: &egui::Context) {
        self.shot_frame += 1;
        ctx.request_repaint(); // keep frames flowing without input

        if self.shot_frame == 8 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }

        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            if let Some(s) = &self.shot {
                save_png(&s.out, &image);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn search_step(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let len = self.search_matches.len();
        self.search_cur = if forward {
            (self.search_cur + 1) % len
        } else {
            (self.search_cur + len - 1) % len
        };
        let line = self.search_matches[self.search_cur];
        if let Some(i) = self.active {
            self.tabs[i].scroll_to_line = Some(line);
            self.tabs[i].flash_line = Some(line);
        }
    }
}

impl eframe::App for CodeLookApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(p) = &self.project_root {
            storage.set_string("last_project", p.to_string_lossy().to_string());
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.shot.is_some() {
            self.drive_capture(ctx);
        }

        if let Some(rx) = &self.index_rx {
            if let Ok(idx) = rx.try_recv() {
                let count = idx.len();
                self.symbol_index = Some(idx);
                self.indexing = false;
                self.index_rx = None;
                self.status = format!("인덱싱 완료 · 심볼 {count}개");
            }
        }

        if let Some(rx) = &self.git_rx {
            if let Ok((status, branch)) = rx.try_recv() {
                self.git_status = status;
                self.git_branch = branch;
                self.git_rx = None;
            }
        }

        if let Some(rx) = &self.commits_rx {
            if let Ok(list) = rx.try_recv() {
                self.commits = list;
                self.commits_rx = None;
                if !self.commits.is_empty() {
                    self.select_commit(0);
                }
            }
        }

        // Keyboard: ⌘F open search, Esc closes it.
        // Navigation (IntelliJ macOS keymap): Back = ⌘[ or ⌥⌘← ; Forward = ⌘] or
        // ⌥⌘→ ; plus the mouse Back/Forward side buttons.
        let (open_find, open_gfind, esc, back, forward) = ctx.input(|i| {
            let cmd = i.modifiers.command || i.modifiers.ctrl;
            let alt = i.modifiers.alt;
            let shift = i.modifiers.shift;
            let back = (cmd && i.key_pressed(Key::OpenBracket))
                || (cmd && alt && i.key_pressed(Key::ArrowLeft))
                || i.pointer.button_pressed(egui::PointerButton::Extra1);
            let forward = (cmd && i.key_pressed(Key::CloseBracket))
                || (cmd && alt && i.key_pressed(Key::ArrowRight))
                || i.pointer.button_pressed(egui::PointerButton::Extra2);
            (
                cmd && !shift && i.key_pressed(Key::F),
                cmd && shift && i.key_pressed(Key::F),
                i.key_pressed(Key::Escape),
                back,
                forward,
            )
        });
        if open_find && self.active.is_some() {
            self.search_open = true;
            self.search_focus = true;
        }
        if open_gfind && self.project_root.is_some() {
            self.gsearch_open = true;
            self.gsearch_focus = true;
            self.gsearch_warm(ctx);
        }
        if esc {
            self.search_open = false;
            self.gsearch_open = false;
        }
        if back {
            self.nav_back();
        }
        if forward {
            self.nav_forward();
        }

        // Search-as-you-type: the debounce timer resets on every keystroke and
        // fires ~120ms after typing stops, so results update live without Enter.
        // (Skipped in capture mode, where results are populated synchronously.)
        self.gsearch_poll();
        let now = ctx.input(|i| i.time);
        if self.shot.is_some() {
            self.gsearch_prev = self.gsearch_query.clone();
        } else if self.gsearch_query != self.gsearch_prev {
            self.gsearch_prev = self.gsearch_query.clone();
            self.gsearch_dirty_at = Some(now);
        }
        if let Some(t0) = self.gsearch_dirty_at {
            if now - t0 > 0.12 {
                self.gsearch_dirty_at = None;
                self.gsearch_kick(ctx);
            } else {
                ctx.request_repaint();
            }
        }

        self.top_bar(ctx);
        self.bottom_bar(ctx);
        self.git_log_panel(ctx);
        self.side_tree(ctx);
        self.structure_panel(ctx);
        self.central(ctx);
        self.global_search_window(ctx);
    }
}

impl CodeLookApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar")
            .frame(
                egui::Frame::default()
                    .fill(C_PANEL)
                    .inner_margin(egui::Margin::symmetric(8, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;

                    // Back / Forward navigation.
                    if nav_arrow(ui, false, self.can_back()) {
                        self.nav_back();
                    }
                    if nav_arrow(ui, true, self.can_forward()) {
                        self.nav_forward();
                    }
                    sep_dot(ui);

                    if let Some(root) = &self.project_root {
                        ui.label(
                            egui::RichText::new(
                                root.file_name().unwrap_or_default().to_string_lossy(),
                            )
                            .strong()
                            .size(13.5)
                            .color(C_TEXT),
                        );
                        ui.add_space(2.0);
                        sep_dot(ui);
                    }

                    if ui.button("프로젝트 열기").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.open_project(ctx, dir);
                        }
                    }
                    if ui.button("검색").on_hover_text("⌘F").clicked() && self.active.is_some() {
                        self.search_open = true;
                        self.search_focus = true;
                    }
                    if ui.button("전체 검색").on_hover_text("⇧⌘F").clicked()
                        && self.project_root.is_some()
                    {
                        self.gsearch_open = true;
                        self.gsearch_focus = true;
                        self.gsearch_warm(ctx);
                    }

                    ui.add_space(4.0);
                    sep_dot(ui);
                    // Tool-window toggles: project tree / structure / commits.
                    if panel_toggle(ui, PanelSide::Left, self.tree_open, "프로젝트 트리") {
                        self.tree_open = !self.tree_open;
                    }
                    if panel_toggle(ui, PanelSide::Right, self.structure_open, "구조(STRUCTURE)") {
                        self.structure_open = !self.structure_open;
                    }
                    if panel_toggle(ui, PanelSide::Bottom, self.log_open, "커밋 로그") {
                        self.toggle_log(ctx);
                    }
                    sep_dot(ui);

                    ui.add_space(4.0);
                    if ui.small_button("A−").on_hover_text("글자 작게").clicked() {
                        self.font_size = (self.font_size - 1.0).max(8.0);
                        self.rehighlight_open_tabs();
                    }
                    ui.label(
                        egui::RichText::new(format!("{}px", self.font_size as i32))
                            .color(C_TEXT_DIM)
                            .size(12.0),
                    );
                    if ui.small_button("A+").on_hover_text("글자 크게").clicked() {
                        self.font_size = (self.font_size + 1.0).min(40.0);
                        self.rehighlight_open_tabs();
                    }

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if self.indexing {
                            ui.spinner();
                        }
                        let hint = if self.status.is_empty() {
                            "⌘+클릭 정의 이동 · ⌘C 복사".to_string()
                        } else {
                            self.status.clone()
                        };
                        ui.label(
                            egui::RichText::new(hint).color(C_TEXT_DIM).size(12.0),
                        );
                    });
                });
            });
    }

    fn side_tree(&mut self, ctx: &egui::Context) {
        if !self.tree_open {
            return;
        }
        egui::SidePanel::left("tree_panel")
            .resizable(true)
            .default_width(260.0)
            .width_range(170.0..=600.0)
            .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
            .show(ctx, |ui| {
                tool_window_header(ui, "PROJECT", None);
                egui::ScrollArea::both()
                    .id_salt("tree_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let active_path = self
                            .active
                            .and_then(|i| self.tabs.get(i))
                            .map(|t| t.path.clone());
                        let mut to_open = None;
                        let status = &self.git_status;
                        if let Some(root) = &mut self.tree {
                            show_node(ui, root, 0, active_path.as_deref(), status, &mut to_open);
                        } else {
                            ui.add_space(20.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new("열린 프로젝트가 없습니다")
                                        .color(C_TEXT_DIM),
                                );
                            });
                        }
                        if let Some(p) = to_open {
                            self.open_file(p.clone());
                            self.record_nav(p, 0);
                        }
                    });
            });
    }

    /// IntelliJ-style "Structure" view: AST-derived document symbols of the
    /// active file. Clicking jumps to the definition.
    fn structure_panel(&mut self, ctx: &egui::Context) {
        if !self.structure_open {
            return;
        }
        let i = match self.active {
            Some(i) if i < self.tabs.len() => i,
            _ => return,
        };
        let lang_label = self.tabs[i].lang.map(|l| l.label()).unwrap_or("Plain");
        egui::SidePanel::right("structure_panel")
            .resizable(true)
            .default_width(240.0)
            .width_range(160.0..=460.0)
            .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
            .show(ctx, |ui| {
                tool_window_header(ui, "STRUCTURE", Some(lang_label));

                if self.tabs[i].outline.is_empty() {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("추출된 심볼이 없습니다").color(C_TEXT_DIM),
                        );
                    });
                } else {
                    let mut jump = None;
                    egui::ScrollArea::vertical()
                        .id_salt("structure_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for s in &self.tabs[i].outline {
                                if symbol_row(ui, s) {
                                    jump = Some(s.line);
                                }
                            }
                        });
                    if let Some(line) = jump {
                        self.tabs[i].scroll_to_line = Some(line);
                        self.tabs[i].flash_line = Some(line);
                        let p = self.tabs[i].path.clone();
                        self.record_nav(p, line);
                    }
                }
            });
    }

    /// IntelliJ-style breadcrumb + status strip pinned to the window bottom:
    /// `project › dir › … › file` on the left, line count + language on the right.
    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::default()
                    .fill(C_PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ctx, |ui| {
                let full = ui.available_width();
                let (rect, _) = ui.allocate_exact_size(Vec2::new(full, 24.0), Sense::hover());
                let p = ui.painter();
                // Top hairline border.
                p.line_segment(
                    [
                        egui::pos2(rect.left() - 10.0, rect.top()),
                        egui::pos2(rect.right() + 10.0, rect.top()),
                    ],
                    Stroke::new(1.0, C_BORDER),
                );

                let Some(root) = self.project_root.as_ref() else {
                    return;
                };
                let font = FontId::proportional(12.0);
                let cy = rect.center().y;

                // Breadcrumbs (left) — only when a file is open.
                if let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) {
                    let mut crumbs: Vec<String> = vec![root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()];
                    if let Ok(rel) = tab.path.strip_prefix(root) {
                        for c in rel.components() {
                            crumbs.push(c.as_os_str().to_string_lossy().to_string());
                        }
                    }
                    let file_name = tab
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let mut x = rect.left();
                    let last = crumbs.len().saturating_sub(1);
                    for (i, crumb) in crumbs.iter().enumerate() {
                        let is_file = i == last;
                        if is_file {
                            let badge =
                                Rect::from_center_size(egui::pos2(x + 7.0, cy), Vec2::splat(14.0));
                            icons::draw_file_icon(p, badge, &file_name);
                            x = badge.right() + 5.0;
                        }
                        let color = if is_file { C_TEXT } else { C_TEXT_DIM };
                        let galley = p.layout_no_wrap(crumb.clone(), font.clone(), color);
                        p.galley(egui::pos2(x, cy - galley.size().y / 2.0), galley.clone(), color);
                        x += galley.size().x;
                        if !is_file {
                            draw_crumb_sep(p, egui::pos2(x + 7.0, cy), C_TEXT_DIM);
                            x += 14.0;
                        }
                    }
                }

                // Right side: [line count · lang]  ·  ⑂ branch.
                let mut right = String::new();
                if let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) {
                    let lang = tab.lang.map(|l| l.label()).unwrap_or("Text");
                    right = format!("{} lines   ·   {}", tab.line_count, lang);
                }
                if let Some(branch) = &self.git_branch {
                    if !right.is_empty() {
                        right.push_str("   ·   ");
                    }
                    right.push_str(branch);
                    // small branch icon before the whole right cluster
                    let g = p.layout_no_wrap(right.clone(), font.clone(), C_TEXT_DIM);
                    let bx = rect.right() - g.size().x - 16.0;
                    draw_branch_icon(p, egui::pos2(bx + 5.0, cy), C_TEXT_DIM);
                }
                p.text(
                    egui::pos2(rect.right(), cy),
                    egui::Align2::RIGHT_CENTER,
                    right,
                    font,
                    C_TEXT_DIM,
                );
            });
    }

    /// "Find in Files" popup: results grouped by file on the left, a live code
    /// preview of the selected match on the right (IntelliJ-style). Typing
    /// updates results live; ↑/↓ move the selection, Enter / double-click opens.
    fn global_search_window(&mut self, ctx: &egui::Context) {
        if !self.gsearch_open {
            return;
        }
        let root = self.project_root.clone().unwrap_or_default();

        // Keyboard selection (handled here so the preview can update this frame).
        let n = self.gsearch_results.len();
        let (up, down, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
            )
        });
        let mut key_moved = false;
        if n > 0 {
            if down {
                self.gsearch_sel = (self.gsearch_sel + 1).min(n - 1);
                key_moved = true;
            }
            if up {
                self.gsearch_sel = self.gsearch_sel.saturating_sub(1);
                key_moved = true;
            }
        }
        let mut navigate: Option<(PathBuf, usize)> = None;
        if enter {
            if let Some(h) = self.gsearch_results.get(self.gsearch_sel) {
                navigate = Some((h.path.clone(), h.line));
            }
        }

        // Refresh the preview for the current selection.
        self.ensure_gpreview();

        // Move fields into locals so the Window closure doesn't alias `self`.
        let mut query = std::mem::take(&mut self.gsearch_query);
        let results = std::mem::take(&mut self.gsearch_results);
        let preview_job = std::mem::take(&mut self.gpreview_job);
        let preview_gutter = std::mem::take(&mut self.gpreview_gutter);
        let preview_marks = std::mem::take(&mut self.gpreview_marks);
        let needle = query.clone(); // markers match ASCII-case-insensitively
        let needle_len_char = needle.chars().count();
        let focus_ci = self.gpreview_focus_ci;
        let mut sel = self.gsearch_sel;
        let running = self.gsearch_running;
        let truncated = self.gsearch_truncated;
        let mut want_focus = self.gsearch_focus;
        let preview_line = self.gpreview_line;
        let mut do_scroll = self.gpreview_scroll || key_moved;
        let mut keep_open = true;

        let status = if running {
            "검색 중…".to_string()
        } else if query.trim().len() < 2 {
            "두 글자 이상 입력하세요".to_string()
        } else if results.is_empty() {
            "결과 없음".to_string()
        } else {
            format!(
                "{}개 파일에서 {}건{}",
                self.gsearch_files,
                results.len(),
                if truncated { " (상한 도달)" } else { "" }
            )
        };

        egui::Window::new("전체 검색")
            .collapsible(false)
            .resizable(false)
            .fixed_size([1000.0, 600.0])
            .anchor(Align2::CENTER_TOP, [0.0, 60.0])
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(C_PANEL)
                    .stroke(Stroke::new(1.0, C_BORDER))
                    .inner_margin(10.0),
            )
            .open(&mut keep_open)
            .show(ctx, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut query)
                        .hint_text("프로젝트 전체에서 검색")
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(14.0)),
                );
                if want_focus {
                    resp.request_focus();
                    want_focus = false;
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new(status).color(C_TEXT_DIM).size(12.0));
                ui.add_space(4.0);
                ui.separator();

                let body_h = ui.available_height();
                let list_h = (body_h * 0.42).clamp(110.0, 300.0);

                // ---- Results list (top) ----
                egui::ScrollArea::vertical()
                    .id_salt("gsearch_list")
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let row_font = FontId::monospace(12.5);
                        let text_x = 42.0;
                        let mut prev: Option<&Path> = None;
                        for (idx, hit) in results.iter().enumerate() {
                            if prev != Some(hit.path.as_path()) {
                                prev = Some(hit.path.as_path());
                                ui.add_space(6.0);
                                let full = ui.available_width();
                                let (hr, _) =
                                    ui.allocate_exact_size(Vec2::new(full, 22.0), Sense::hover());
                                // Rows keep their slot in the layout, but text/
                                // icon work only happens for visible ones.
                                if ui.is_rect_visible(hr) {
                                    let rel = hit
                                        .path
                                        .strip_prefix(&root)
                                        .unwrap_or(&hit.path)
                                        .to_string_lossy()
                                        .to_string();
                                    let name = hit
                                        .path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    let p = ui.painter();
                                    let badge = Rect::from_center_size(
                                        egui::pos2(hr.left() + 9.0, hr.center().y),
                                        Vec2::splat(14.0),
                                    );
                                    icons::draw_file_icon(p, badge, &name);
                                    p.text(
                                        egui::pos2(badge.right() + 6.0, hr.center().y),
                                        Align2::LEFT_CENTER,
                                        rel,
                                        FontId::proportional(12.5),
                                        C_HEADER,
                                    );
                                }
                            }

                            let full = ui.available_width();
                            let (rr, rresp) =
                                ui.allocate_exact_size(Vec2::new(full, 22.0), Sense::click());
                            if ui.is_rect_visible(rr) {
                                let p = ui.painter();
                                if idx == sel {
                                    p.rect_filled(rr, 0.0, C_SEL);
                                } else if rresp.hovered() {
                                    p.rect_filled(rr, 0.0, C_HOVER);
                                }
                                p.text(
                                    egui::pos2(rr.left() + 30.0, rr.center().y),
                                    Align2::RIGHT_CENTER,
                                    (hit.line + 1).to_string(),
                                    FontId::monospace(11.5),
                                    C_TEXT_DIM,
                                );
                                let text: String =
                                    hit.text.trim_start().chars().take(300).collect();
                                p.text(
                                    egui::pos2(rr.left() + text_x, rr.center().y),
                                    Align2::LEFT_CENTER,
                                    &text,
                                    FontId::monospace(12.5),
                                    if idx == sel { Color32::WHITE } else { C_TEXT },
                                );
                                // Highlighter marker over each occurrence of the
                                // term (ASCII-case-insensitive), positioned by
                                // measuring real text width (tab-safe).
                                if !needle.is_empty() {
                                    for bpos in crate::search::find_all_ci(
                                        text.as_bytes(),
                                        needle.as_bytes(),
                                        32,
                                    ) {
                                        let px = ui.fonts(|f| {
                                            f.layout_no_wrap(
                                                text[..bpos].to_string(),
                                                row_font.clone(),
                                                C_TEXT,
                                            )
                                            .size()
                                            .x
                                        });
                                        let mw = ui.fonts(|f| {
                                            f.layout_no_wrap(
                                                text[bpos..bpos + needle.len()].to_string(),
                                                row_font.clone(),
                                                C_TEXT,
                                            )
                                            .size()
                                            .x
                                        });
                                        let mx = rr.left() + text_x + px;
                                        let mrect = Rect::from_min_size(
                                            egui::pos2(mx - 1.0, rr.top() + 3.0),
                                            Vec2::new(mw + 2.0, rr.height() - 6.0),
                                        );
                                        ui.painter().rect_filled(
                                            mrect,
                                            2.0,
                                            Color32::from_rgba_unmultiplied(0xe6, 0xb0, 0x3a, 72),
                                        );
                                    }
                                }
                            }
                            if rresp.clicked() {
                                sel = idx; // select → preview updates next frame
                            }
                            if rresp.double_clicked() {
                                navigate = Some((hit.path.clone(), hit.line));
                            }
                            if key_moved && idx == sel {
                                ui.scroll_to_rect(rr, Some(Align::Center));
                            }
                        }
                    });

                // Horizontal divider between results and preview.
                ui.add_space(2.0);
                let (dr, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                ui.painter()
                    .hline(dr.x_range(), dr.center().y, Stroke::new(1.0, C_BORDER));
                ui.add_space(2.0);

                // ---- Preview (bottom) ----
                if results.is_empty() {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("미리볼 결과가 없습니다").color(C_TEXT_DIM),
                        );
                    });
                } else {
                    let preview_h = ui.available_height();
                    egui::Frame::default()
                        .fill(C_EDITOR)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            egui::ScrollArea::both()
                                .id_salt("gpreview_scroll")
                                .max_height(preview_h)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    let g_galley =
                                        ui.fonts(|f| f.layout_job(preview_gutter.clone()));
                                    let c_galley =
                                        ui.fonts(|f| f.layout_job(preview_job.clone()));
                                    let rows = c_galley.rows.len().max(1);
                                    let row_h = c_galley.size().y / rows as f32;
                                    ui.horizontal_top(|ui| {
                                        ui.spacing_mut().item_spacing.x = 12.0;
                                        let (g_rect, _) = ui
                                            .allocate_exact_size(g_galley.size(), Sense::hover());
                                        let (c_rect, _) = ui.allocate_exact_size(
                                            Vec2::new(
                                                c_galley.size().x.max(ui.available_width()),
                                                c_galley.size().y,
                                            ),
                                            Sense::hover(),
                                        );
                                        let p = ui.painter();
                                        // Subtle current-line band (gutter + code).
                                        let y = c_rect.top() + row_h * preview_line as f32;
                                        let band = Rect::from_min_size(
                                            egui::pos2(g_rect.left() - 6.0, y),
                                            Vec2::new(
                                                (c_rect.right() - g_rect.left()) + 12.0,
                                                row_h,
                                            ),
                                        );
                                        p.rect_filled(
                                            band,
                                            0.0,
                                            Color32::from_rgba_unmultiplied(0x5a, 0x62, 0x78, 28),
                                        );
                                        // Marker on every occurrence — positioned
                                        // from the galley's real glyph layout so
                                        // tabs / wide glyphs stay aligned.
                                        for &(ci, len) in &preview_marks {
                                            let a = c_galley
                                                .pos_from_ccursor(egui::text::CCursor::new(ci));
                                            let b = c_galley
                                                .pos_from_ccursor(egui::text::CCursor::new(ci + len));
                                            let mrect = Rect::from_min_max(
                                                c_rect.min + Vec2::new(a.left() - 1.0, a.top() + 1.0),
                                                c_rect.min + Vec2::new(b.left() + 1.0, a.bottom() - 1.0),
                                            );
                                            p.rect_filled(
                                                mrect,
                                                2.0,
                                                Color32::from_rgba_unmultiplied(
                                                    0xe0, 0xa8, 0x3a, 110,
                                                ),
                                            );
                                        }
                                        p.galley(g_rect.min, g_galley, C_TEXT_DIM);
                                        p.galley(c_rect.min, c_galley.clone(), C_TEXT);
                                        if do_scroll {
                                            // Focus the FIRST match on the line (not
                                            // the whole-line band, which would center
                                            // on the middle/last match).
                                            let a = c_galley
                                                .pos_from_ccursor(egui::text::CCursor::new(focus_ci));
                                            let b = c_galley.pos_from_ccursor(
                                                egui::text::CCursor::new(focus_ci + needle_len_char),
                                            );
                                            let frect = Rect::from_min_max(
                                                c_rect.min + Vec2::new(a.left(), a.top()),
                                                c_rect.min + Vec2::new(b.left(), a.bottom()),
                                            );
                                            ui.scroll_to_rect(frect, Some(Align::Center));
                                            do_scroll = false;
                                        }
                                    });
                                });
                        });
                }
            });

        // Restore fields and apply outputs.
        self.gsearch_query = query;
        self.gsearch_results = results;
        self.gpreview_job = preview_job;
        self.gpreview_gutter = preview_gutter;
        self.gpreview_marks = preview_marks;
        self.gsearch_sel = sel;
        self.gsearch_focus = want_focus;
        self.gpreview_scroll = false;

        if let Some((p, l)) = navigate {
            self.navigate_to(p, l);
            self.gsearch_open = false;
        }
        if !keep_open {
            self.gsearch_open = false;
        }
    }

    /// Bottom Git-Log tool window: commits (left) + changed files (right).
    fn git_log_panel(&mut self, ctx: &egui::Context) {
        if !self.log_open {
            return;
        }
        let loading = self.commits_rx.is_some();
        let mut sel_commit: Option<usize> = None;
        let mut open_file: Option<String> = None;
        let mut close = false;

        egui::TopBottomPanel::bottom("git_log")
            .resizable(true)
            .default_height(230.0)
            .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
            .show(ctx, |ui| {
                // Header.
                let full = ui.available_width();
                let (hr, _) = ui.allocate_exact_size(Vec2::new(full, 26.0), Sense::hover());
                let p = ui.painter();
                p.text(
                    egui::pos2(hr.left() + 10.0, hr.center().y),
                    Align2::LEFT_CENTER,
                    "COMMITS",
                    FontId::proportional(11.5),
                    C_HEADER,
                );
                let count_label = if loading {
                    "불러오는 중…".to_string()
                } else {
                    format!("{}개", self.commits.len())
                };
                p.text(
                    egui::pos2(hr.right() - 30.0, hr.center().y),
                    Align2::RIGHT_CENTER,
                    count_label,
                    FontId::proportional(11.5),
                    C_TEXT_DIM,
                );
                let close_c =
                    Rect::from_center_size(egui::pos2(hr.right() - 12.0, hr.center().y), Vec2::splat(15.0));
                if ui
                    .interact(close_c, ui.id().with("log_close"), Sense::click())
                    .clicked()
                {
                    close = true;
                }
                draw_x(ui.painter(), close_c, C_TEXT_DIM);
                ui.painter().hline(
                    hr.x_range(),
                    hr.bottom(),
                    Stroke::new(1.0, C_BORDER),
                );

                let body_h = ui.available_height();
                let left_w = (ui.available_width() * 0.6).max(320.0);
                ui.horizontal_top(|ui| {
                    // Commit list.
                    ui.vertical(|ui| {
                        ui.set_min_width(left_w);
                        ui.set_max_width(left_w);
                        egui::ScrollArea::vertical()
                            .id_salt("commit_list")
                            .max_height(body_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for (i, c) in self.commits.iter().enumerate() {
                                    let (rr, resp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 24.0),
                                        Sense::click(),
                                    );
                                    let p = ui.painter();
                                    if i == self.commit_sel {
                                        p.rect_filled(rr, 0.0, C_SEL);
                                    } else if resp.hovered() {
                                        p.rect_filled(rr, 0.0, C_HOVER);
                                    }
                                    let sel = i == self.commit_sel;
                                    p.text(
                                        egui::pos2(rr.left() + 10.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        &c.short,
                                        FontId::monospace(12.0),
                                        Color32::from_rgb(0xd1, 0x9a, 0x66),
                                    );
                                    p.text(
                                        egui::pos2(rr.left() + 74.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        &c.summary,
                                        FontId::proportional(13.0),
                                        if sel { Color32::WHITE } else { C_TEXT },
                                    );
                                    let meta = format!("{}   {}", c.author, rel_time(c.time));
                                    p.text(
                                        egui::pos2(rr.right() - 8.0, rr.center().y),
                                        Align2::RIGHT_CENTER,
                                        meta,
                                        FontId::proportional(11.5),
                                        C_TEXT_DIM,
                                    );
                                    if resp.clicked() {
                                        sel_commit = Some(i);
                                    }
                                }
                            });
                    });

                    let (dr, _) = ui.allocate_exact_size(Vec2::new(9.0, body_h), Sense::hover());
                    ui.painter()
                        .vline(dr.center().x, dr.y_range(), Stroke::new(1.0, C_BORDER));

                    // Changed files of the selected commit.
                    ui.vertical(|ui| {
                        ui.set_min_height(body_h);
                        egui::ScrollArea::vertical()
                            .id_salt("commit_files")
                            .max_height(body_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for f in &self.commit_files {
                                    let (rr, resp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 22.0),
                                        Sense::click(),
                                    );
                                    let p = ui.painter();
                                    let is_open = self
                                        .diff_view
                                        .as_ref()
                                        .map(|d| d.file == f.path)
                                        .unwrap_or(false);
                                    if is_open {
                                        p.rect_filled(rr, 0.0, C_SEL);
                                    } else if resp.hovered() {
                                        p.rect_filled(rr, 0.0, C_HOVER);
                                    }
                                    // status letter
                                    let (letter, lc) = match f.status {
                                        crate::git::FileStatus::Added => ("A", status_color(f.status)),
                                        crate::git::FileStatus::Deleted => ("D", status_color(f.status)),
                                        crate::git::FileStatus::Renamed => ("R", status_color(f.status)),
                                        _ => ("M", status_color(f.status)),
                                    };
                                    p.text(
                                        egui::pos2(rr.left() + 10.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        letter,
                                        FontId::monospace(12.0),
                                        lc,
                                    );
                                    p.text(
                                        egui::pos2(rr.left() + 26.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        &f.path,
                                        FontId::proportional(12.5),
                                        if is_open { Color32::WHITE } else { C_TEXT },
                                    );
                                    if resp.clicked() {
                                        open_file = Some(f.path.clone());
                                    }
                                }
                            });
                    });
                });
            });

        if close {
            self.log_open = false;
        }
        if let Some(i) = sel_commit {
            self.select_commit(i);
        }
        if let Some(f) = open_file {
            self.open_commit_diff(f);
        }
    }

    fn central(&mut self, ctx: &egui::Context) {
        let bg = C_EDITOR;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(bg).inner_margin(0.0))
            .show(ctx, |ui| {
                // A commit diff takes over the editor area while open.
                if self.diff_view.is_some() {
                    self.diff_area(ui);
                    return;
                }
                self.tab_bar(ui);
                if self.search_open {
                    self.search_bar(ui);
                }
                if self.active.is_none() {
                    self.welcome(ui);
                    return;
                }
                self.editor(ui, bg);
            });
    }

    /// Render the open commit diff (header + unified diff body).
    fn diff_area(&mut self, ui: &mut egui::Ui) {
        let Some(diff) = self.diff_view.take() else {
            return;
        };
        let mut keep = true;

        // Header bar: ◀ close · commit · file.
        egui::Frame::default()
            .fill(C_PANEL)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("✕ 닫기").clicked() {
                        keep = false;
                    }
                    sep_dot(ui);
                    ui.label(
                        egui::RichText::new(format!("{}  ·  {}", diff.commit_short, diff.file))
                            .color(C_TEXT)
                            .size(13.0),
                    );
                });
            });
        ui.painter().hline(
            ui.max_rect().x_range(),
            ui.min_rect().bottom(),
            Stroke::new(1.0, C_BORDER),
        );

        let font = FontId::monospace(self.font_size);
        let lh = crate::highlight::line_height(self.font_size);
        egui::ScrollArea::both()
            .id_salt("diff_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let full_w = ui.available_width();
                for dl in &diff.lines {
                    let (rr, _) = ui.allocate_exact_size(Vec2::new(full_w.max(600.0), lh), Sense::hover());
                    let p = ui.painter();
                    let (bg_col, mark, mark_col, txt_col) = match dl.kind {
                        crate::git::DiffKind::Add => (
                            Color32::from_rgb(0x28, 0x3a, 0x28),
                            "+",
                            Color32::from_rgb(0x7e, 0xc6, 0x99),
                            C_TEXT,
                        ),
                        crate::git::DiffKind::Del => (
                            Color32::from_rgb(0x3f, 0x2b, 0x2b),
                            "-",
                            Color32::from_rgb(0xe0, 0x6c, 0x75),
                            C_TEXT,
                        ),
                        crate::git::DiffKind::Hunk => (
                            Color32::from_rgb(0x2b, 0x2d, 0x30),
                            "",
                            C_TEXT_DIM,
                            Color32::from_rgb(0x56, 0xb6, 0xc2),
                        ),
                        crate::git::DiffKind::Context => {
                            (C_EDITOR, "", C_TEXT_DIM, C_TEXT_DIM)
                        }
                    };
                    if bg_col != C_EDITOR {
                        p.rect_filled(rr, 0.0, bg_col);
                    }
                    // old / new line-number gutter
                    let onum = dl.old_no.map(|n| n.to_string()).unwrap_or_default();
                    let nnum = dl.new_no.map(|n| n.to_string()).unwrap_or_default();
                    p.text(
                        egui::pos2(rr.left() + 44.0, rr.center().y),
                        Align2::RIGHT_CENTER,
                        onum,
                        FontId::monospace(11.5),
                        C_TEXT_DIM,
                    );
                    p.text(
                        egui::pos2(rr.left() + 90.0, rr.center().y),
                        Align2::RIGHT_CENTER,
                        nnum,
                        FontId::monospace(11.5),
                        C_TEXT_DIM,
                    );
                    p.text(
                        egui::pos2(rr.left() + 100.0, rr.center().y),
                        Align2::LEFT_CENTER,
                        mark,
                        font.clone(),
                        mark_col,
                    );
                    p.text(
                        egui::pos2(rr.left() + 112.0, rr.center().y),
                        Align2::LEFT_CENTER,
                        &dl.text,
                        font.clone(),
                        txt_col,
                    );
                }
            });

        if keep {
            self.diff_view = Some(diff);
        }
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::default()
            .fill(C_PANEL)
            .inner_margin(6.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("검색").color(C_TEXT_DIM).size(12.0));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .hint_text("파일 내 검색")
                            .desired_width(260.0),
                    );
                    if self.search_focus {
                        resp.request_focus();
                        self.search_focus = false;
                    }
                    if resp.changed() {
                        self.refresh_search();
                        // jump to first match
                        if let (Some(i), Some(line)) =
                            (self.active, self.search_matches.first().copied())
                        {
                            self.tabs[i].scroll_to_line = Some(line);
                        }
                    }
                    let enter =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let shift = ui.input(|i| i.modifiers.shift);
                    if enter {
                        self.search_step(!shift);
                        self.search_focus = true; // keep focus for repeated Enter
                    }
                    if ui.small_button("▲").clicked() {
                        self.search_step(false);
                    }
                    if ui.small_button("▼").clicked() {
                        self.search_step(true);
                    }
                    let label = if self.search_matches.is_empty() {
                        if self.search_query.is_empty() {
                            String::new()
                        } else {
                            "결과 없음".to_string()
                        }
                    } else {
                        format!("{}/{}", self.search_cur + 1, self.search_matches.len())
                    };
                    ui.label(egui::RichText::new(label).weak());
                    if ui.small_button("✕").clicked() {
                        self.search_open = false;
                    }
                });
            });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            return;
        }
        let mut close: Option<usize> = None;
        let mut select: Option<usize> = None;
        let mut active_tab_rect: Option<Rect> = None;
        const TAB_H: f32 = 34.0;

        // Background strip for the tab bar + its bottom border.
        let strip = egui::Frame::default()
            .fill(C_PANEL)
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("tab_scroll")
                    .auto_shrink([false, false])
                    .max_height(TAB_H)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.horizontal(|ui| {
                            for i in 0..self.tabs.len() {
                                let selected = self.active == Some(i);
                                let name = self.tabs[i]
                                    .path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string();

                                let text_w = ui
                                    .fonts(|f| {
                                        f.layout_no_wrap(
                                            name.clone(),
                                            FontId::proportional(13.0),
                                            C_TEXT,
                                        )
                                    })
                                    .size()
                                    .x;
                                let tab_w = 16.0 + 16.0 + 6.0 + text_w + 8.0 + 16.0 + 12.0;
                                let (rect, resp) = ui.allocate_exact_size(
                                    Vec2::new(tab_w, TAB_H),
                                    Sense::click(),
                                );
                                let p = ui.painter();

                                if selected {
                                    p.rect_filled(rect, 0.0, C_EDITOR);
                                    active_tab_rect = Some(rect); // underline drawn last
                                } else if resp.hovered() {
                                    p.rect_filled(rect, 0.0, C_HOVER);
                                }

                                let icon = Rect::from_center_size(
                                    egui::pos2(rect.left() + 16.0, rect.center().y),
                                    Vec2::splat(15.0),
                                );
                                icons::draw_file_icon(p, icon, &name);
                                p.text(
                                    egui::pos2(icon.right() + 6.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    &name,
                                    FontId::proportional(13.0),
                                    if selected { C_TEXT } else { C_TEXT_DIM },
                                );

                                // Close affordance (✕ drawn, not glyph).
                                let close_c = Rect::from_center_size(
                                    egui::pos2(rect.right() - 13.0, rect.center().y),
                                    Vec2::splat(15.0),
                                );
                                let close_resp = ui.interact(
                                    close_c,
                                    ui.id().with(("tabclose", i)),
                                    Sense::click(),
                                );
                                let p = ui.painter();
                                if close_resp.hovered() {
                                    p.rect_filled(close_c, 3.0, C_HOVER.gamma_multiply(1.4));
                                }
                                draw_x(
                                    p,
                                    close_c,
                                    if close_resp.hovered() { C_TEXT } else { C_TEXT_DIM },
                                );

                                if close_resp.clicked() {
                                    close = Some(i);
                                } else if resp.clicked() {
                                    select = Some(i);
                                }

                                // tab divider
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(rect.right(), rect.top() + 6.0),
                                        egui::pos2(rect.right(), rect.bottom() - 6.0),
                                    ],
                                    Stroke::new(1.0, C_BORDER),
                                );
                            }
                        });
                    });
            });

        // Full-width hairline under the tab strip (the active tab's blue
        // underline sits on top of it).
        let r = strip.response.rect;
        ui.painter().line_segment(
            [
                egui::pos2(r.left(), r.bottom() - 0.5),
                egui::pos2(r.right(), r.bottom() - 0.5),
            ],
            Stroke::new(1.0, C_BORDER),
        );
        // Active-tab accent underline, drawn on top of the strip border so it
        // is never occluded.
        if let Some(t) = active_tab_rect {
            ui.painter().rect_filled(
                Rect::from_min_max(egui::pos2(t.left(), t.bottom() - 2.0), t.max),
                0.0,
                C_ACCENT,
            );
        }

        if let Some(i) = select {
            self.active = Some(i);
            self.refresh_search();
            let (p, line) = (self.tabs[i].path.clone(), self.tabs[i].flash_line.unwrap_or(0));
            self.record_nav(p, line);
        }
        if let Some(i) = close {
            self.tabs.remove(i);
            self.active = if self.tabs.is_empty() {
                None
            } else {
                Some(self.active.unwrap_or(0).min(self.tabs.len() - 1))
            };
            self.refresh_search();
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(120.0);
            ui.label(
                egui::RichText::new("CodeLook")
                    .size(40.0)
                    .color(Color32::from_rgb(0x8a, 0x9b, 0xc6)),
            );
            ui.add_space(8.0);
            ui.label(egui::RichText::new("가벼운 소스 코드 뷰어").weak().size(16.0));
            ui.add_space(24.0);
            if ui
                .button(egui::RichText::new("프로젝트 열기").size(16.0))
                .clicked()
            {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let ctx = ui.ctx().clone();
                    self.open_project(&ctx, dir);
                }
            }
            ui.add_space(16.0);
            ui.label(
                egui::RichText::new(
                    "좌측 트리에서 파일을 클릭하면 열립니다.\n\
                     ⌘+클릭 = 정의로 이동 · 드래그 = 선택 · ⌘C = 복사 · ⌘F = 검색",
                )
                .weak(),
            );
        });
    }

    fn editor(&mut self, ui: &mut egui::Ui, bg: Color32) {
        let idx = match self.active {
            Some(i) if i < self.tabs.len() => i,
            _ => return,
        };

        let cmd_held = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        let gutter_color = Color32::from_rgb(0x4b, 0x50, 0x58);
        let font = FontId::monospace(self.font_size);

        // Build (or reuse cached) gutter + code width — only rebuilt on font change.
        let ctx = ui.ctx().clone();
        self.tabs[idx].ensure_render(&ctx, self.font_size);

        // Layouter feeds the cached highlighted job to the TextEdit.
        let job_for_layouter = self.tabs[idx].job.clone();
        let mut layouter = move |ui: &egui::Ui, _buf: &str, _w: f32| {
            ui.fonts(|f| f.layout_job(job_for_layouter.clone()))
        };

        let scroll_target = self.tabs[idx].scroll_to_line;
        let mut goto: Option<String> = None;
        let mut clear_scroll = false;
        let mut clicked_nothing = false;

        // Borrow the single tab so edit_buf / content can be touched independently.
        let tab = &mut self.tabs[idx];
        let gutter_job = tab.gutter_job.clone();
        let code_w = tab.code_w;

        egui::ScrollArea::both()
            .id_salt("editor_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 16.0;
                ui.horizontal_top(|ui| {
                    // Gutter — same fill as the editor (no seam), generous left pad.
                    let g_galley = ui.fonts(|f| f.layout_job(gutter_job.clone()));
                    let (g_rect, _) = ui.allocate_exact_size(g_galley.size(), Sense::hover());
                    ui.painter()
                        .rect_filled(g_rect.expand2(Vec2::new(8.0, 2.0)), 0.0, bg);
                    ui.painter().galley(g_rect.min, g_galley, gutter_color);

                    // Git change bars in the gap between the gutter and the code
                    // (vs HEAD). Drawn here so they're inside the panel clip rect.
                    {
                        let lh = crate::highlight::line_height(self.font_size);
                        let bx = g_rect.right() + 5.0;
                        let p = ui.painter();
                        for &(line, kind) in &tab.git_changes.changed {
                            let y = g_rect.top() + line as f32 * lh;
                            let col = match kind {
                                crate::git::LineChange::Added => {
                                    Color32::from_rgb(0x62, 0xb5, 0x43)
                                }
                                crate::git::LineChange::Modified => {
                                    Color32::from_rgb(0x6c, 0x9c, 0xd2)
                                }
                            };
                            p.rect_filled(
                                Rect::from_min_size(egui::pos2(bx, y), Vec2::new(3.0, lh)),
                                1.0,
                                col,
                            );
                        }
                        // Deletion markers: a small gray triangle at the boundary.
                        for &line in &tab.git_changes.deleted_before {
                            let y = g_rect.top() + line as f32 * lh;
                            let c = Color32::from_rgb(0x9a, 0x9a, 0x9a);
                            p.add(egui::Shape::convex_polygon(
                                vec![
                                    egui::pos2(bx, y - 3.5),
                                    egui::pos2(bx, y + 3.5),
                                    egui::pos2(bx + 5.0, y),
                                ],
                                c,
                                Stroke::NONE,
                            ));
                        }
                    }

                    // Code — read-only TextEdit (selection, copy, find cursor for free).
                    let out = egui::TextEdit::multiline(&mut tab.edit_buf)
                        .font(font.clone())
                        .frame(false)
                        .desired_width(code_w.max(ui.available_width()))
                        .layouter(&mut layouter)
                        .show(ui);

                    // Enforce read-only: revert any edit immediately.
                    if out.response.changed() {
                        tab.edit_buf = tab.content.clone();
                    }

                    let rows = out.galley.rows.len().max(1);
                    let row_h = out.galley.size().y / rows as f32;
                    let band_w = out.galley.size().x.max(ui.available_width());
                    let band_left = g_rect.left() - 8.0;

                    // Active (caret) line band — gives the editor a live feel.
                    // Painted over the text, so keep it translucent (text shows through).
                    if let Some(cur) = out.cursor_range {
                        let row = cur.primary.rcursor.row;
                        let y = out.galley_pos.y + row_h * row as f32;
                        ui.painter().rect_filled(
                            Rect::from_min_size(
                                egui::pos2(band_left, y),
                                Vec2::new(band_w + 16.0, row_h),
                            ),
                            0.0,
                            Color32::from_rgba_unmultiplied(0x6a, 0x72, 0x8a, 30),
                        );
                    }

                    // Soft highlight of the most recently jumped-to line.
                    if let Some(line) = tab.flash_line {
                        let y = out.galley_pos.y + row_h * line as f32;
                        let rect = Rect::from_min_size(
                            egui::pos2(band_left, y),
                            Vec2::new(band_w + 16.0, row_h),
                        );
                        ui.painter().rect_filled(
                            rect,
                            0.0,
                            Color32::from_rgba_unmultiplied(0x3a, 0x55, 0x6e, 96),
                        );
                    }

                    // ⌘+Click → go to definition.
                    if cmd_held && out.response.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if cmd_held && out.response.clicked() {
                        // Use the TextEdit's own hit-test (accurate, gutter-independent),
                        // falling back to a galley lookup.
                        let char_idx = out
                            .cursor_range
                            .map(|r| r.primary.ccursor.index)
                            .or_else(|| {
                                out.response.interact_pointer_pos().map(|p| {
                                    out.galley.cursor_from_pos(p - out.galley_pos).ccursor.index
                                })
                            });
                        if let Some(ci) = char_idx {
                            match word_at(&tab.content, ci) {
                                Some(w) => goto = Some(w),
                                None => clicked_nothing = true,
                            }
                        }
                    }

                    // Scroll to a target line after a jump.
                    if let Some(line) = scroll_target {
                        let rows = out.galley.rows.len().max(1);
                        let row_h = out.galley.size().y / rows as f32;
                        let y = out.galley_pos.y + row_h * line as f32;
                        let target = Rect::from_min_size(
                            egui::pos2(out.galley_pos.x, y),
                            Vec2::new(8.0, row_h * 3.0),
                        );
                        ui.scroll_to_rect(target, Some(Align::Center));
                        clear_scroll = true;
                    }
                });
            });

        if clear_scroll {
            self.tabs[idx].scroll_to_line = None;
        }
        if clicked_nothing {
            self.status = "⌘+클릭: 식별자 위에서 클릭하세요".to_string();
        }
        if let Some(w) = goto {
            self.goto_definition(&w);
        }
    }
}

/// Number of distinct files in a (path-grouped) hit list — shown in the
/// search status line, computed once per result set instead of per frame.
fn count_hit_files(hits: &[crate::search::SearchHit]) -> usize {
    let mut files = 0;
    let mut prev: Option<&Path> = None;
    for h in hits {
        if prev != Some(h.path.as_path()) {
            files += 1;
            prev = Some(h.path.as_path());
        }
    }
    files
}

/// Whole-text char index of a (line, byte-column) position — used to focus the
/// preview on a specific match via the galley's char cursor.
fn char_index_of(content: &str, line: usize, col_byte: usize) -> usize {
    let start: usize = content
        .split_inclusive('\n')
        .take(line)
        .map(|l| l.chars().count())
        .sum();
    let target = content.lines().nth(line).unwrap_or("");
    let in_line = target
        .get(..col_byte.min(target.len()))
        .map(|s| s.chars().count())
        .unwrap_or(0);
    start + in_line
}

/// Extract the identifier surrounding `char_index` (a char offset into `text`).
fn word_at(text: &str, char_index: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = char_index.min(chars.len() - 1);
    if !is_word(chars[i]) && i > 0 && is_word(chars[i - 1]) {
        i -= 1;
    }
    if !is_word(chars[i]) {
        return None;
    }
    let mut start = i;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = i;
    while end + 1 < chars.len() && is_word(chars[end + 1]) {
        end += 1;
    }
    let word: String = chars[start..=end].iter().collect();
    if word.chars().all(|c| c.is_numeric()) {
        return None;
    }
    Some(word)
}

#[cfg(feature = "shot")]
fn save_png(path: &Path, image: &egui::ColorImage) {
    let [w, h] = [image.size[0] as u32, image.size[1] as u32];
    let mut bytes = Vec::with_capacity((w * h * 4) as usize);
    for px in &image.pixels {
        let [r, g, b, a] = px.to_array();
        bytes.extend_from_slice(&[r, g, b, a]);
    }
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("shot: cannot create {}: {e}", path.display());
            return;
        }
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    match enc.write_header().and_then(|mut wr| wr.write_image_data(&bytes)) {
        Ok(_) => eprintln!("shot: wrote {} ({w}x{h})", path.display()),
        Err(e) => eprintln!("shot: encode failed: {e}"),
    }
}

#[cfg(not(feature = "shot"))]
fn save_png(_path: &Path, _image: &egui::ColorImage) {}

fn load_children(dir: &Path) -> Vec<TreeNode> {
    let mut nodes = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = path.is_dir();
            if is_dir && IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
            if name == ".DS_Store" {
                continue;
            }
            nodes.push(TreeNode::new(path, is_dir));
        }
    }
    nodes.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    nodes
}

/// Draw a small breadcrumb chevron "›" centered in `rect`.
fn draw_crumb_sep(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let s = 3.0;
    let st = Stroke::new(1.2, color);
    painter.line_segment(
        [egui::pos2(center.x - s * 0.5, center.y - s), egui::pos2(center.x + s * 0.5, center.y)],
        st,
    );
    painter.line_segment(
        [egui::pos2(center.x + s * 0.5, center.y), egui::pos2(center.x - s * 0.5, center.y + s)],
        st,
    );
}

/// Draw a small git-branch glyph (two nodes on a trunk + a fork) centered at `c`.
fn draw_branch_icon(p: &egui::Painter, c: egui::Pos2, color: Color32) {
    let st = Stroke::new(1.2, color);
    let (top, bot, fork) = (
        egui::pos2(c.x - 2.5, c.y - 4.0),
        egui::pos2(c.x - 2.5, c.y + 4.0),
        egui::pos2(c.x + 3.0, c.y - 1.0),
    );
    p.line_segment([top, bot], st); // trunk
    p.line_segment([egui::pos2(c.x - 2.5, c.y + 1.0), fork], st); // branch out
    for n in [top, bot, fork] {
        p.circle_filled(n, 1.6, color);
    }
}

/// Draw an ✕ (tab close) centered in `rect`.
fn draw_x(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = 3.2;
    let st = Stroke::new(1.3, color);
    painter.line_segment([egui::pos2(c.x - s, c.y - s), egui::pos2(c.x + s, c.y + s)], st);
    painter.line_segment([egui::pos2(c.x - s, c.y + s), egui::pos2(c.x + s, c.y - s)], st);
}

/// Which edge a tool-window sits on (for the toggle icon).
#[derive(Clone, Copy)]
enum PanelSide {
    Left,
    Right,
    Bottom,
}

/// An IntelliJ-style tool-window toggle: a small window glyph with the docked
/// region highlighted. `active` = panel currently visible. Returns clicked.
fn panel_toggle(ui: &mut egui::Ui, side: PanelSide, active: bool, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(24.0, 22.0), Sense::click());
    let resp = resp.on_hover_text(tip);
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, C_HOVER);
    }
    // The window frame.
    let win = Rect::from_center_size(rect.center(), Vec2::new(15.0, 13.0));
    let frame_col = if active { C_TEXT } else { C_TEXT_DIM };
    p.rect_stroke(win, 2.0, Stroke::new(1.3, frame_col), egui::StrokeKind::Inside);
    // The docked region.
    let region = match side {
        PanelSide::Left => Rect::from_min_max(
            win.min,
            egui::pos2(win.left() + win.width() * 0.42, win.bottom()),
        ),
        PanelSide::Right => Rect::from_min_max(
            egui::pos2(win.right() - win.width() * 0.42, win.top()),
            win.max,
        ),
        PanelSide::Bottom => Rect::from_min_max(
            egui::pos2(win.left(), win.bottom() - win.height() * 0.42),
            win.max,
        ),
    };
    let fill = if active {
        C_ACCENT
    } else {
        C_TEXT_DIM.gamma_multiply(0.5)
    };
    p.rect_filled(region.shrink(1.2), 1.0, fill);
    resp.clicked()
}

/// A toolbar Back/Forward arrow button (drawn chevron). Greyed out when
/// disabled. Returns true when an enabled button is clicked.
fn nav_arrow(ui: &mut egui::Ui, forward: bool, enabled: bool) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(24.0, 22.0), sense);
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(rect, 4.0, C_HOVER);
    }
    let col = if !enabled {
        C_TEXT_DIM.gamma_multiply(0.4)
    } else if resp.hovered() {
        C_TEXT
    } else {
        C_TEXT_DIM
    };
    let c = rect.center();
    let (dx, s) = (2.5, 4.0);
    let st = Stroke::new(1.6, col);
    if forward {
        p.line_segment([egui::pos2(c.x - dx, c.y - s), egui::pos2(c.x + dx, c.y)], st);
        p.line_segment([egui::pos2(c.x + dx, c.y), egui::pos2(c.x - dx, c.y + s)], st);
    } else {
        p.line_segment([egui::pos2(c.x + dx, c.y - s), egui::pos2(c.x - dx, c.y)], st);
        p.line_segment([egui::pos2(c.x - dx, c.y), egui::pos2(c.x + dx, c.y + s)], st);
    }
    enabled && resp.clicked()
}

/// A small mid-height dot used as a lightweight separator in the toolbar.
fn sep_dot(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(3.0, ui.available_height()), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 1.5, C_TEXT_DIM.gamma_multiply(0.7));
}

/// IntelliJ-style tool-window header: a short uppercase title in a slim bar,
/// with an optional right-aligned trailing label, closed by a hairline border.
fn tool_window_header(ui: &mut egui::Ui, title: &str, trailing: Option<&str>) {
    let full_w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(full_w, 28.0), Sense::hover());
    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::proportional(11.5),
        C_HEADER,
    );
    if let Some(t) = trailing {
        painter.text(
            egui::pos2(rect.right() - 12.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            t,
            FontId::proportional(11.5),
            C_TEXT_DIM,
        );
    }
    painter.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom() - 0.5),
            egui::pos2(rect.right(), rect.bottom() - 0.5),
        ],
        Stroke::new(1.0, C_BORDER),
    );
}

const TREE_ROW_H: f32 = 24.0;
const TREE_INDENT: f32 = 14.0;
const TREE_PAD_L: f32 = 6.0;

/// One row in the Structure panel: colored kind-badge + symbol name, indented
/// by nesting depth. Returns true when clicked.
fn symbol_row(ui: &mut egui::Ui, s: &DocSymbol) -> bool {
    let full_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(full_w, 22.0), Sense::click());
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, 0.0, C_HOVER);
    }

    let base_x = rect.left() + TREE_PAD_L + s.depth as f32 * TREE_INDENT;
    let (r, g, b) = s.kind.rgb();
    let accent = Color32::from_rgb(r, g, b);

    // Rounded square badge with the kind glyph.
    let side = 15.0;
    let badge = Rect::from_center_size(
        egui::pos2(base_x + side / 2.0, rect.center().y),
        Vec2::splat(side),
    );
    painter.rect_filled(badge, side * 0.24, accent.gamma_multiply(0.30));
    painter.text(
        badge.center(),
        egui::Align2::CENTER_CENTER,
        s.kind.glyph(),
        FontId::monospace(11.0),
        accent,
    );

    painter.text(
        egui::pos2(badge.right() + 7.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &s.name,
        FontId::proportional(13.0),
        C_TEXT,
    );
    resp.clicked()
}

/// Relative time label from a unix timestamp (e.g. "3d", "5h", "just now").
fn rel_time(secs: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(secs);
    let d = (now - secs).max(0);
    if d < 60 {
        "방금".to_string()
    } else if d < 3600 {
        format!("{}분 전", d / 60)
    } else if d < 86400 {
        format!("{}시간 전", d / 3600)
    } else if d < 86400 * 30 {
        format!("{}일 전", d / 86400)
    } else if d < 86400 * 365 {
        format!("{}개월 전", d / (86400 * 30))
    } else {
        format!("{}년 전", d / (86400 * 365))
    }
}

fn status_color(s: crate::git::FileStatus) -> Color32 {
    use crate::git::FileStatus::*;
    match s {
        Modified | Renamed => Color32::from_rgb(0x6c, 0x9c, 0xd2), // blue
        Added | Untracked => Color32::from_rgb(0x62, 0xb5, 0x43),  // green
        Deleted => Color32::from_rgb(0x9a, 0x9a, 0x9a),            // gray
        Conflicted => Color32::from_rgb(0xe0, 0x6c, 0x75),         // red
    }
}

fn show_node(
    ui: &mut egui::Ui,
    node: &mut TreeNode,
    depth: usize,
    active: Option<&Path>,
    status: &HashMap<PathBuf, crate::git::FileStatus>,
    to_open: &mut Option<PathBuf>,
) {
    let selected = !node.is_dir && active == Some(node.path.as_path());

    let full_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(full_w, TREE_ROW_H), Sense::click());
    let painter = ui.painter();

    // Row background: full-width selection / hover.
    if selected {
        painter.rect_filled(rect, 0.0, C_SEL);
    } else if resp.hovered() {
        painter.rect_filled(rect, 0.0, C_HOVER);
    }

    let base_x = rect.left() + TREE_PAD_L + depth as f32 * TREE_INDENT;

    // Indent guides (faint vertical lines under each ancestor).
    if depth > 0 {
        let guide = C_BORDER.gamma_multiply(0.8);
        for i in 0..depth {
            let gx = rect.left() + TREE_PAD_L + i as f32 * TREE_INDENT + 7.0;
            painter.line_segment(
                [egui::pos2(gx, rect.top()), egui::pos2(gx, rect.bottom())],
                Stroke::new(1.0, guide),
            );
        }
    }

    let icon_side = 15.0;
    let chevron_rect = Rect::from_min_size(egui::pos2(base_x, rect.top()), Vec2::new(14.0, TREE_ROW_H));
    let icon_rect = Rect::from_center_size(
        egui::pos2(base_x + 14.0 + icon_side / 2.0, rect.center().y),
        Vec2::splat(icon_side),
    );
    let text_x = icon_rect.right() + 6.0;

    if node.is_dir {
        icons::draw_chevron(painter, chevron_rect, node.expanded, resp.hovered());
        icons::draw_folder_icon(painter, icon_rect, node.expanded);
    } else {
        icons::draw_file_icon(painter, icon_rect, &node.name);
    }

    // Color by git status (files only); selection still forces white.
    let node_status = if node.is_dir {
        None
    } else {
        status.get(&node.path).copied()
    };
    let text_color = if selected {
        Color32::WHITE
    } else if let Some(s) = node_status {
        status_color(s)
    } else {
        C_TEXT
    };
    painter.text(
        egui::pos2(text_x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &node.name,
        FontId::proportional(13.5),
        text_color,
    );

    if resp.clicked() {
        if node.is_dir {
            node.expanded = !node.expanded;
            if node.expanded && node.children.is_none() {
                node.children = Some(load_children(&node.path));
            }
        } else {
            *to_open = Some(node.path.clone());
        }
    }

    if node.is_dir && node.expanded {
        if let Some(children) = &mut node.children {
            for child in children.iter_mut() {
                show_node(ui, child, depth + 1, active, status, to_open);
            }
        }
    }
}

/// Wire up the IDE font stack:
///   • code  → JetBrains Mono (bundled), the IntelliJ editor font
///   • UI    → SF Pro Text (system, runtime-loaded), the macOS UI font
///   • both fall back to a system CJK face so Korean/CJK always renders.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Bundled JetBrains Mono for code.
    fonts.font_data.insert(
        "jbmono".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/JetBrainsMono-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "jbmono-medium".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/JetBrainsMono-Medium.ttf"
        ))),
    );

    // System UI font (SF Pro Text). Loaded at runtime, never bundled.
    let ui_font = [
        "/System/Library/Fonts/SFNSText.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/SFNSDisplay.ttf",
        "/Library/Fonts/SF-Pro-Text-Regular.otf",
    ]
    .iter()
    .find_map(|p| std::fs::read(p).ok());
    if let Some(bytes) = ui_font {
        fonts
            .font_data
            .insert("ui".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
    }

    // System CJK fallback.
    let cjk = [
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
        "/System/Library/Fonts/Supplemental/NotoSansGothic-Regular.ttf",
        "/System/Library/Fonts/PingFang.ttc",
    ]
    .iter()
    .find_map(|p| std::fs::read(p).ok());
    if let Some(bytes) = cjk {
        fonts
            .font_data
            .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
    }

    let prop = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    prop.clear();
    if fonts.font_data.contains_key("ui") {
        prop.push("ui".to_owned());
    }
    prop.push("jbmono".to_owned()); // covers glyphs SF lacks
    if fonts.font_data.contains_key("cjk") {
        prop.push("cjk".to_owned());
    }

    let mono = fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default();
    mono.clear();
    mono.push("jbmono".to_owned());
    if fonts.font_data.contains_key("cjk") {
        mono.push("cjk".to_owned());
    }
    let _ = "jbmono-medium"; // available for emphasis where needed

    ctx.set_fonts(fonts);
}

// IntelliJ "New UI" dark palette.
pub const C_PANEL: Color32 = Color32::from_rgb(0x2b, 0x2d, 0x30);
pub const C_EDITOR: Color32 = Color32::from_rgb(0x1e, 0x1f, 0x22);
pub const C_BORDER: Color32 = Color32::from_rgb(0x39, 0x3b, 0x40);
pub const C_TEXT: Color32 = Color32::from_rgb(0xbc, 0xbe, 0xc4);
pub const C_TEXT_DIM: Color32 = Color32::from_rgb(0x82, 0x84, 0x8c);
pub const C_SEL: Color32 = Color32::from_rgb(0x2e, 0x43, 0x6e); // focused tree selection
pub const C_HOVER: Color32 = Color32::from_rgb(0x33, 0x35, 0x39);
pub const C_ACCENT: Color32 = Color32::from_rgb(0x35, 0x74, 0xf0); // active tab underline
pub const C_HEADER: Color32 = Color32::from_rgb(0x9d, 0xa0, 0xa8); // tool-window header text

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = C_PANEL;
    visuals.window_fill = C_PANEL;
    visuals.extreme_bg_color = C_EDITOR;
    visuals.override_text_color = Some(C_TEXT);
    visuals.selection.bg_fill = Color32::from_rgb(0x21, 0x4d, 0x83);
    visuals.selection.stroke = egui::Stroke::NONE;
    visuals.hyperlink_color = C_ACCENT;
    visuals.window_stroke = egui::Stroke::new(1.0, C_BORDER);

    // Flatten widget chrome — IDE tool windows have almost no button borders.
    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.bg_stroke = egui::Stroke::NONE;
    }
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, C_BORDER);
    visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.hovered.weak_bg_fill = C_HOVER;
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(0x3d, 0x40, 0x45);
    visuals.widgets.hovered.bg_fill = C_HOVER;
    visuals.widgets.active.bg_fill = Color32::from_rgb(0x3d, 0x40, 0x45);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(6.0, 3.0);
    style.spacing.button_padding = Vec2::new(7.0, 3.0);
    style.spacing.indent = 14.0;
    // Solid (always-visible) thin scrollbars. The default floating/thin bars
    // fade in and out on hover, which reads as flicker near panel edges.
    let mut scroll = egui::style::ScrollStyle::solid();
    scroll.bar_width = 9.0;
    scroll.floating = false;
    style.spacing.scroll = scroll;
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::word_at;

    #[test]
    fn picks_identifier_under_cursor() {
        let s = "fun fetchClient(id: ClientId) {}";
        let p = s.find("ClientId").unwrap();
        assert_eq!(word_at(s, p).as_deref(), Some("ClientId"));
        assert_eq!(word_at(s, p + 3).as_deref(), Some("ClientId")); // middle
        assert_eq!(word_at(s, s.find("fetchClient").unwrap()).as_deref(), Some("fetchClient"));
    }

    #[test]
    fn ignores_numbers_and_punctuation() {
        let s = "let n = 42;";
        assert_eq!(word_at(s, s.find("42").unwrap()), None);
        assert_eq!(word_at(s, s.find(';').unwrap()), None);
    }

    #[test]
    fn handles_unicode_identifier() {
        let s = "val 사용자이름 = name";
        // char index of the Korean identifier start
        let ci = s.chars().take_while(|c| *c != '사').count();
        assert_eq!(word_at(s, ci).as_deref(), Some("사용자이름"));
    }
}
