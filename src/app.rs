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
    /// Last caret position (char index / line), tracked so jumps can record
    /// the exact origin for Back and ⌘B can resolve the word under the caret.
    /// Updated only when the TextEdit caret actually moves (see editor()) —
    /// jumps overwrite it with the destination so stale carets never leak
    /// into the navigation history.
    caret_ci: Option<usize>,
    caret_line: Option<usize>,
    /// Raw TextEdit caret from the previous frame (change detector).
    last_cursor_ci: Option<usize>,
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
            caret_ci: None,
            caret_line: None,
            last_cursor_ci: None,
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
    /// The whole diff body as one syntax-highlighted document, rendered via a
    /// read-only TextEdit so the text is drag-selectable / copyable.
    text: String,
    job: LayoutJob,
    /// TextEdit buffer; reverted to `text` on any edit (read-only).
    edit_buf: String,
    /// Font size the job was built at (rebuilt on zoom).
    jobs_font: f32,
}

/// Merge the per-line highlighted jobs into one whole-document LayoutJob
/// (uniform line height) whose text is every diff row joined by newlines.
fn build_diff_doc(
    hl: &Highlighter,
    file: &str,
    lines: &[crate::git::DiffLine],
    font_size: f32,
) -> (String, LayoutJob) {
    let line_jobs = build_diff_jobs(hl, file, lines, font_size);
    let lh = crate::highlight::line_height(font_size);
    let plain_fmt = |color: Color32| {
        let mut f = TextFormat::simple(FontId::monospace(font_size), color);
        f.line_height = Some(lh);
        f
    };
    let hunk_color = Color32::from_rgb(0x56, 0xb6, 0xc2);

    let mut text = String::new();
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    for (i, dl) in lines.iter().enumerate() {
        let base = text.len();
        match line_jobs.get(i).and_then(|j| j.as_ref()) {
            Some(lj) if !lj.text.is_empty() => {
                text.push_str(&lj.text);
                for sec in &lj.sections {
                    job.sections.push(egui::text::LayoutSection {
                        leading_space: 0.0,
                        byte_range: (sec.byte_range.start + base)..(sec.byte_range.end + base),
                        format: sec.format.clone(),
                    });
                }
            }
            _ => {
                text.push_str(&dl.text);
                if text.len() > base {
                    let color = if dl.kind == crate::git::DiffKind::Hunk {
                        hunk_color
                    } else {
                        C_TEXT
                    };
                    job.sections.push(egui::text::LayoutSection {
                        leading_space: 0.0,
                        byte_range: base..text.len(),
                        format: plain_fmt(color),
                    });
                }
            }
        }
        if i + 1 < lines.len() {
            let b = text.len();
            text.push('\n');
            job.sections.push(egui::text::LayoutSection {
                leading_space: 0.0,
                byte_range: b..text.len(),
                format: plain_fmt(C_TEXT),
            });
        }
    }
    job.text = text.clone();
    (text, job)
}

/// Reconstruct the old/new documents from a unified diff, highlight each once
/// with the file's real grammar, and hand every diff row its slice: deleted
/// rows read from the old document, added/context rows from the new one.
fn build_diff_jobs(
    hl: &Highlighter,
    file: &str,
    lines: &[crate::git::DiffLine],
    font_size: f32,
) -> Vec<Option<LayoutJob>> {
    use crate::git::DiffKind::*;
    let mut old_doc = String::new();
    let mut new_doc = String::new();
    // For each diff row: which side it reads from and its line index there.
    let mut source: Vec<Option<(bool, usize)>> = Vec::with_capacity(lines.len()); // (is_old, line)
    let (mut old_n, mut new_n) = (0usize, 0usize);
    for dl in lines {
        match dl.kind {
            Del => {
                old_doc.push_str(&dl.text);
                old_doc.push('\n');
                source.push(Some((true, old_n)));
                old_n += 1;
            }
            Add => {
                new_doc.push_str(&dl.text);
                new_doc.push('\n');
                source.push(Some((false, new_n)));
                new_n += 1;
            }
            Context => {
                old_doc.push_str(&dl.text);
                old_doc.push('\n');
                old_n += 1;
                new_doc.push_str(&dl.text);
                new_doc.push('\n');
                source.push(Some((false, new_n)));
                new_n += 1;
            }
            Hunk => source.push(None),
        }
    }
    let old_jobs = split_job_lines(&old_doc, &hl.highlight(file, &old_doc, font_size));
    let new_jobs = split_job_lines(&new_doc, &hl.highlight(file, &new_doc, font_size));
    source
        .into_iter()
        .map(|s| {
            s.and_then(|(is_old, n)| {
                if is_old {
                    old_jobs.get(n).cloned()
                } else {
                    new_jobs.get(n).cloned()
                }
            })
        })
        .collect()
}

/// Split a whole-document LayoutJob into one single-line job per line, with
/// section byte ranges clipped and rebased. Sections are assumed sorted by
/// start (true for the highlighter's output).
fn split_job_lines(doc: &str, job: &LayoutJob) -> Vec<LayoutJob> {
    let bytes = doc.as_bytes();
    let mut out = Vec::new();
    let mut ls = 0usize;
    let mut si = 0usize; // first section that may overlap the current line
    for i in 0..=bytes.len() {
        if i == bytes.len() || bytes[i] == b'\n' {
            if i == bytes.len() && ls > i {
                break; // trailing newline already consumed
            }
            let le = i;
            let mut lj = LayoutJob {
                text: doc[ls..le].to_string(),
                ..Default::default()
            };
            lj.wrap.max_width = f32::INFINITY;
            while si < job.sections.len() && job.sections[si].byte_range.end <= ls {
                si += 1;
            }
            let mut j = si;
            while j < job.sections.len() && job.sections[j].byte_range.start < le {
                let a = job.sections[j].byte_range.start.max(ls);
                let b = job.sections[j].byte_range.end.min(le);
                if a < b {
                    lj.sections.push(egui::text::LayoutSection {
                        leading_space: 0.0,
                        byte_range: (a - ls)..(b - ls),
                        format: job.sections[j].format.clone(),
                    });
                }
                j += 1;
            }
            out.push(lj);
            ls = i + 1;
        }
    }
    out
}

/// Which view occupies the left side panel (IntelliJ tool-window strip).
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeftView {
    Project,
    Commit,
}

/// Single status letter for changed-file lists.
fn status_letter(s: crate::git::FileStatus) -> &'static str {
    use crate::git::FileStatus::*;
    match s {
        Added => "A",
        Untracked => "U",
        Deleted => "D",
        Renamed => "R",
        Conflicted => "C",
        _ => "M",
    }
}

/// What the "Go to …" finder popup is currently searching over.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinderMode {
    File,
    Symbol,
    Line,
    Recent,
}

impl FinderMode {
    fn title(&self) -> &'static str {
        match self {
            FinderMode::File => "파일로 이동",
            FinderMode::Symbol => "심볼로 이동",
            FinderMode::Line => "줄로 이동",
            FinderMode::Recent => "최근 파일",
        }
    }
    fn hint(&self) -> &'static str {
        match self {
            FinderMode::File => "파일 이름 입력",
            FinderMode::Symbol => "심볼 이름 입력",
            FinderMode::Line => "줄 번호 입력",
            FinderMode::Recent => "필터 입력",
        }
    }
}

/// Everything that belongs to ONE opened project — its tree, open file tabs,
/// git state, search state, symbol index, navigation history. The app keeps
/// the active workspace in `CodeLookApp::ws` and parks the rest, so several
/// projects can be open at once and switched via the project tab strip.
pub struct Workspace {
    /// Open order — project tabs display sorted by this.
    seq: usize,
    project_root: Option<PathBuf>,
    tree: Option<TreeNode>,
    /// One-shot: scroll the project tree to this path on the next frame
    /// ("Select Opened File").
    reveal_path: Option<PathBuf>,
    tabs: Vec<Tab>,
    active: Option<usize>,
    symbol_index: Option<SymbolIndex>,
    index_rx: Option<Receiver<(SymbolIndex, Vec<PathBuf>)>>,
    indexing: bool,
    status: String,
    /// All project files (from the index walk) — powers "Go to File".
    files: Vec<PathBuf>,
    /// Recently opened files, most recent first (capped).
    recent: Vec<PathBuf>,
    // "Go to …" finder popup (file / symbol / line / recent).
    finder_open: bool,
    finder_mode: FinderMode,
    finder_query: String,
    finder_sel: usize,
    finder_focus: bool,
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
    git_rx: Option<Receiver<(HashMap<PathBuf, crate::git::FileStatus>, Option<String>, usize, usize)>>,
    // Branch switcher popup + running git CLI operation (checkout/pull/fetch).
    branch_menu_open: bool,
    branch_menu_pos: egui::Pos2,
    branch_filter: String,
    branch_focus: bool,
    branches: Vec<crate::git::BranchInfo>,
    git_op: Option<(String, Receiver<(bool, String)>)>,
    // Ahead/behind vs upstream + the quiet background fetch that keeps the
    // "↓ pull 필요" badge honest.
    git_ahead: usize,
    git_behind: usize,
    autofetch_rx: Option<Receiver<(usize, usize)>>,
    autofetch_at: Option<f64>,
    // Commit Log panel + diff viewer.
    log_open: bool,
    /// Commit-list / file-list split as a fraction of the panel width.
    log_split: f32,
    commits: Vec<crate::git::CommitInfo>,
    commits_rx: Option<Receiver<Vec<crate::git::CommitInfo>>>,
    commit_sel: usize,
    /// The pinned "변경사항 (커밋 전)" row is selected instead of a commit.
    local_sel: bool,
    /// Last periodic working-tree status refresh (while reviewing changes).
    local_refresh_at: Option<f64>,
    commit_files: Vec<crate::git::FileChange>,
    commit_files_for: String,
    diff_view: Option<DiffView>,
}

impl Workspace {
    fn new(seq: usize) -> Self {
        Self {
            seq,
            project_root: None,
            tree: None,
            reveal_path: None,
            tabs: Vec::new(),
            active: None,
            symbol_index: None,
            index_rx: None,
            indexing: false,
            status: String::new(),
            files: Vec::new(),
            recent: Vec::new(),
            finder_open: false,
            finder_mode: FinderMode::File,
            finder_query: String::new(),
            finder_sel: 0,
            finder_focus: false,
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
            branch_menu_open: false,
            branch_menu_pos: egui::Pos2::ZERO,
            branch_filter: String::new(),
            branch_focus: false,
            branches: Vec::new(),
            git_op: None,
            git_ahead: 0,
            git_behind: 0,
            autofetch_rx: None,
            autofetch_at: None,
            log_open: false,
            log_split: 0.6,
            commits: Vec::new(),
            commits_rx: None,
            commit_sel: 0,
            local_sel: false,
            local_refresh_at: None,
            commit_files: Vec::new(),
            commit_files_for: String::new(),
            diff_view: None,
        }
    }

    /// Stop this workspace's in-flight background search (called on close).
    fn cancel_background(&self) {
        if let Some(c) = &self.gsearch_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub struct CodeLookApp {
    /// The active project.
    ws: Workspace,
    /// Other open projects (order irrelevant — tabs sort by `seq`).
    parked: Vec<Workspace>,
    next_seq: usize,
    highlighter: Highlighter,
    font_size: f32,
    // Configurable shortcuts + settings window state.
    keymap: crate::keymap::Keymap,
    settings_open: bool,
    /// Action currently waiting for a new key chord in the settings window.
    rebind: Option<crate::keymap::Action>,
    // Tool-window visibility toggles.
    tree_open: bool,
    structure_open: bool,
    /// Which view the left panel shows (project tree / commit changes).
    left_view: LeftView,
    // Double-Shift (Search Everywhere) detection.
    shift_prev: bool,
    last_shift_at: f64,
    // IntelliJ expUI icon textures (files / folders / structure symbols).
    icons: icons::IconSet,
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
        let restored_all: Vec<PathBuf> = cc
            .storage
            .and_then(|s| s.get_string("open_projects"))
            .map(|s| s.lines().map(PathBuf::from).filter(|p| p.is_dir()).collect())
            .unwrap_or_default();

        let mut app = Self {
            ws: Workspace::new(0),
            parked: Vec::new(),
            next_seq: 1,
            highlighter: Highlighter::new(),
            font_size: 14.0,
            keymap: cc
                .storage
                .and_then(|s| s.get_string("keymap"))
                .map(|s| crate::keymap::Keymap::deserialize(&s))
                .unwrap_or_else(crate::keymap::Keymap::default_map),
            settings_open: false,
            rebind: None,
            left_view: LeftView::Project,
            shift_prev: false,
            last_shift_at: f64::NEG_INFINITY,
            tree_open: true,
            structure_open: true,
            icons: icons::IconSet::new(&cc.egui_ctx),
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
            if let Some(root) = app.ws.project_root.clone() {
                app.ws.git_status = crate::git::status_map(&root);
                app.ws.git_branch = crate::git::current_branch(&root);
                let (a, b) = crate::git::ahead_behind(&root).unwrap_or((0, 0));
                app.ws.git_ahead = a;
                app.ws.git_behind = b;
                if s.log {
                    app.ws.log_open = true;
                    app.ws.commits = crate::git::commit_log(&root, 300);
                    if !app.ws.git_status.is_empty() {
                        // Dirty tree → show the Local Changes review view.
                        app.ws.local_sel = true;
                        let mut files: Vec<String> = app
                            .ws
                            .git_status
                            .keys()
                            .map(|p| {
                                p.strip_prefix(&root)
                                    .unwrap_or(p)
                                    .to_string_lossy()
                                    .to_string()
                            })
                            .collect();
                        files.sort();
                        if let Some(f) = files.first().cloned() {
                            app.open_working_diff(f);
                        }
                    } else if !app.ws.commits.is_empty() {
                        app.select_commit(0);
                        if let Some(f) = app.ws.commit_files.first().cloned() {
                            app.open_commit_diff(f.path);
                        }
                    }
                }
            }
            // Populate the global-search popup synchronously for the screenshot.
            if let (Some(q), Some(root)) = (&s.gsearch, app.ws.project_root.clone()) {
                let (hits, tr) = crate::search::global_search(&root, q);
                app.ws.gsearch_files = count_hit_files(&hits);
                app.ws.gsearch_results = hits;
                app.ws.gsearch_truncated = tr;
                app.ws.gsearch_query = q.clone();
                app.ws.gsearch_open = true;
            }
            return app;
        }

        if let Some(p) = initial {
            app.open_project(&cc.egui_ctx, p);
        } else {
            // Restore every project that was open, then focus the last active
            // one (open_project just switches when it's already open).
            for p in restored_all {
                app.open_project(&cc.egui_ctx, p);
            }
            if let Some(p) = restored {
                app.open_project(&cc.egui_ctx, p);
            }
        }
        app
    }

    // ---- Multi-project workspaces -------------------------------------------

    /// Bring an already-open project to the front.
    fn switch_project(&mut self, root: &Path) {
        if self.ws.project_root.as_deref() == Some(root) {
            return;
        }
        if let Some(i) = self
            .parked
            .iter()
            .position(|w| w.project_root.as_deref() == Some(root))
        {
            std::mem::swap(&mut self.ws, &mut self.parked[i]);
        }
    }

    /// Close an open project tab. Closing the active one activates the most
    /// recently opened remaining project (or leaves an empty workspace).
    fn close_project(&mut self, root: &Path) {
        if let Some(i) = self
            .parked
            .iter()
            .position(|w| w.project_root.as_deref() == Some(root))
        {
            self.parked.remove(i).cancel_background();
            return;
        }
        if self.ws.project_root.as_deref() == Some(root) {
            self.ws.cancel_background();
            let seq = self.next_seq;
            self.next_seq += 1;
            let closed = std::mem::replace(&mut self.ws, Workspace::new(seq));
            drop(closed);
            // Activate the most recently opened remaining project.
            if let Some((i, _)) = self
                .parked
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| w.seq)
                .map(|(i, w)| (i, w.seq))
            {
                let w = self.parked.remove(i);
                self.ws = w;
            }
        }
    }

    fn open_project(&mut self, ctx: &egui::Context, path: PathBuf) {
        let path = path.canonicalize().unwrap_or(path);
        // Already open → just focus its tab.
        if self.ws.project_root.as_ref() == Some(&path) {
            return;
        }
        if self
            .parked
            .iter()
            .any(|w| w.project_root.as_ref() == Some(&path))
        {
            self.switch_project(&path);
            return;
        }
        // Park the current project and load the new one into a fresh tab.
        if self.ws.project_root.is_some() {
            let seq = self.next_seq;
            self.next_seq += 1;
            let old = std::mem::replace(&mut self.ws, Workspace::new(seq));
            self.parked.push(old);
        }
        let mut root = TreeNode::new(path.clone(), true);
        root.expanded = true;
        root.children = Some(load_children(&path));
        self.ws.tree = Some(root);
        self.ws.project_root = Some(path.clone());
        self.ws.tabs.clear();
        self.ws.active = None;
        self.ws.symbol_index = None;

        let (tx, rx) = std::sync::mpsc::channel();
        let ctx2 = ctx.clone();
        let index_path = path.clone();
        std::thread::spawn(move || {
            let idx = symbols::build_index(&index_path);
            let _ = tx.send(idx);
            ctx2.request_repaint();
        });
        self.ws.index_rx = Some(rx);
        self.ws.indexing = true;
        self.ws.status = "프로젝트 인덱싱 중…".to_string();

        // Drop the search worker (its snapshot belongs to the old root) and
        // any in-flight query.
        if let Some(c) = &self.ws.gsearch_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.ws.gsearch_worker = None;
        self.ws.gsearch_cancel = None;
        self.ws.gsearch_rx = None;
        self.ws.gsearch_results.clear();
        self.ws.gsearch_files = 0;
        self.ws.gsearch_truncated = false;
        self.ws.gsearch_running = false;
        self.ws.gsearch_sel = 0;
        self.ws.gsearch_open = false;

        // Reset commit-log / diff state for the new project.
        self.ws.commits.clear();
        self.ws.commits_rx = None;
        self.ws.commit_files.clear();
        self.ws.commit_files_for.clear();
        self.ws.commit_sel = 0;
        self.ws.diff_view = None;

        // Git status + branch on a background thread (opens the repo in-thread).
        self.ws.git_status.clear();
        self.ws.git_branch = None;
        let (gtx, grx) = std::sync::mpsc::channel();
        let ctx3 = ctx.clone();
        let gpath = path.clone();
        std::thread::spawn(move || {
            let status = crate::git::status_map(&gpath);
            let branch = crate::git::current_branch(&gpath);
            let (ahead, behind) = crate::git::ahead_behind(&gpath).unwrap_or((0, 0));
            let _ = gtx.send((status, branch, ahead, behind));
            ctx3.request_repaint();
        });
        self.ws.git_rx = Some(grx);

        // For a git repository, open the commit log automatically and load it.
        self.ws.log_open = false;
        if crate::git::current_branch(&path).is_some() {
            self.ws.log_open = true;
            let (ctx4, cpath) = (ctx.clone(), path.clone());
            let (ctx_tx, ctx_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let log = crate::git::commit_log(&cpath, 300);
                let _ = ctx_tx.send(log);
                ctx4.request_repaint();
            });
            self.ws.commits_rx = Some(ctx_rx);
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        self.ws.recent.retain(|p| p != &path);
        self.ws.recent.insert(0, path.clone());
        self.ws.recent.truncate(30);
        if let Some(i) = self.ws.tabs.iter().position(|t| t.path == path) {
            self.ws.active = Some(i);
            self.ws.diff_view = None; // an open diff would keep covering the editor
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.ws.diff_view = None;
                let job =
                    self.highlighter
                        .highlight(path.to_str().unwrap_or(""), &content, self.font_size);
                let lang = ast::Lang::from_path(&path);
                let outline = lang
                    .map(|l| ast::document_symbols(l, &content))
                    .unwrap_or_default();
                let git_changes = self
                    .ws
                    .project_root
                    .as_ref()
                    .and_then(|root| crate::git::file_line_changes(root, &path))
                    .unwrap_or_default();
                let mut tab = Tab::new(path, content, job, self.font_size, lang, outline);
                tab.git_changes = git_changes;
                self.ws.tabs.push(tab);
                self.ws.active = Some(self.ws.tabs.len() - 1);
                self.ws.status.clear();
                self.refresh_search();
            }
            Err(_) => {
                self.ws.status = format!(
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
        if self.ws.history.get(self.ws.hist_pos) == Some(&loc) {
            return; // already here
        }
        if !self.ws.history.is_empty() {
            self.ws.history.truncate(self.ws.hist_pos + 1);
        }
        self.ws.history.push(loc);
        self.ws.hist_pos = self.ws.history.len() - 1;
    }

    fn can_back(&self) -> bool {
        !self.ws.history.is_empty() && self.ws.hist_pos > 0
    }

    fn can_forward(&self) -> bool {
        self.ws.hist_pos + 1 < self.ws.history.len()
    }

    fn nav_back(&mut self) {
        if !self.can_back() {
            return;
        }
        self.pin_current();
        self.ws.hist_pos -= 1;
        self.go_to_loc(self.ws.history[self.ws.hist_pos].clone());
    }

    fn nav_forward(&mut self) {
        if !self.can_forward() {
            return;
        }
        self.pin_current();
        self.ws.hist_pos += 1;
        self.go_to_loc(self.ws.history[self.ws.hist_pos].clone());
    }

    /// Where the user is right now: active tab + caret line (falling back to
    /// the last jumped-to line).
    fn current_loc(&self) -> Option<NavLoc> {
        let i = self.ws.active?;
        let t = self.ws.tabs.get(i)?;
        Some(NavLoc {
            path: t.path.clone(),
            line: t.caret_line.or(t.flash_line).unwrap_or(0),
        })
    }

    /// Record the exact current spot before a jump, so Back returns to where
    /// the caret was (not merely to the previous jump target).
    fn record_origin(&mut self) {
        if let Some(loc) = self.current_loc() {
            self.record_nav(loc.path, loc.line);
        }
    }

    /// A plain editor click becomes a navigation point. Clicks within a few
    /// lines of the current entry just refine its position; farther clicks
    /// push a new entry (so Back walks click history like IntelliJ).
    fn note_click_nav(&mut self, line: usize) {
        let Some(path) = self
            .ws
            .active
            .and_then(|i| self.ws.tabs.get(i))
            .map(|t| t.path.clone())
        else {
            return;
        };
        if let Some(top) = self.ws.history.get_mut(self.ws.hist_pos) {
            if top.path == path && top.line.abs_diff(line) < 5 {
                top.line = line;
                return;
            }
        }
        self.record_nav(path, line);
    }

    /// Before moving through history, update the current entry's line to the
    /// live caret position so Forward/Back return to the precise spot.
    fn pin_current(&mut self) {
        if let Some(loc) = self.current_loc() {
            if let Some(entry) = self.ws.history.get_mut(self.ws.hist_pos) {
                if entry.path == loc.path {
                    entry.line = loc.line;
                }
            }
        }
    }

    /// Navigate to a history location WITHOUT recording (avoids feedback loops).
    fn go_to_loc(&mut self, loc: NavLoc) {
        self.open_file(loc.path.clone());
        self.set_jump_target(loc.line);
    }

    // ---- Project-wide search ("Find in Files") -----------------------------

    /// Create the persistent search worker for the current project root.
    fn ensure_gsearch_worker(&mut self, ctx: &egui::Context) {
        if self.ws.gsearch_worker.is_some() {
            return;
        }
        let root = match &self.ws.project_root {
            Some(r) => r.clone(),
            None => return,
        };
        let ctx2 = ctx.clone();
        let (worker, rx) = crate::search::Worker::new(root, move || ctx2.request_repaint());
        self.ws.gsearch_worker = Some(worker);
        self.ws.gsearch_rx = Some(rx);
    }

    /// Pre-build the file snapshot (called when the search popup opens), so
    /// the first real query only pays the scan, not the initial disk walk.
    fn gsearch_warm(&mut self, ctx: &egui::Context) {
        self.ensure_gsearch_worker(ctx);
        if let (Some(w), None) = (&self.ws.gsearch_worker, &self.ws.gsearch_cancel) {
            self.ws.gsearch_cancel = Some(w.submit(String::new()));
        }
    }

    /// Queue the current query on the worker (min 2 chars), cancelling the
    /// previous one.
    fn gsearch_kick(&mut self, ctx: &egui::Context) {
        if let Some(c) = &self.ws.gsearch_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
            self.ws.gsearch_cancel = None;
        }
        let query = self.ws.gsearch_query.clone();
        if query.trim().len() < 2 {
            self.ws.gsearch_results.clear();
            self.ws.gsearch_files = 0;
            self.ws.gsearch_truncated = false;
            self.ws.gsearch_running = false;
            return;
        }
        self.ensure_gsearch_worker(ctx);
        if let Some(w) = &self.ws.gsearch_worker {
            self.ws.gsearch_cancel = Some(w.submit(query));
            self.ws.gsearch_running = true;
        }
    }

    /// Accept background results matching the current query (drop stale).
    fn gsearch_poll(&mut self) {
        let mut accepted = None;
        if let Some(rx) = &self.ws.gsearch_rx {
            while let Ok(reply) = rx.try_recv() {
                if reply.0 == self.ws.gsearch_query {
                    accepted = Some(reply);
                }
            }
        }
        if let Some((_, hits, truncated)) = accepted {
            self.ws.gsearch_files = count_hit_files(&hits);
            self.ws.gsearch_results = hits;
            self.ws.gsearch_truncated = truncated;
            self.ws.gsearch_sel = 0;
            self.ws.gsearch_running = false;
        }
    }

    /// Prepare the preview pane for the currently selected search hit: highlight
    /// the file (cached per path) and mark it to scroll to the matched line.
    fn ensure_gpreview(&mut self) {
        let hit = match self.ws.gsearch_results.get(self.ws.gsearch_sel) {
            Some(h) => h.clone(),
            None => {
                self.ws.gpreview_path = None;
                self.ws.gpreview_job = LayoutJob::default();
                self.ws.gpreview_content.clear();
                self.ws.gpreview_marks.clear();
                self.ws.gpreview_lines = 0;
                return;
            }
        };
        let path_changed = self.ws.gpreview_path.as_deref() != Some(hit.path.as_path());
        if path_changed {
            match std::fs::read_to_string(&hit.path) {
                Ok(content) => {
                    self.ws.gpreview_job = self.highlighter.highlight(
                        hit.path.to_str().unwrap_or(""),
                        &content,
                        13.0,
                    );
                    let lines = content.lines().count().max(1);
                    self.ws.gpreview_lines = lines;
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
                    self.ws.gpreview_gutter = g;
                    self.ws.gpreview_content = content;
                }
                Err(_) => {
                    self.ws.gpreview_job = LayoutJob::default();
                    self.ws.gpreview_gutter = LayoutJob::default();
                    self.ws.gpreview_content.clear();
                    self.ws.gpreview_lines = 0;
                }
            }
            self.ws.gpreview_path = Some(hit.path.clone());
            self.ws.gpreview_line = hit.line;
            self.ws.gpreview_focus_ci = char_index_of(&self.ws.gpreview_content, hit.line, hit.col);
            self.ws.gpreview_scroll = true;
        } else if self.ws.gpreview_line != hit.line {
            self.ws.gpreview_line = hit.line;
            self.ws.gpreview_focus_ci = char_index_of(&self.ws.gpreview_content, hit.line, hit.col);
            self.ws.gpreview_scroll = true;
        }

        // (Re)compute match markers when the file or the query term changes.
        // Matching is ASCII-case-insensitive (crate::search), so byte offsets
        // are exact and a match's char length equals the query's.
        let needle = self.ws.gsearch_query.clone();
        if path_changed || needle != self.ws.gpreview_needle {
            let mut marks = Vec::new();
            let len_char = needle.chars().count();
            if !needle.is_empty() {
                let content = &self.ws.gpreview_content;
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
            self.ws.gpreview_needle = needle;
            self.ws.gpreview_marks = marks;
        }
    }

    // ---- Keyboard shortcuts (configurable, see keymap.rs) -------------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use crate::keymap::Action as A;
        if self.keymap.pressed(ctx, A::FindInFile) && self.ws.active.is_some() {
            self.ws.search_open = true;
            self.ws.search_focus = true;
        }
        if self.keymap.pressed(ctx, A::FindInProject) && self.ws.project_root.is_some() {
            self.ws.gsearch_open = true;
            self.ws.gsearch_focus = true;
            self.gsearch_warm(ctx);
        }
        if self.keymap.pressed(ctx, A::GoToFile) {
            self.open_finder(FinderMode::File);
        }
        if self.keymap.pressed(ctx, A::GoToSymbol) {
            self.open_finder(FinderMode::Symbol);
        }
        if self.keymap.pressed(ctx, A::GoToLine) && self.ws.active.is_some() {
            self.open_finder(FinderMode::Line);
        }
        if self.keymap.pressed(ctx, A::RecentFiles) {
            self.open_finder(FinderMode::Recent);
        }
        if self.keymap.pressed(ctx, A::GoToDeclaration) {
            self.goto_declaration_at_caret();
        }
        if self.keymap.pressed(ctx, A::Back) {
            self.nav_back();
        }
        if self.keymap.pressed(ctx, A::Forward) {
            self.nav_forward();
        }
        if self.keymap.pressed(ctx, A::ToggleProject) {
            self.tree_open = !self.tree_open;
        }
        if self.keymap.pressed(ctx, A::ToggleStructure) {
            self.structure_open = !self.structure_open;
        }
        if self.keymap.pressed(ctx, A::ToggleCommits) {
            self.toggle_log(ctx);
        }
        if self.keymap.pressed(ctx, A::CloseTab) {
            self.close_active_tab();
        }
        if self.keymap.pressed(ctx, A::PrevTab) {
            self.cycle_tab(-1);
        }
        if self.keymap.pressed(ctx, A::NextTab) {
            self.cycle_tab(1);
        }
        if self.keymap.pressed(ctx, A::ZoomIn) {
            self.zoom(1.0);
        }
        if self.keymap.pressed(ctx, A::ZoomOut) {
            self.zoom(-1.0);
        }
    }

    fn open_finder(&mut self, mode: FinderMode) {
        if self.ws.project_root.is_none() {
            return;
        }
        self.ws.finder_open = true;
        self.ws.finder_mode = mode;
        self.ws.finder_query.clear();
        self.ws.finder_sel = 0;
        self.ws.finder_focus = true;
    }

    fn close_active_tab(&mut self) {
        if let Some(i) = self.ws.active {
            self.ws.tabs.remove(i);
            self.ws.active = if self.ws.tabs.is_empty() {
                None
            } else {
                Some(i.min(self.ws.tabs.len() - 1))
            };
            self.refresh_search();
        }
    }

    fn cycle_tab(&mut self, dir: isize) {
        let n = self.ws.tabs.len();
        if n < 2 {
            return;
        }
        if let Some(a) = self.ws.active {
            let next = (a as isize + dir).rem_euclid(n as isize) as usize;
            self.record_origin();
            self.ws.active = Some(next);
            self.refresh_search();
        }
    }

    fn zoom(&mut self, delta: f32) {
        self.font_size = (self.font_size + delta).clamp(8.0, 40.0);
        self.rehighlight_open_tabs();
    }

    /// ⌘B — go to the definition of the identifier under the caret.
    fn goto_declaration_at_caret(&mut self) {
        let word = self.ws.active.and_then(|i| self.ws.tabs.get(i)).and_then(|t| {
            t.caret_ci.and_then(|ci| word_at(&t.content, ci))
        });
        if let Some(w) = word {
            self.goto_definition(&w);
        }
    }

    /// "Go to …" finder popup (file / symbol / line / recent).
    fn finder_window(&mut self, ctx: &egui::Context) {
        if !self.ws.finder_open {
            return;
        }
        let (up, down, enter) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Enter),
            )
        });

        // Collect matches: (label, dim detail, target path, target line).
        let query = self.ws.finder_query.trim().to_string();
        let ql = query.to_lowercase();
        let root = self.ws.project_root.clone().unwrap_or_default();
        let mut results: Vec<(String, String, Option<PathBuf>, Option<usize>)> = Vec::new();
        const CAP: usize = 100;
        match self.ws.finder_mode {
            FinderMode::File | FinderMode::Recent => {
                let list: Vec<&PathBuf> = match self.ws.finder_mode {
                    FinderMode::Recent => self.ws.recent.iter().collect(),
                    _ => self.ws.files.iter().collect(),
                };
                // Name matches first, then path-only matches.
                let mut by_path: Vec<&PathBuf> = Vec::new();
                for p in list {
                    if results.len() >= CAP {
                        break;
                    }
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy();
                    if ql.is_empty() || name.to_lowercase().contains(&ql) {
                        results.push((name.to_string(), rel.to_string(), Some((*p).clone()), None));
                    } else if rel.to_lowercase().contains(&ql) {
                        by_path.push(p);
                    }
                }
                for p in by_path {
                    if results.len() >= CAP {
                        break;
                    }
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy();
                    results.push((name.to_string(), rel.to_string(), Some(p.clone()), None));
                }
            }
            FinderMode::Symbol => {
                if !ql.is_empty() {
                    if let Some(idx) = &self.ws.symbol_index {
                        let mut names: Vec<&String> =
                            idx.keys().filter(|n| n.to_lowercase().contains(&ql)).collect();
                        // Shorter names (closer matches) first.
                        names.sort_by_key(|n| (n.len(), (*n).clone()));
                        'outer: for name in names {
                            for loc in &idx[name] {
                                let rel = loc.path.strip_prefix(&root).unwrap_or(&loc.path);
                                results.push((
                                    name.clone(),
                                    format!("{}:{}", rel.to_string_lossy(), loc.line + 1),
                                    Some(loc.path.clone()),
                                    Some(loc.line),
                                ));
                                if results.len() >= CAP {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
            FinderMode::Line => {
                if let Some(t) = self.ws.active.and_then(|i| self.ws.tabs.get(i)) {
                    if let Ok(n) = query.parse::<usize>() {
                        let line = n.clamp(1, t.line_count);
                        results.push((
                            format!("{line}번째 줄로 이동"),
                            t.path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                            Some(t.path.clone()),
                            Some(line - 1),
                        ));
                    }
                }
            }
        }

        let n = results.len();
        if n > 0 {
            if down {
                self.ws.finder_sel = (self.ws.finder_sel + 1).min(n - 1);
            }
            if up {
                self.ws.finder_sel = self.ws.finder_sel.saturating_sub(1);
            }
        }
        self.ws.finder_sel = self.ws.finder_sel.min(n.saturating_sub(1));
        let mut navigate: Option<(PathBuf, Option<usize>)> = None;
        if enter {
            if let Some((_, _, Some(p), line)) = results.get(self.ws.finder_sel) {
                navigate = Some((p.clone(), *line));
            }
        }

        let title = self.ws.finder_mode.title();
        let hint = self.ws.finder_mode.hint();
        let mut keep_open = true;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .fixed_size([620.0, 420.0])
            .anchor(Align2::CENTER_TOP, [0.0, 90.0])
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(C_PANEL)
                    .stroke(Stroke::new(1.0, C_BORDER))
                    .inner_margin(10.0),
            )
            .open(&mut keep_open)
            .show(ctx, |ui| {
                let fresp = ui.add(
                    egui::TextEdit::singleline(&mut self.ws.finder_query)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(14.0)),
                );
                if self.ws.finder_focus {
                    fresp.request_focus();
                    self.ws.finder_focus = false;
                }
                if fresp.changed() {
                    self.ws.finder_sel = 0;
                }
                ui.add_space(6.0);
                ui.separator();
                let sel = self.ws.finder_sel;
                egui::ScrollArea::vertical()
                    .id_salt(("finder_list", self.ws.seq))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for (idx, (label, detail, path, line)) in results.iter().enumerate() {
                            let (rr, rresp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 26.0),
                                Sense::click(),
                            );
                            if !ui.is_rect_visible(rr) {
                                if rresp.clicked() {
                                    navigate = path.clone().map(|p| (p, *line));
                                }
                                continue;
                            }
                            let p = ui.painter();
                            if idx == sel {
                                p.rect_filled(rr, 0.0, C_SEL);
                            } else if rresp.hovered() {
                                p.rect_filled(rr, 0.0, C_HOVER);
                            }
                            if let Some(fp) = path {
                                let badge = Rect::from_center_size(
                                    egui::pos2(rr.left() + 14.0, rr.center().y),
                                    Vec2::splat(15.0),
                                );
                                self.icons.file(
                                    p,
                                    badge,
                                    &fp.file_name().unwrap_or_default().to_string_lossy(),
                                );
                            }
                            p.text(
                                egui::pos2(rr.left() + 28.0, rr.center().y),
                                Align2::LEFT_CENTER,
                                label,
                                FontId::proportional(13.5),
                                if idx == sel { Color32::WHITE } else { C_TEXT },
                            );
                            p.text(
                                egui::pos2(rr.right() - 8.0, rr.center().y),
                                Align2::RIGHT_CENTER,
                                detail,
                                FontId::proportional(11.5),
                                C_TEXT_DIM,
                            );
                            if rresp.clicked() {
                                navigate = path.clone().map(|p| (p, *line));
                            }
                            if (up || down) && idx == sel {
                                ui.scroll_to_rect(rr, Some(Align::Center));
                            }
                        }
                        if results.is_empty() && !ql.is_empty() {
                            ui.add_space(16.0);
                            ui.vertical_centered(|ui| {
                                ui.label(egui::RichText::new("결과 없음").color(C_TEXT_DIM));
                            });
                        }
                    });
            });

        if let Some((path, line)) = navigate {
            self.ws.finder_open = false;
            match line {
                Some(l) => self.navigate_to(path, l),
                None => {
                    self.record_origin();
                    self.open_file(path.clone());
                    self.record_nav(path, 0);
                }
            }
        } else {
            self.ws.finder_open = keep_open && self.ws.finder_open;
        }
    }

    /// Settings window: the editable keymap.
    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            self.rebind = None;
            return;
        }
        // While rebinding, the next non-modifier key press becomes the chord.
        if let Some(action) = self.rebind {
            let captured = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                })
            });
            if let Some((key, mods)) = captured {
                if key == Key::Escape {
                    self.rebind = None;
                } else {
                    self.keymap.set(action, crate::keymap::Chord::new(mods, key));
                    self.rebind = None;
                }
            }
        }

        let mut keep_open = true;
        let mut reset_all = false;
        egui::Window::new("설정 · 단축키")
            .collapsible(false)
            .resizable(false)
            .fixed_size([460.0, 560.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(C_PANEL)
                    .stroke(Stroke::new(1.0, C_BORDER))
                    .inner_margin(12.0),
            )
            .open(&mut keep_open)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new("단축키를 클릭한 뒤 새 키 조합을 누르세요 (Esc 취소)")
                        .color(C_TEXT_DIM)
                        .size(12.0),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .id_salt("keymap_list")
                    .max_height(440.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for &action in crate::keymap::ACTIONS {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [250.0, 24.0],
                                    egui::Label::new(
                                        egui::RichText::new(action.label()).size(13.0),
                                    ),
                                );
                                let rebinding = self.rebind == Some(action);
                                let chord = self.keymap.get(action);
                                let dup = !self.keymap.conflicts(action, chord).is_empty();
                                let text = if rebinding {
                                    egui::RichText::new("키 입력 대기…").color(C_ACCENT)
                                } else if dup {
                                    egui::RichText::new(chord.text())
                                        .color(Color32::from_rgb(0xe0, 0x6c, 0x75))
                                } else {
                                    egui::RichText::new(chord.text()).color(C_TEXT)
                                };
                                let b = ui.add_sized([120.0, 24.0], egui::Button::new(text));
                                let b = if dup {
                                    b.on_hover_text("다른 동작과 겹칩니다")
                                } else {
                                    b
                                };
                                if b.clicked() {
                                    self.rebind = Some(action);
                                }
                            });
                        }
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("기본값 복원").clicked() {
                        reset_all = true;
                    }
                });
            });
        if reset_all {
            self.keymap = crate::keymap::Keymap::default_map();
            self.rebind = None;
        }
        self.settings_open = keep_open;
    }

    // ---- Git operations (checkout / pull / fetch via the git CLI) ----------

    /// Run a git subcommand on a background thread. One at a time per project.
    fn git_op(&mut self, ctx: &egui::Context, label: &str, args: Vec<String>) {
        if self.ws.git_op.is_some() {
            return;
        }
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::git::run_git(&root, &args);
            let _ = tx.send(result);
            c.request_repaint();
        });
        self.ws.git_op = Some((label.to_string(), rx));
    }

    /// Collect a finished git operation: report one line in the status bar
    /// and reload project state on success.
    fn git_op_poll(&mut self, ctx: &egui::Context) {
        let done = match &self.ws.git_op {
            Some((label, rx)) => rx.try_recv().ok().map(|r| (label.clone(), r)),
            None => None,
        };
        let Some((label, (ok, msg))) = done else {
            return;
        };
        self.ws.git_op = None;
        let line = msg
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(if ok { "완료" } else { "실패" });
        self.ws.status = format!("git {label}: {line}");
        if ok {
            self.reload_after_git(ctx);
        }
    }

    /// Refresh everything that may have changed on disk after a checkout /
    /// pull: tree (expansion preserved), open tabs, git state, commit log,
    /// symbol index. The search snapshot revalidates itself by mtime.
    fn reload_after_git(&mut self, ctx: &egui::Context) {
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };

        // Project tree, keeping currently expanded folders open.
        let mut expanded = std::collections::HashSet::new();
        if let Some(t) = &self.ws.tree {
            collect_expanded(t, &mut expanded);
        }
        let mut new_root = TreeNode::new(root.clone(), true);
        new_root.expanded = true;
        new_root.children = Some(load_children(&root));
        apply_expanded(&mut new_root, &expanded);
        self.ws.tree = Some(new_root);

        // Open tabs: drop deleted files, rebuild changed ones in place.
        let active_path = self.ws.active.and_then(|i| self.ws.tabs.get(i)).map(|t| t.path.clone());
        let mut rebuilt = Vec::with_capacity(self.ws.tabs.len());
        for tab in self.ws.tabs.drain(..) {
            match std::fs::read_to_string(&tab.path) {
                Ok(content) if content != tab.content => {
                    let job = self.highlighter.highlight(
                        tab.path.to_str().unwrap_or(""),
                        &content,
                        self.font_size,
                    );
                    let lang = ast::Lang::from_path(&tab.path);
                    let outline = lang
                        .map(|l| ast::document_symbols(l, &content))
                        .unwrap_or_default();
                    let mut t = Tab::new(tab.path, content, job, self.font_size, lang, outline);
                    t.git_changes =
                        crate::git::file_line_changes(&root, &t.path).unwrap_or_default();
                    rebuilt.push(t);
                }
                Ok(_) => {
                    let mut t = tab;
                    t.git_changes =
                        crate::git::file_line_changes(&root, &t.path).unwrap_or_default();
                    rebuilt.push(t);
                }
                Err(_) => {} // file no longer exists on this branch
            }
        }
        self.ws.tabs = rebuilt;
        self.ws.active = active_path
            .and_then(|p| self.ws.tabs.iter().position(|t| t.path == p))
            .or(if self.ws.tabs.is_empty() { None } else { Some(0) });
        self.refresh_search();

        // Git status + branch (background), commit log, symbol index.
        let (gtx, grx) = std::sync::mpsc::channel();
        let (c, p) = (ctx.clone(), root.clone());
        std::thread::spawn(move || {
            let (ahead, behind) = crate::git::ahead_behind(&p).unwrap_or((0, 0));
            let _ = gtx.send((
                crate::git::status_map(&p),
                crate::git::current_branch(&p),
                ahead,
                behind,
            ));
            c.request_repaint();
        });
        self.ws.git_rx = Some(grx);

        self.ws.commits.clear();
        self.ws.commit_files.clear();
        self.ws.commit_files_for.clear();
        self.ws.commit_sel = 0;
        self.ws.diff_view = None;
        if self.ws.log_open {
            let (ltx, lrx) = std::sync::mpsc::channel();
            let (c, p) = (ctx.clone(), root.clone());
            std::thread::spawn(move || {
                let _ = ltx.send(crate::git::commit_log(&p, 300));
                c.request_repaint();
            });
            self.ws.commits_rx = Some(lrx);
        }

        let (itx, irx) = std::sync::mpsc::channel();
        let (c, p) = (ctx.clone(), root);
        std::thread::spawn(move || {
            let _ = itx.send(symbols::build_index(&p));
            c.request_repaint();
        });
        self.ws.index_rx = Some(irx);
        self.ws.indexing = true;
    }

    /// "Select Opened File": expand every ancestor of the active tab's file
    /// and scroll the project tree to it.
    fn reveal_active_file(&mut self) {
        let Some(path) = self
            .ws
            .active
            .and_then(|i| self.ws.tabs.get(i))
            .map(|t| t.path.clone())
        else {
            return;
        };
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let mut ancestors = std::collections::HashSet::new();
        let mut p = path.parent();
        while let Some(d) = p {
            ancestors.insert(d.to_path_buf());
            if d == root {
                break;
            }
            p = d.parent();
        }
        if let Some(tree) = &mut self.ws.tree {
            apply_expanded(tree, &ancestors);
        }
        self.left_view = LeftView::Project;
        self.tree_open = true;
        self.ws.reveal_path = Some(path);
    }

    /// Left tool-window header with the branch control on the right:
    /// ⑂ branch-name, ↓N when the remote has new commits (pull needed),
    /// ↑M when local is ahead. Click opens the branch switcher.
    fn left_header(&mut self, ui: &mut egui::Ui, title: &str, locate: bool) {
        let full_w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(full_w, 28.0), Sense::hover());
        let p = ui.painter();
        let title_g = p.layout_no_wrap(
            title.to_string(),
            FontId::proportional(11.5),
            C_HEADER,
        );
        p.galley(
            egui::pos2(rect.left() + 12.0, rect.center().y - title_g.size().y / 2.0),
            title_g.clone(),
            C_HEADER,
        );
        // "Select Opened File" — crosshair right after the title.
        if locate && self.ws.active.is_some() {
            let center = egui::pos2(
                rect.left() + 12.0 + title_g.size().x + 16.0,
                rect.center().y,
            );
            let r = Rect::from_center_size(center, Vec2::splat(18.0));
            let resp = ui.interact(r, ui.id().with("locate_btn"), Sense::click());
            let c = if resp.hovered() { C_TEXT } else { C_TEXT_DIM };
            if resp.hovered() {
                p.rect_filled(r, 4.0, C_HOVER);
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            p.circle_stroke(center, 4.5, Stroke::new(1.3, c));
            p.circle_filled(center, 1.4, c);
            for (dx, dy) in [(0.0, -1.0), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)] {
                let v = Vec2::new(dx, dy);
                p.line_segment(
                    [center + v * 4.5, center + v * 7.0],
                    Stroke::new(1.3, c),
                );
            }
            if resp.on_hover_text("열린 파일 위치 찾기").clicked() {
                self.reveal_active_file();
            }
        }
        p.line_segment(
            [
                egui::pos2(rect.left(), rect.bottom() - 0.5),
                egui::pos2(rect.right(), rect.bottom() - 0.5),
            ],
            Stroke::new(1.0, C_BORDER),
        );

        let Some(branch) = self.ws.git_branch.clone() else {
            return;
        };
        let font = FontId::proportional(12.0);
        let cy = rect.center().y;

        // Compose the chip text, truncating the branch name to fit the panel.
        let op_running = self.ws.git_op.is_some();
        let mut suffix = String::new();
        if self.ws.git_behind > 0 {
            suffix.push_str(&format!("  ↓{}", self.ws.git_behind));
        }
        if self.ws.git_ahead > 0 {
            suffix.push_str(&format!("  ↑{}", self.ws.git_ahead));
        }
        let max_w = full_w - 90.0; // leave room for the PROJECT title
        let mut name = branch.clone();
        let text_of = |n: &str, s: &str| format!("{n}{s}");
        let width = |t: &str, ui: &egui::Ui| {
            ui.fonts(|f| f.layout_no_wrap(t.to_string(), font.clone(), C_TEXT).size().x)
        };
        while name.chars().count() > 8 && width(&text_of(&name, &suffix), ui) + 22.0 > max_w {
            let mut cs: Vec<char> = name.chars().collect();
            cs.truncate(cs.len().saturating_sub(2));
            name = cs.into_iter().collect::<String>() + "…";
        }
        let label = text_of(&name, &suffix);
        let text_w = width(&label, ui);

        let chip = Rect::from_min_max(
            egui::pos2(rect.right() - text_w - 26.0, rect.top() + 3.0),
            egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
        );
        let resp = ui.interact(chip, ui.id().with("branch_chip"), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            p.rect_filled(chip, 4.0, C_HOVER);
        }
        let col = if resp.hovered() { C_TEXT } else { C_TEXT_DIM };
        // Accent-colored icon doubles as the "operation running" indicator.
        draw_branch_icon(
            p,
            egui::pos2(chip.left() + 8.0, cy),
            if op_running { C_ACCENT } else { col },
        );
        // Name in the base color, ↓/↑ counters accented.
        let mut x = chip.left() + 16.0;
        let g = p.layout_no_wrap(name, font.clone(), col);
        p.galley(egui::pos2(x, cy - g.size().y / 2.0), g.clone(), col);
        x += g.size().x;
        if self.ws.git_behind > 0 {
            let g = p.layout_no_wrap(format!("  ↓{}", self.ws.git_behind), font.clone(), C_ACCENT);
            p.galley(egui::pos2(x, cy - g.size().y / 2.0), g.clone(), C_ACCENT);
            x += g.size().x;
        }
        if self.ws.git_ahead > 0 {
            let green = Color32::from_rgb(0x62, 0xb5, 0x43);
            let g = p.layout_no_wrap(format!("  ↑{}", self.ws.git_ahead), font.clone(), green);
            p.galley(egui::pos2(x, cy - g.size().y / 2.0), g, green);
        }
        let resp = if self.ws.git_behind > 0 {
            resp.on_hover_text(format!(
                "원격에 새 커밋 {}개 — Pull 필요",
                self.ws.git_behind
            ))
        } else {
            resp.on_hover_text("브랜치 전환 / Pull / Fetch")
        };
        if resp.clicked() {
            self.ws.branch_menu_open = true;
            self.ws.branch_menu_pos = egui::pos2(chip.left().min(rect.right() - 348.0), rect.bottom() + 4.0);
            self.ws.branch_focus = true;
            self.ws.branch_filter.clear();
            if let Some(root) = &self.ws.project_root {
                self.ws.branches = crate::git::branches(root);
            }
        }
    }

    /// Branch switcher popup (anchored under the PROJECT-header branch chip).
    fn branch_menu(&mut self, ctx: &egui::Context) {
        if !self.ws.branch_menu_open {
            return;
        }
        let op_running = self.ws.git_op.is_some();
        let mut checkout: Option<String> = None;
        let mut do_pull = false;
        let mut do_fetch = false;
        let mut keep_open = true;

        let resp = egui::Window::new("branch_menu")
            .title_bar(false)
            .resizable(false)
            .fixed_size([340.0, 420.0])
            .fixed_pos(self.ws.branch_menu_pos)
            .pivot(Align2::LEFT_TOP)
            .frame(
                egui::Frame::window(&ctx.style())
                    .fill(C_PANEL)
                    .stroke(Stroke::new(1.0, C_BORDER))
                    .inner_margin(8.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!op_running, egui::Button::new("Pull ↓"))
                        .on_hover_text("git pull --ff-only")
                        .clicked()
                    {
                        do_pull = true;
                    }
                    if ui
                        .add_enabled(!op_running, egui::Button::new("Fetch"))
                        .on_hover_text("git fetch --prune")
                        .clicked()
                    {
                        do_fetch = true;
                    }
                    if op_running {
                        ui.label(egui::RichText::new("작업 중…").color(C_TEXT_DIM).size(12.0));
                    }
                });
                ui.add_space(6.0);
                let fresp = ui.add(
                    egui::TextEdit::singleline(&mut self.ws.branch_filter)
                        .hint_text("브랜치 검색 (클릭 = 체크아웃)")
                        .desired_width(f32::INFINITY),
                );
                if self.ws.branch_focus {
                    fresp.request_focus();
                    self.ws.branch_focus = false;
                }
                ui.add_space(4.0);
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt(("branch_list", self.ws.seq))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let filt = self.ws.branch_filter.to_lowercase();
                        for b in &self.ws.branches {
                            if !filt.is_empty() && !b.name.to_lowercase().contains(&filt) {
                                continue;
                            }
                            let (rr, rresp) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 24.0),
                                Sense::click(),
                            );
                            let p = ui.painter();
                            if b.is_current {
                                p.rect_filled(rr, 0.0, C_SEL);
                            } else if rresp.hovered() {
                                p.rect_filled(rr, 0.0, C_HOVER);
                            }
                            draw_branch_icon(
                                p,
                                egui::pos2(rr.left() + 12.0, rr.center().y),
                                if b.is_current { C_ACCENT } else { C_TEXT_DIM },
                            );
                            p.text(
                                egui::pos2(rr.left() + 24.0, rr.center().y),
                                Align2::LEFT_CENTER,
                                &b.name,
                                FontId::proportional(13.0),
                                if b.is_current {
                                    Color32::WHITE
                                } else if b.is_remote {
                                    C_TEXT_DIM
                                } else {
                                    C_TEXT
                                },
                            );
                            if b.is_remote {
                                p.text(
                                    egui::pos2(rr.right() - 8.0, rr.center().y),
                                    Align2::RIGHT_CENTER,
                                    "remote",
                                    FontId::proportional(11.0),
                                    C_TEXT_DIM,
                                );
                            }
                            if rresp.clicked() && !b.is_current && !op_running {
                                checkout = Some(b.name.clone());
                            }
                        }
                    });
            });

        if let Some(r) = &resp {
            if r.response.clicked_elsewhere() {
                keep_open = false;
            }
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            keep_open = false;
        }
        if let Some(name) = checkout {
            keep_open = false;
            self.git_op(ctx, "checkout", vec!["checkout".into(), name]);
        }
        if do_pull {
            self.git_op(ctx, "pull", vec!["pull".into(), "--ff-only".into()]);
        }
        if do_fetch {
            self.git_op(ctx, "fetch", vec!["fetch".into(), "--prune".into()]);
        }
        self.ws.branch_menu_open = keep_open;
    }

    // ---- Commit log / diff -------------------------------------------------

    fn toggle_log(&mut self, ctx: &egui::Context) {
        self.ws.log_open = !self.ws.log_open;
        if self.ws.log_open && self.ws.commits.is_empty() && self.ws.commits_rx.is_none() {
            if let Some(root) = self.ws.project_root.clone() {
                let (tx, rx) = std::sync::mpsc::channel();
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let log = crate::git::commit_log(&root, 300);
                    let _ = tx.send(log);
                    c.request_repaint();
                });
                self.ws.commits_rx = Some(rx);
            }
        }
    }

    /// Select a commit and load its changed-file list (synchronously).
    fn select_commit(&mut self, idx: usize) {
        self.ws.local_sel = false;
        self.ws.commit_sel = idx;
        if let (Some(root), Some(c)) = (self.ws.project_root.clone(), self.ws.commits.get(idx).cloned()) {
            self.ws.commit_files = crate::git::commit_files(&root, &c.id);
            self.ws.commit_files_for = c.id;
        }
    }

    /// Open the unified diff of `file` in the selected commit.
    fn open_commit_diff(&mut self, file: String) {
        if let (Some(root), Some(c)) =
            (self.ws.project_root.clone(), self.ws.commits.get(self.ws.commit_sel).cloned())
        {
            let lines = crate::git::commit_file_diff(&root, &c.id, &file);
            let (text, job) = build_diff_doc(&self.highlighter, &file, &lines, self.font_size);
            self.ws.diff_view = Some(DiffView {
                commit_short: c.short.clone(),
                file,
                lines,
                edit_buf: text.clone(),
                text,
                job,
                jobs_font: self.font_size,
            });
        }
    }

    /// Open the diff of one file's uncommitted changes (HEAD vs working tree).
    fn open_working_diff(&mut self, file: String) {
        if let Some(root) = self.ws.project_root.clone() {
            let lines = crate::git::working_file_diff(&root, &file);
            let (text, job) = build_diff_doc(&self.highlighter, &file, &lines, self.font_size);
            self.ws.diff_view = Some(DiffView {
                commit_short: "변경사항".into(),
                file,
                lines,
                edit_buf: text.clone(),
                text,
                job,
                jobs_font: self.font_size,
            });
        }
    }

    /// Refresh the working-tree status (and branch / ahead-behind) in the
    /// background — used when reviewing uncommitted changes.
    fn refresh_git_status(&mut self, ctx: &egui::Context) {
        if self.ws.git_rx.is_some() {
            return;
        }
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        self.ws.local_refresh_at = Some(ctx.input(|i| i.time));
        let (gtx, grx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let (ahead, behind) = crate::git::ahead_behind(&root).unwrap_or((0, 0));
            let _ = gtx.send((
                crate::git::status_map(&root),
                crate::git::current_branch(&root),
                ahead,
                behind,
            ));
            c.request_repaint();
        });
        self.ws.git_rx = Some(grx);
    }

    /// Open a file at a line, recording it in the nav history.
    fn navigate_to(&mut self, path: PathBuf, line: usize) {
        self.record_origin();
        self.open_file(path.clone());
        self.set_jump_target(line);
        self.record_nav(path, line);
    }

    /// Scroll/flash the active tab to a jumped-to line and move the tracked
    /// caret there, so the history sees the destination as "current".
    fn set_jump_target(&mut self, line: usize) {
        if let Some(i) = self.ws.active {
            let t = &mut self.ws.tabs[i];
            t.scroll_to_line = Some(line);
            t.flash_line = Some(line);
            t.caret_line = Some(line);
            t.caret_ci = None;
        }
    }

    fn goto_definition(&mut self, name: &str) {
        let cur_path = self.ws.active.and_then(|i| self.ws.tabs.get(i)).map(|t| t.path.clone());
        let cur_line = self
            .ws
            .active
            .and_then(|i| self.ws.tabs.get(i))
            .and_then(|t| t.flash_line);

        let matches: Vec<_> = self
            .ws
            .symbol_index
            .as_ref()
            .and_then(|idx| idx.get(name))
            .cloned()
            .unwrap_or_default();

        if matches.is_empty() {
            self.ws.status = if self.ws.indexing {
                format!("⌘+클릭 ‘{name}’ — 인덱싱 중, 잠시 후 다시 시도")
            } else {
                format!("⌘+클릭 ‘{name}’ — 정의를 찾지 못함")
            };
            return;
        }

        // Prefer, in order: a definition in the current file (but not the line
        // we're already on) → one in the same language as the current file →
        // the first match. Keeps ⌘+Click in .kt from landing in unrelated
        // languages when names collide.
        let cur_lang = cur_path.as_deref().and_then(ast::Lang::from_path);
        let cur_ext = cur_path
            .as_deref()
            .and_then(|p| p.extension())
            .map(|e| e.to_ascii_lowercase());
        let same_lang = |l: &&crate::symbols::SymbolLoc| match (cur_lang, ast::Lang::from_path(&l.path)) {
            (Some(a), Some(b)) => a == b,
            _ => l.path.extension().map(|e| e.to_ascii_lowercase()) == cur_ext,
        };
        let pick = matches
            .iter()
            .find(|l| Some(&l.path) == cur_path.as_ref() && Some(l.line) != cur_line)
            .or_else(|| matches.iter().find(same_lang))
            .or_else(|| matches.first())
            .cloned()
            .unwrap();

        self.record_origin();
        self.open_file(pick.path.clone());
        self.set_jump_target(pick.line);
        self.record_nav(pick.path.clone(), pick.line);

        let where_ = pick
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.ws.status = if matches.len() > 1 {
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
        for tab in &mut self.ws.tabs {
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
        self.ws.search_matches.clear();
        self.ws.search_cur = 0;
        let q = self.ws.search_query.to_lowercase();
        if q.is_empty() {
            return;
        }
        if let Some(i) = self.ws.active {
            for (n, line) in self.ws.tabs[i].content.lines().enumerate() {
                if line.to_lowercase().contains(&q) {
                    self.ws.search_matches.push(n);
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
        if self.ws.search_matches.is_empty() {
            return;
        }
        let len = self.ws.search_matches.len();
        self.ws.search_cur = if forward {
            (self.ws.search_cur + 1) % len
        } else {
            (self.ws.search_cur + len - 1) % len
        };
        let line = self.ws.search_matches[self.ws.search_cur];
        if let Some(i) = self.ws.active {
            self.ws.tabs[i].scroll_to_line = Some(line);
            self.ws.tabs[i].flash_line = Some(line);
        }
    }
}

impl eframe::App for CodeLookApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(p) = &self.ws.project_root {
            storage.set_string("last_project", p.to_string_lossy().to_string());
        }
        // All open projects (in open order) so the whole set restores.
        let mut all: Vec<(usize, String)> = self
            .parked
            .iter()
            .chain(std::iter::once(&self.ws))
            .filter_map(|w| {
                w.project_root
                    .as_ref()
                    .map(|p| (w.seq, p.to_string_lossy().to_string()))
            })
            .collect();
        all.sort_by_key(|(seq, _)| *seq);
        storage.set_string(
            "open_projects",
            all.into_iter().map(|(_, p)| p).collect::<Vec<_>>().join("\n"),
        );
        storage.set_string("keymap", self.keymap.serialize());
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.shot.is_some() {
            self.drive_capture(ctx);
        }

        if let Some(rx) = &self.ws.index_rx {
            if let Ok((idx, files)) = rx.try_recv() {
                let count = idx.len();
                self.ws.symbol_index = Some(idx);
                self.ws.files = files;
                self.ws.indexing = false;
                self.ws.index_rx = None;
                self.ws.status = format!("인덱싱 완료 · 심볼 {count}개");
            }
        }

        if let Some(rx) = &self.ws.git_rx {
            if let Ok((status, branch, ahead, behind)) = rx.try_recv() {
                self.ws.git_status = status;
                self.ws.git_branch = branch;
                self.ws.git_ahead = ahead;
                self.ws.git_behind = behind;
                self.ws.git_rx = None;
            }
        }

        // Quiet background fetch keeps the ↓ badge honest (per active project,
        // at most every 5 minutes, never while a git op runs).
        if let Some(rx) = &self.ws.autofetch_rx {
            if let Ok((ahead, behind)) = rx.try_recv() {
                self.ws.git_ahead = ahead;
                self.ws.git_behind = behind;
                self.ws.autofetch_rx = None;
            }
        } else if self.ws.git_branch.is_some()
            && self.ws.git_op.is_none()
            && self.shot.is_none()
            && {
                let now = ctx.input(|i| i.time);
                self.ws.autofetch_at.is_none_or(|t| now - t > 300.0)
            }
        {
            if let Some(root) = self.ws.project_root.clone() {
                self.ws.autofetch_at = Some(ctx.input(|i| i.time));
                let (tx, rx) = std::sync::mpsc::channel();
                let c = ctx.clone();
                std::thread::spawn(move || {
                    let _ = crate::git::run_git(
                        &root,
                        &["fetch".to_string(), "--prune".to_string(), "-q".to_string()],
                    );
                    let ab = crate::git::ahead_behind(&root).unwrap_or((0, 0));
                    let _ = tx.send(ab);
                    c.request_repaint();
                });
                self.ws.autofetch_rx = Some(rx);
            }
        }

        if let Some(rx) = &self.ws.commits_rx {
            if let Ok(list) = rx.try_recv() {
                self.ws.commits = list;
                self.ws.commits_rx = None;
                if !self.ws.commits.is_empty() {
                    self.select_commit(0);
                }
                // With uncommitted changes present, start on the review view.
                if !self.ws.git_status.is_empty() {
                    self.ws.local_sel = true;
                }
            }
        }

        // While reviewing uncommitted changes (bottom-panel row or the left
        // Commit view), keep the file list fresh — files may be edited
        // outside, e.g. by an agent in a terminal.
        let reviewing = (self.ws.log_open && self.ws.local_sel)
            || (self.tree_open && self.left_view == LeftView::Commit);
        if reviewing && self.shot.is_none() {
            let now = ctx.input(|i| i.time);
            if self.ws.local_refresh_at.is_none_or(|t| now - t > 5.0) {
                self.refresh_git_status(ctx);
            }
        }

        // Double-Shift → Search Everywhere (파일로 이동), IntelliJ style.
        {
            let (shift_down, only_shift, other_key, t) = ctx.input(|i| {
                (
                    i.modifiers.shift,
                    i.modifiers == egui::Modifiers::SHIFT,
                    i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Key { pressed: true, .. })),
                    i.time,
                )
            });
            if other_key {
                self.last_shift_at = f64::NEG_INFINITY; // real key between shifts cancels
            }
            if shift_down && !self.shift_prev && only_shift && !self.settings_open {
                if t - self.last_shift_at < 0.4 {
                    self.last_shift_at = f64::NEG_INFINITY;
                    self.open_finder(FinderMode::File);
                } else {
                    self.last_shift_at = t;
                }
            }
            self.shift_prev = shift_down;
        }

        // Keyboard: ⌘F open search, Esc closes it.
        // Navigation (IntelliJ macOS keymap): Back = ⌘[ or ⌥⌘← ; Forward = ⌘] or
        // ⌥⌘→ ; plus the mouse Back/Forward side buttons.
        // Hardware / legacy navigation inputs (not part of the keymap).
        let (mouse_back, mouse_fwd, alt_back, alt_fwd, esc) = ctx.input(|i| {
            let cmd_alt = (i.modifiers.command || i.modifiers.ctrl) && i.modifiers.alt;
            (
                i.pointer.button_pressed(egui::PointerButton::Extra1),
                i.pointer.button_pressed(egui::PointerButton::Extra2),
                cmd_alt && i.key_pressed(Key::ArrowLeft),
                cmd_alt && i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Escape),
            )
        });
        // Keymap shortcuts are suspended while the settings window captures keys.
        if !self.settings_open {
            self.handle_shortcuts(ctx);
        }
        if esc {
            self.ws.search_open = false;
            self.ws.gsearch_open = false;
            self.ws.finder_open = false;
        }
        if mouse_back || alt_back {
            self.nav_back();
        }
        if mouse_fwd || alt_fwd {
            self.nav_forward();
        }

        // Search-as-you-type: the debounce timer resets on every keystroke and
        // fires ~120ms after typing stops, so results update live without Enter.
        // (Skipped in capture mode, where results are populated synchronously.)
        self.gsearch_poll();
        self.git_op_poll(ctx);
        let now = ctx.input(|i| i.time);
        if self.shot.is_some() {
            self.ws.gsearch_prev = self.ws.gsearch_query.clone();
        } else if self.ws.gsearch_query != self.ws.gsearch_prev {
            self.ws.gsearch_prev = self.ws.gsearch_query.clone();
            self.ws.gsearch_dirty_at = Some(now);
        }
        if let Some(t0) = self.ws.gsearch_dirty_at {
            if now - t0 > 0.12 {
                self.ws.gsearch_dirty_at = None;
                self.gsearch_kick(ctx);
            } else {
                ctx.request_repaint();
            }
        }

        self.top_bar(ctx);
        self.branch_menu(ctx); // before bottom_bar so click-away logic is stable
        self.bottom_bar(ctx);
        self.git_log_panel(ctx);
        self.side_tree(ctx);
        self.structure_panel(ctx);
        self.central(ctx);
        self.global_search_window(ctx);
        self.finder_window(ctx);
        self.settings_window(ctx);
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

                    // Project tabs: every open project, sorted by open order.
                    let mut projects: Vec<(usize, String, PathBuf, bool)> = Vec::new();
                    if let Some(r) = &self.ws.project_root {
                        let name = r.file_name().unwrap_or_default().to_string_lossy().to_string();
                        projects.push((self.ws.seq, name, r.clone(), true));
                    }
                    for w in &self.parked {
                        if let Some(r) = &w.project_root {
                            let name =
                                r.file_name().unwrap_or_default().to_string_lossy().to_string();
                            projects.push((w.seq, name, r.clone(), false));
                        }
                    }
                    projects.sort_by_key(|p| p.0);
                    let mut switch_to: Option<PathBuf> = None;
                    let mut close_p: Option<PathBuf> = None;
                    for (seq, name, root, is_active) in &projects {
                        if project_chip(ui, *seq, name, *is_active, &mut close_p, root) {
                            switch_to = Some(root.clone());
                        }
                    }
                    if let Some(r) = switch_to {
                        self.switch_project(&r);
                    }
                    if let Some(r) = close_p {
                        self.close_project(&r);
                    }
                    if !projects.is_empty() {
                        ui.add_space(2.0);
                        sep_dot(ui);
                    }

                    if ui.button("프로젝트 열기").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.open_project(ctx, dir);
                        }
                    }
                    if ui
                        .button("검색")
                        .on_hover_text(self.keymap.text(crate::keymap::Action::FindInFile))
                        .clicked()
                        && self.ws.active.is_some()
                    {
                        self.ws.search_open = true;
                        self.ws.search_focus = true;
                    }
                    if ui
                        .button("전체 검색")
                        .on_hover_text(self.keymap.text(crate::keymap::Action::FindInProject))
                        .clicked()
                        && self.ws.project_root.is_some()
                    {
                        self.ws.gsearch_open = true;
                        self.ws.gsearch_focus = true;
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
                    if panel_toggle(ui, PanelSide::Bottom, self.ws.log_open, "커밋 로그") {
                        self.toggle_log(ctx);
                    }
                    sep_dot(ui);

                    ui.add_space(4.0);
                    if ui.small_button("A−").on_hover_text("글자 작게").clicked() {
                        self.zoom(-1.0);
                    }
                    ui.label(
                        egui::RichText::new(format!("{}px", self.font_size as i32))
                            .color(C_TEXT_DIM)
                            .size(12.0),
                    );
                    if ui.small_button("A+").on_hover_text("글자 크게").clicked() {
                        self.zoom(1.0);
                    }
                    ui.add_space(4.0);
                    sep_dot(ui);
                    if ui.small_button("설정").on_hover_text("단축키 설정").clicked() {
                        self.settings_open = true;
                    }

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if self.ws.indexing {
                            ui.spinner();
                        }
                        let hint = if self.ws.status.is_empty() {
                            "⌘+클릭 정의 이동 · ⌘C 복사".to_string()
                        } else {
                            self.ws.status.clone()
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
            .default_width(296.0)
            .width_range(206.0..=640.0)
            .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    self.tool_strip(ui);
                    ui.vertical(|ui| match self.left_view {
                        LeftView::Project => self.project_tree_view(ui),
                        LeftView::Commit => self.commit_sidebar(ui),
                    });
                });
            });
    }

    /// The file tree (left panel, Project view).
    fn project_tree_view(&mut self, ui: &mut egui::Ui) {
        self.left_header(ui, "PROJECT", true);
        let mut reveal = self.ws.reveal_path.take();
        egui::ScrollArea::both()
            .id_salt(("tree_scroll", self.ws.seq))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let active_path = self
                    .ws
                    .active
                    .and_then(|i| self.ws.tabs.get(i))
                    .map(|t| t.path.clone());
                let mut to_open = None;
                let status = &self.ws.git_status;
                let icons = &self.icons;
                if let Some(root) = &mut self.ws.tree {
                    show_node(
                        ui,
                        root,
                        0,
                        active_path.as_deref(),
                        status,
                        icons,
                        &mut to_open,
                        &mut reveal,
                    );
                } else {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("열린 프로젝트가 없습니다").color(C_TEXT_DIM),
                        );
                    });
                }
                if let Some(p) = to_open {
                    self.record_origin();
                    self.open_file(p.clone());
                    self.record_nav(p, 0);
                }
            });
    }

    /// Vertical tool-window strip on the far left (IntelliJ New UI): switch
    /// the left panel between the project tree and the commit (changes) view.
    fn tool_strip(&mut self, ui: &mut egui::Ui) {
        let h = ui.available_height();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, h), Sense::hover());
        let p = ui.painter();
        p.vline(
            rect.right() - 0.5,
            rect.y_range(),
            Stroke::new(1.0, C_BORDER),
        );
        let n_changes = self.ws.git_status.len();
        for (i, (view, tip)) in [
            (LeftView::Project, "프로젝트".to_string()),
            (LeftView::Commit, format!("변경사항 (커밋 전) — {n_changes}개 파일")),
        ]
        .into_iter()
        .enumerate()
        {
            let r = Rect::from_min_size(
                egui::pos2(rect.left() + 5.0, rect.top() + 8.0 + i as f32 * 36.0),
                Vec2::splat(26.0),
            );
            let resp = ui.interact(r, ui.id().with(("tool_strip", i)), Sense::click());
            let active = self.left_view == view;
            if active {
                p.rect_filled(r, 6.0, C_HOVER);
            } else if resp.hovered() {
                p.rect_filled(r, 6.0, C_HOVER.gamma_multiply(0.5));
            }
            match view {
                LeftView::Project => {
                    self.icons.folder(p, r.shrink(4.0), "", false);
                }
                LeftView::Commit => {
                    draw_branch_icon(
                        p,
                        r.center(),
                        if active { C_TEXT } else { C_TEXT_DIM },
                    );
                    // Green dot = uncommitted changes exist.
                    if n_changes > 0 {
                        p.circle_filled(
                            egui::pos2(r.right() - 3.0, r.top() + 3.0),
                            3.0,
                            Color32::from_rgb(0x62, 0xb5, 0x43),
                        );
                    }
                }
            }
            if resp.on_hover_text(tip).clicked() {
                self.left_view = view;
            }
        }
    }

    /// Left-panel Commit view: uncommitted working-tree changes at a glance;
    /// clicking a file opens its HEAD-vs-worktree diff for review.
    fn commit_sidebar(&mut self, ui: &mut egui::Ui) {
        self.left_header(ui, "COMMIT", false);
        let files = self.local_changes();
        let mut open: Option<String> = None;
        egui::ScrollArea::vertical()
            .id_salt(("commit_sidebar", self.ws.seq))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add_space(2.0);
                let (hr, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 22.0),
                    Sense::hover(),
                );
                ui.painter().text(
                    egui::pos2(hr.left() + 10.0, hr.center().y),
                    Align2::LEFT_CENTER,
                    format!("변경사항 {}개 파일", files.len()),
                    FontId::proportional(12.0),
                    C_TEXT_DIM,
                );
                if files.is_empty() {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("커밋 전 변경사항 없음").color(C_TEXT_DIM),
                        );
                    });
                }
                for (path, status) in &files {
                    let (rr, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 22.0),
                        Sense::click(),
                    );
                    let p = ui.painter();
                    let is_open = self
                        .ws
                        .diff_view
                        .as_ref()
                        .map(|d| &d.file == path)
                        .unwrap_or(false);
                    if is_open {
                        p.rect_filled(rr, 0.0, C_SEL);
                    } else if resp.hovered() {
                        p.rect_filled(rr, 0.0, C_HOVER);
                    }
                    p.text(
                        egui::pos2(rr.left() + 10.0, rr.center().y),
                        Align2::LEFT_CENTER,
                        status_letter(*status),
                        FontId::monospace(12.0),
                        status_color(*status),
                    );
                    p.text(
                        egui::pos2(rr.left() + 26.0, rr.center().y),
                        Align2::LEFT_CENTER,
                        path,
                        FontId::proportional(12.5),
                        if is_open { Color32::WHITE } else { C_TEXT },
                    );
                    if resp.on_hover_text(path).clicked() {
                        open = Some(path.clone());
                    }
                }
            });
        if let Some(f) = open {
            self.open_working_diff(f);
        }
    }

    /// Uncommitted changes as sorted (relative path, status) pairs.
    fn local_changes(&self) -> Vec<(String, crate::git::FileStatus)> {
        let root = self.ws.project_root.clone().unwrap_or_default();
        let mut files: Vec<(String, crate::git::FileStatus)> = self
            .ws
            .git_status
            .iter()
            .map(|(p, s)| {
                (
                    p.strip_prefix(&root).unwrap_or(p).to_string_lossy().to_string(),
                    *s,
                )
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    /// IntelliJ-style "Structure" view: AST-derived document symbols of the
    /// active file. Clicking jumps to the definition.
    fn structure_panel(&mut self, ctx: &egui::Context) {
        if !self.structure_open {
            return;
        }
        let i = match self.ws.active {
            Some(i) if i < self.ws.tabs.len() => i,
            _ => return,
        };
        let lang_label = self.ws.tabs[i].lang.map(|l| l.label()).unwrap_or("Plain");
        egui::SidePanel::right("structure_panel")
            .resizable(true)
            .default_width(240.0)
            .width_range(160.0..=460.0)
            .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
            .show(ctx, |ui| {
                tool_window_header(ui, "STRUCTURE", Some(lang_label));

                if self.ws.tabs[i].outline.is_empty() {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("추출된 심볼이 없습니다").color(C_TEXT_DIM),
                        );
                    });
                } else {
                    let mut jump = None;
                    egui::ScrollArea::vertical()
                        .id_salt(("structure_scroll", self.ws.seq))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for s in &self.ws.tabs[i].outline {
                                if symbol_row(ui, s, &self.icons) {
                                    jump = Some(s.line);
                                }
                            }
                        });
                    if let Some(line) = jump {
                        self.record_origin();
                        self.set_jump_target(line);
                        let p = self.ws.tabs[i].path.clone();
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

                let Some(root) = self.ws.project_root.as_ref() else {
                    return;
                };
                let font = FontId::proportional(12.0);
                let cy = rect.center().y;

                // Breadcrumbs (left) — only when a file is open.
                if let Some(tab) = self.ws.active.and_then(|i| self.ws.tabs.get(i)) {
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
                            self.icons.file(p, badge, &file_name);
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

                // Right side: [git-op] · line count · lang. (The branch
                // control lives in the PROJECT header.)
                let mut right = String::new();
                if let Some((label, _)) = &self.ws.git_op {
                    right = format!("git {label} 실행 중…   ·   ");
                }
                if let Some(tab) = self.ws.active.and_then(|i| self.ws.tabs.get(i)) {
                    let lang = tab.lang.map(|l| l.label()).unwrap_or("Text");
                    right.push_str(&format!("{} lines   ·   {}", tab.line_count, lang));
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
        if !self.ws.gsearch_open {
            return;
        }
        let root = self.ws.project_root.clone().unwrap_or_default();

        // Keyboard selection (handled here so the preview can update this frame).
        let n = self.ws.gsearch_results.len();
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
                self.ws.gsearch_sel = (self.ws.gsearch_sel + 1).min(n - 1);
                key_moved = true;
            }
            if up {
                self.ws.gsearch_sel = self.ws.gsearch_sel.saturating_sub(1);
                key_moved = true;
            }
        }
        let mut navigate: Option<(PathBuf, usize)> = None;
        if enter {
            if let Some(h) = self.ws.gsearch_results.get(self.ws.gsearch_sel) {
                navigate = Some((h.path.clone(), h.line));
            }
        }

        // Refresh the preview for the current selection.
        self.ensure_gpreview();

        // Move fields into locals so the Window closure doesn't alias `self`.
        let mut query = std::mem::take(&mut self.ws.gsearch_query);
        let results = std::mem::take(&mut self.ws.gsearch_results);
        let preview_job = std::mem::take(&mut self.ws.gpreview_job);
        let preview_gutter = std::mem::take(&mut self.ws.gpreview_gutter);
        let preview_marks = std::mem::take(&mut self.ws.gpreview_marks);
        let needle = query.clone(); // markers match ASCII-case-insensitively
        let needle_len_char = needle.chars().count();
        let focus_ci = self.ws.gpreview_focus_ci;
        let mut sel = self.ws.gsearch_sel;
        let running = self.ws.gsearch_running;
        let truncated = self.ws.gsearch_truncated;
        let mut want_focus = self.ws.gsearch_focus;
        let preview_line = self.ws.gpreview_line;
        let mut do_scroll = self.ws.gpreview_scroll || key_moved;
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
                self.ws.gsearch_files,
                results.len(),
                if truncated { " (상한 도달)" } else { "" }
            )
        };

        egui::Window::new("전체 검색")
            .collapsible(false)
            .resizable(true)
            .default_size([1000.0, 600.0])
            .min_width(560.0)
            .min_height(340.0)
            .default_pos(egui::pos2(
                ctx.screen_rect().center().x - 500.0,
                ctx.screen_rect().top() + 60.0,
            ))
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
                let list_h = (body_h * 0.42).max(110.0);

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
                                    self.icons.file(p, badge, &name);
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
        self.ws.gsearch_query = query;
        self.ws.gsearch_results = results;
        self.ws.gpreview_job = preview_job;
        self.ws.gpreview_gutter = preview_gutter;
        self.ws.gpreview_marks = preview_marks;
        self.ws.gsearch_sel = sel;
        self.ws.gsearch_focus = want_focus;
        self.ws.gpreview_scroll = false;

        if let Some((p, l)) = navigate {
            self.navigate_to(p, l);
            self.ws.gsearch_open = false;
        }
        if !keep_open {
            self.ws.gsearch_open = false;
        }
    }

    /// Bottom Git-Log tool window: commits (left) + changed files (right).
    fn git_log_panel(&mut self, ctx: &egui::Context) {
        if !self.ws.log_open {
            return;
        }
        let loading = self.ws.commits_rx.is_some();
        let mut sel_commit: Option<usize> = None;
        let mut sel_local = false;
        let mut open_file: Option<String> = None;
        let mut open_wfile: Option<String> = None;
        let mut close = false;
        // Working-tree changes (uncommitted), as relative paths.
        let local_files = self.local_changes();

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
                    format!("{}개", self.ws.commits.len())
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
                let total_w = ui.available_width();
                // Draggable split between the commit list and the file list.
                let min_side = 240.0_f32.min(total_w * 0.3);
                let left_w = (total_w * self.ws.log_split).clamp(min_side, (total_w - min_side).max(min_side));
                ui.horizontal_top(|ui| {
                    // Commit list.
                    ui.vertical(|ui| {
                        ui.set_min_width(left_w);
                        ui.set_max_width(left_w);
                        egui::ScrollArea::vertical()
                            .id_salt(("commit_list", self.ws.seq))
                            .max_height(body_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;

                                // Pinned row: uncommitted working-tree changes
                                // (커밋 전 검토) — IntelliJ's Local Changes.
                                {
                                    let (rr, resp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 24.0),
                                        Sense::click(),
                                    );
                                    let p = ui.painter();
                                    if self.ws.local_sel {
                                        p.rect_filled(rr, 0.0, C_SEL);
                                    } else if resp.hovered() {
                                        p.rect_filled(rr, 0.0, C_HOVER);
                                    }
                                    let dot = if local_files.is_empty() {
                                        C_TEXT_DIM
                                    } else {
                                        Color32::from_rgb(0x62, 0xb5, 0x43)
                                    };
                                    p.circle_filled(
                                        egui::pos2(rr.left() + 14.0, rr.center().y),
                                        3.5,
                                        dot,
                                    );
                                    p.text(
                                        egui::pos2(rr.left() + 26.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        "변경사항 (커밋 전)",
                                        FontId::proportional(13.0),
                                        if self.ws.local_sel {
                                            Color32::WHITE
                                        } else {
                                            C_TEXT
                                        },
                                    );
                                    p.text(
                                        egui::pos2(rr.right() - 8.0, rr.center().y),
                                        Align2::RIGHT_CENTER,
                                        format!("{}개 파일", local_files.len()),
                                        FontId::proportional(11.5),
                                        C_TEXT_DIM,
                                    );
                                    if resp.clicked() {
                                        sel_local = true;
                                    }
                                    ui.painter().hline(
                                        rr.x_range(),
                                        rr.bottom(),
                                        Stroke::new(1.0, C_BORDER.gamma_multiply(0.7)),
                                    );
                                }

                                for (i, c) in self.ws.commits.iter().enumerate() {
                                    let (rr, resp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 24.0),
                                        Sense::click(),
                                    );
                                    let p = ui.painter();
                                    let sel = !self.ws.local_sel && i == self.ws.commit_sel;
                                    if sel {
                                        p.rect_filled(rr, 0.0, C_SEL);
                                    } else if resp.hovered() {
                                        p.rect_filled(rr, 0.0, C_HOVER);
                                    }
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

                    let (dr, dresp) =
                        ui.allocate_exact_size(Vec2::new(9.0, body_h), Sense::click_and_drag());
                    if dresp.hovered() || dresp.dragged() {
                        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                    }
                    if dresp.dragged() {
                        let x = left_w + dresp.drag_delta().x;
                        self.ws.log_split = (x / total_w).clamp(0.12, 0.88);
                    }
                    let line_c = if dresp.hovered() || dresp.dragged() {
                        C_ACCENT
                    } else {
                        C_BORDER
                    };
                    ui.painter()
                        .vline(dr.center().x, dr.y_range(), Stroke::new(1.0, line_c));

                    // Changed files of the selected commit.
                    ui.vertical(|ui| {
                        ui.set_min_height(body_h);
                        egui::ScrollArea::vertical()
                            .id_salt(("commit_files", self.ws.seq))
                            .max_height(body_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                // (path, status) rows: working-tree changes when
                                // the pinned row is selected, else the commit's.
                                let rows: Vec<(String, crate::git::FileStatus)> =
                                    if self.ws.local_sel {
                                        local_files.clone()
                                    } else {
                                        self.ws
                                            .commit_files
                                            .iter()
                                            .map(|f| (f.path.clone(), f.status))
                                            .collect()
                                    };
                                if rows.is_empty() {
                                    ui.add_space(14.0);
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            egui::RichText::new(if self.ws.local_sel {
                                                "커밋 전 변경사항 없음"
                                            } else {
                                                "변경 파일 없음"
                                            })
                                            .color(C_TEXT_DIM),
                                        );
                                    });
                                }
                                for (path, status) in &rows {
                                    let (rr, resp) = ui.allocate_exact_size(
                                        Vec2::new(ui.available_width(), 22.0),
                                        Sense::click(),
                                    );
                                    let p = ui.painter();
                                    let is_open = self
                                        .ws
                                        .diff_view
                                        .as_ref()
                                        .map(|d| &d.file == path)
                                        .unwrap_or(false);
                                    if is_open {
                                        p.rect_filled(rr, 0.0, C_SEL);
                                    } else if resp.hovered() {
                                        p.rect_filled(rr, 0.0, C_HOVER);
                                    }
                                    p.text(
                                        egui::pos2(rr.left() + 10.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        status_letter(*status),
                                        FontId::monospace(12.0),
                                        status_color(*status),
                                    );
                                    p.text(
                                        egui::pos2(rr.left() + 26.0, rr.center().y),
                                        Align2::LEFT_CENTER,
                                        path,
                                        FontId::proportional(12.5),
                                        if is_open { Color32::WHITE } else { C_TEXT },
                                    );
                                    if resp.clicked() {
                                        if self.ws.local_sel {
                                            open_wfile = Some(path.clone());
                                        } else {
                                            open_file = Some(path.clone());
                                        }
                                    }
                                }
                            });
                    });
                });
            });

        if close {
            self.ws.log_open = false;
        }
        if sel_local {
            self.ws.local_sel = true;
            self.refresh_git_status(ctx);
        }
        if let Some(i) = sel_commit {
            self.select_commit(i);
        }
        if let Some(f) = open_file {
            self.open_commit_diff(f);
        }
        if let Some(f) = open_wfile {
            self.open_working_diff(f);
        }
    }

    fn central(&mut self, ctx: &egui::Context) {
        let bg = C_EDITOR;
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(bg).inner_margin(0.0))
            .show(ctx, |ui| {
                // A commit diff takes over the editor area while open.
                if self.ws.diff_view.is_some() {
                    self.diff_area(ui);
                    return;
                }
                self.tab_bar(ui);
                if self.ws.search_open {
                    self.search_bar(ui);
                }
                if self.ws.active.is_none() {
                    self.welcome(ui);
                    return;
                }
                self.editor(ui, bg);
            });
    }

    /// Render the open commit diff (header + unified diff body).
    fn diff_area(&mut self, ui: &mut egui::Ui) {
        let Some(mut diff) = self.ws.diff_view.take() else {
            return;
        };
        if diff.jobs_font != self.font_size {
            let (text, job) =
                build_diff_doc(&self.highlighter, &diff.file, &diff.lines, self.font_size);
            diff.edit_buf = text.clone();
            diff.text = text;
            diff.job = job;
            diff.jobs_font = self.font_size;
        }
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
        let n = diff.lines.len().max(1);
        let gutter_w = 104.0;
        egui::ScrollArea::both()
            .id_salt(("diff_scroll", self.ws.seq))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.horizontal_top(|ui| {
                    // Gutter: old/new line numbers + the +/- mark per row.
                    let (g_rect, _) = ui.allocate_exact_size(
                        Vec2::new(gutter_w, n as f32 * lh + 4.0),
                        Sense::hover(),
                    );
                    let p = ui.painter();
                    let clip = ui.clip_rect();
                    // Rows start 2px down: the TextEdit's inner top margin.
                    let top = g_rect.top() + 2.0;

                    // 1) Add/Del/Hunk row backgrounds first (full width, under
                    // both the gutter text and the code).
                    let galley_w = ui.fonts(|f| f.layout_job(diff.job.clone())).size().x;
                    let band_right = clip.right().max(g_rect.right() + galley_w + 24.0);
                    for (li, dl) in diff.lines.iter().enumerate() {
                        let bg = match dl.kind {
                            crate::git::DiffKind::Add => Color32::from_rgb(0x28, 0x3a, 0x28),
                            crate::git::DiffKind::Del => Color32::from_rgb(0x3f, 0x2b, 0x2b),
                            crate::git::DiffKind::Hunk => Color32::from_rgb(0x2b, 0x2d, 0x30),
                            crate::git::DiffKind::Context => continue,
                        };
                        let y = top + li as f32 * lh;
                        if y + lh < clip.top() || y > clip.bottom() {
                            continue;
                        }
                        p.rect_filled(
                            Rect::from_min_max(
                                egui::pos2(g_rect.left() - 8.0, y),
                                egui::pos2(band_right, y + lh),
                            ),
                            0.0,
                            bg,
                        );
                    }

                    // 2) Gutter: old/new line numbers + the +/- mark per row.
                    for (li, dl) in diff.lines.iter().enumerate() {
                        let y = top + li as f32 * lh;
                        if y + lh < clip.top() || y > clip.bottom() {
                            continue;
                        }
                        let cy = y + lh / 2.0;
                        let (mark, mark_col) = match dl.kind {
                            crate::git::DiffKind::Add => {
                                ("+", Color32::from_rgb(0x7e, 0xc6, 0x99))
                            }
                            crate::git::DiffKind::Del => {
                                ("-", Color32::from_rgb(0xe0, 0x6c, 0x75))
                            }
                            _ => ("", C_TEXT_DIM),
                        };
                        if let Some(o) = dl.old_no {
                            p.text(
                                egui::pos2(g_rect.left() + 40.0, cy),
                                Align2::RIGHT_CENTER,
                                o.to_string(),
                                FontId::monospace(11.5),
                                C_TEXT_DIM,
                            );
                        }
                        if let Some(nn) = dl.new_no {
                            p.text(
                                egui::pos2(g_rect.left() + 84.0, cy),
                                Align2::RIGHT_CENTER,
                                nn.to_string(),
                                FontId::monospace(11.5),
                                C_TEXT_DIM,
                            );
                        }
                        if !mark.is_empty() {
                            p.text(
                                egui::pos2(g_rect.left() + 92.0, cy),
                                Align2::LEFT_CENTER,
                                mark,
                                font.clone(),
                                mark_col,
                            );
                        }
                    }

                    // The diff body: read-only TextEdit → drag-select + ⌘C.
                    let job_for_layouter = diff.job.clone();
                    let mut layouter = move |ui: &egui::Ui, _buf: &str, _w: f32| {
                        ui.fonts(|f| f.layout_job(job_for_layouter.clone()))
                    };
                    let out = egui::TextEdit::multiline(&mut diff.edit_buf)
                        .font(font.clone())
                        .frame(false)
                        .desired_width(galley_w.max(ui.available_width()))
                        .layouter(&mut layouter)
                        .show(ui);
                    if out.response.changed() {
                        diff.edit_buf = diff.text.clone();
                    }
                });
            });

        if keep {
            self.ws.diff_view = Some(diff);
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
                        egui::TextEdit::singleline(&mut self.ws.search_query)
                            .hint_text("파일 내 검색")
                            .desired_width(260.0),
                    );
                    if self.ws.search_focus {
                        resp.request_focus();
                        self.ws.search_focus = false;
                    }
                    if resp.changed() {
                        self.refresh_search();
                        // jump to first match
                        if let (Some(i), Some(line)) =
                            (self.ws.active, self.ws.search_matches.first().copied())
                        {
                            self.ws.tabs[i].scroll_to_line = Some(line);
                        }
                    }
                    let enter =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    let shift = ui.input(|i| i.modifiers.shift);
                    if enter {
                        self.search_step(!shift);
                        self.ws.search_focus = true; // keep focus for repeated Enter
                    }
                    if ui.small_button("▲").clicked() {
                        self.search_step(false);
                    }
                    if ui.small_button("▼").clicked() {
                        self.search_step(true);
                    }
                    let label = if self.ws.search_matches.is_empty() {
                        if self.ws.search_query.is_empty() {
                            String::new()
                        } else {
                            "결과 없음".to_string()
                        }
                    } else {
                        format!("{}/{}", self.ws.search_cur + 1, self.ws.search_matches.len())
                    };
                    ui.label(egui::RichText::new(label).weak());
                    if ui.small_button("✕").clicked() {
                        self.ws.search_open = false;
                    }
                });
            });
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        if self.ws.tabs.is_empty() {
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
                    .id_salt(("tab_scroll", self.ws.seq))
                    .auto_shrink([false, false])
                    .max_height(TAB_H)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.horizontal(|ui| {
                            for i in 0..self.ws.tabs.len() {
                                let selected = self.ws.active == Some(i);
                                let name = self.ws.tabs[i]
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
                                self.icons.file(p, icon, &name);
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
            self.record_origin();
            self.ws.active = Some(i);
            self.refresh_search();
            let t = &self.ws.tabs[i];
            let (p, line) = (t.path.clone(), t.caret_line.or(t.flash_line).unwrap_or(0));
            self.record_nav(p, line);
        }
        if let Some(i) = close {
            self.ws.tabs.remove(i);
            self.ws.active = if self.ws.tabs.is_empty() {
                None
            } else {
                Some(self.ws.active.unwrap_or(0).min(self.ws.tabs.len() - 1))
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
        let idx = match self.ws.active {
            Some(i) if i < self.ws.tabs.len() => i,
            _ => return,
        };

        let cmd_held = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        let gutter_color = Color32::from_rgb(0x4b, 0x50, 0x58);
        let font = FontId::monospace(self.font_size);

        // Build (or reuse cached) gutter + code width — only rebuilt on font change.
        let ctx = ui.ctx().clone();
        self.ws.tabs[idx].ensure_render(&ctx, self.font_size);

        // Layouter feeds the cached highlighted job to the TextEdit.
        let job_for_layouter = self.ws.tabs[idx].job.clone();
        let mut layouter = move |ui: &egui::Ui, _buf: &str, _w: f32| {
            ui.fonts(|f| f.layout_job(job_for_layouter.clone()))
        };

        let scroll_target = self.ws.tabs[idx].scroll_to_line;
        let mut goto: Option<String> = None;
        let mut clear_scroll = false;
        let mut clicked_nothing = false;
        let mut caret_clicked: Option<usize> = None;

        // Borrow the single tab so edit_buf / content can be touched independently.
        let tab = &mut self.ws.tabs[idx];
        let gutter_job = tab.gutter_job.clone();
        let code_w = tab.code_w;

        // Make the TextEdit cover at least the whole visible pane, so the
        // empty space below a short file still behaves like the editor
        // (I-beam cursor, click places the caret, drag selects).
        let view_h = ui.available_height();
        let min_rows = ui.fonts(|f| (view_h / f.row_height(&font)).floor().max(1.0) as usize);

        egui::ScrollArea::both()
            .id_salt(("editor_scroll", self.ws.seq))
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
                        .desired_rows(min_rows)
                        .layouter(&mut layouter)
                        .show(ui);

                    // Enforce read-only: revert any edit immediately.
                    if out.response.changed() {
                        tab.edit_buf = tab.content.clone();
                    }

                    let rows = out.galley.rows.len().max(1);
                    let row_h = out.galley.size().y / rows as f32;
                    let band_left = g_rect.left() - 8.0;
                    // Full-width band: reach the visible pane edge (clip rect)
                    // even when the code is narrower — IntelliJ paints the
                    // caret line across the whole editor, not just the text.
                    let band_right = ui
                        .clip_rect()
                        .right()
                        .max(out.galley_pos.x + out.galley.size().x + 16.0);

                    // Track the caret. The TextEdit keeps a (possibly stale)
                    // caret across tab switches and jumps, so only a real
                    // caret move (click / drag / arrow keys) may update our
                    // tracked position.
                    if let Some(cur) = out.cursor_range {
                        let row = cur.primary.rcursor.row;
                        let ci = cur.primary.ccursor.index;
                        if tab.last_cursor_ci != Some(ci) || out.response.clicked() {
                            tab.last_cursor_ci = Some(ci);
                            tab.caret_ci = Some(ci);
                            tab.caret_line = Some(row);
                            if out.response.clicked() {
                                caret_clicked = Some(row);
                                // A click supersedes the jump highlight — one
                                // band on screen, where the user clicked.
                                tab.flash_line = None;
                            }
                        }
                    }

                    // Active (caret) line band, from the TRACKED caret — a
                    // freshly opened file shows no band until the user clicks
                    // or jumps. Translucent so text shows through.
                    if let Some(row) = tab.caret_line {
                        let y = out.galley_pos.y + row_h * row as f32;
                        ui.painter().rect_filled(
                            Rect::from_min_max(
                                egui::pos2(band_left, y),
                                egui::pos2(band_right, y + row_h),
                            ),
                            0.0,
                            Color32::from_rgba_unmultiplied(0x6a, 0x72, 0x8a, 30),
                        );
                    }

                    // Soft highlight of the most recently jumped-to line.
                    if let Some(line) = tab.flash_line {
                        let y = out.galley_pos.y + row_h * line as f32;
                        ui.painter().rect_filled(
                            Rect::from_min_max(
                                egui::pos2(band_left, y),
                                egui::pos2(band_right, y + row_h),
                            ),
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

        // Every plain click is a navigation point (IntelliJ-style): Back walks
        // through previous click positions, not only through jumps. Clicks on
        // nearby lines refine the current entry instead of flooding history.
        if let Some(line) = caret_clicked {
            if !ctx.input(|i| i.modifiers.command || i.modifiers.ctrl) {
                self.note_click_nav(line);
            }
        }

        if clear_scroll {
            self.ws.tabs[idx].scroll_to_line = None;
        }
        if clicked_nothing {
            self.ws.status = "⌘+클릭: 식별자 위에서 클릭하세요".to_string();
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

/// Record every expanded directory (used to rebuild the tree after git ops
/// without collapsing what the user had open).
fn collect_expanded(node: &TreeNode, out: &mut std::collections::HashSet<PathBuf>) {
    if node.is_dir && node.expanded {
        out.insert(node.path.clone());
        if let Some(children) = &node.children {
            for c in children {
                collect_expanded(c, out);
            }
        }
    }
}

/// Re-expand directories from a recorded set, loading children as needed.
fn apply_expanded(node: &mut TreeNode, set: &std::collections::HashSet<PathBuf>) {
    if node.is_dir && set.contains(&node.path) {
        node.expanded = true;
        if node.children.is_none() {
            node.children = Some(load_children(&node.path));
        }
        if let Some(children) = &mut node.children {
            for c in children {
                apply_expanded(c, set);
            }
        }
    }
}

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

/// One project tab in the top bar. Returns true when clicked (switch to it);
/// sets `close_to` instead when its ✕ is clicked.
fn project_chip(
    ui: &mut egui::Ui,
    seq: usize,
    name: &str,
    active: bool,
    close_to: &mut Option<PathBuf>,
    root: &Path,
) -> bool {
    let font = FontId::proportional(13.0);
    let text_w = ui.fonts(|f| {
        f.layout_no_wrap(name.to_string(), font.clone(), C_TEXT)
            .size()
            .x
    });
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(text_w + 16.0 + 18.0, 24.0), Sense::click());
    let p = ui.painter();
    if active {
        p.rect_filled(rect, 5.0, C_HOVER);
    } else if resp.hovered() {
        p.rect_filled(rect, 5.0, C_HOVER.gamma_multiply(0.55));
    }
    p.text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        name,
        font,
        if active { C_TEXT } else { C_TEXT_DIM },
    );
    if active {
        p.hline(
            egui::Rangef::new(rect.left() + 5.0, rect.right() - 5.0),
            rect.bottom() - 1.0,
            Stroke::new(2.0, C_ACCENT),
        );
    }
    // Close ✕ with its own hit area (shown while the chip is hovered/active).
    let cx = Rect::from_center_size(
        egui::pos2(rect.right() - 11.0, rect.center().y),
        Vec2::splat(14.0),
    );
    let cresp = ui.interact(cx, ui.id().with(("proj_close", seq)), Sense::click());
    if active || resp.hovered() || cresp.hovered() {
        draw_x(
            ui.painter(),
            cx.shrink(3.0),
            if cresp.hovered() { C_TEXT } else { C_TEXT_DIM },
        );
    }
    if cresp.clicked() {
        *close_to = Some(root.to_path_buf());
        return false;
    }
    resp.clicked()
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

/// One row in the Structure panel: IntelliJ kind icon + symbol name, indented
/// by nesting depth. Returns true when clicked.
fn symbol_row(ui: &mut egui::Ui, s: &DocSymbol, icons: &icons::IconSet) -> bool {
    let full_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(full_w, 22.0), Sense::click());
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect, 0.0, C_HOVER);
    }

    let base_x = rect.left() + TREE_PAD_L + s.depth as f32 * TREE_INDENT;

    let side = 16.0;
    let badge = Rect::from_center_size(
        egui::pos2(base_x + side / 2.0, rect.center().y),
        Vec2::splat(side),
    );
    if !icons.symbol(painter, badge, s.kind) {
        // Fallback: colored rounded badge with the kind glyph.
        let (r, g, b) = s.kind.rgb();
        let accent = Color32::from_rgb(r, g, b);
        painter.rect_filled(badge, side * 0.24, accent.gamma_multiply(0.30));
        painter.text(
            badge.center(),
            egui::Align2::CENTER_CENTER,
            s.kind.glyph(),
            FontId::monospace(11.0),
            accent,
        );
    }

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

#[allow(clippy::too_many_arguments)]
fn show_node(
    ui: &mut egui::Ui,
    node: &mut TreeNode,
    depth: usize,
    active: Option<&Path>,
    status: &HashMap<PathBuf, crate::git::FileStatus>,
    icons: &icons::IconSet,
    to_open: &mut Option<PathBuf>,
    reveal: &mut Option<PathBuf>,
) {
    let selected = !node.is_dir && active == Some(node.path.as_path());

    let full_w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(full_w, TREE_ROW_H), Sense::click());
    // "Select Opened File": scroll this row into view (one-shot).
    if reveal.as_deref() == Some(node.path.as_path()) {
        ui.scroll_to_rect(rect, Some(Align::Center));
        *reveal = None;
    }
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
        icons.folder(painter, icon_rect, &node.name, node.expanded);
    } else {
        icons.file(painter, icon_rect, &node.name);
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
                show_node(ui, child, depth + 1, active, status, icons, to_open, reveal);
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
