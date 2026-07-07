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
    /// Per-line changes vs HEAD, for the gutter change bars. In a review tab
    /// this instead holds the PR's base…head overlay.
    git_changes: crate::git::FileDiff,
    /// PR-review tab: content comes from the PR head commit (not disk) and
    /// `git_changes` lines are painted as full-width change bands.
    is_review: bool,
    /// Markdown tab showing the RENDERED document instead of raw text.
    md_preview: bool,
    /// Review ghost rows: display lines holding PR-DELETED code, merged
    /// inline (struck through). Empty for normal tabs.
    ghost_rows: Vec<usize>,
    /// Byte ranges of the ghost rows in `content` (re-applied after any
    /// re-highlight, e.g. zoom).
    ghost_ranges: Vec<(usize, usize)>,
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
        // Markdown opens rendered by default (toggle back to raw any time).
        let md_preview = is_md_path(&path);
        Self {
            md_preview,
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
            is_review: false,
            ghost_rows: Vec::new(),
            ghost_ranges: Vec::new(),
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
        let ghost_color = Color32::from_rgba_unmultiplied(0xe0, 0x6c, 0x75, 150);
        let line_h = crate::highlight::line_height(font_size);
        // Ghost rows carry no line number ("−" instead), so real numbering
        // stays identical to the file on disk / at the PR head.
        let ghost: std::collections::HashSet<usize> = self.ghost_rows.iter().copied().collect();
        let digits = (self.line_count - self.ghost_rows.len()).to_string().len();
        let mut gutter = LayoutJob::default();
        gutter.wrap.max_width = f32::INFINITY;
        let mut n = 1usize;
        for row in 0..self.line_count {
            let (text, color) = if ghost.contains(&row) {
                (format!("{:>width$}\n", "−", width = digits), ghost_color)
            } else {
                let t = format!("{:>width$}\n", n, width = digits);
                n += 1;
                (t, gutter_color)
            };
            let mut fmt = TextFormat::simple(font.clone(), color);
            fmt.line_height = Some(line_h);
            gutter.append(&text, 0.0, fmt);
        }
        self.gutter_job = gutter;
        self.code_w = ctx.fonts(|f| f.layout_job(self.job.clone())).size().x + 24.0;
        self.render_font = font_size;
    }

    /// 1-based PR-head line for a display line; None on a ghost row (that
    /// code doesn't exist at the head, so e.g. it can't take a comment).
    fn head_line_of(&self, disp: usize) -> Option<usize> {
        if self.ghost_rows.binary_search(&disp).is_ok() {
            return None;
        }
        let ghosts_before = self.ghost_rows.partition_point(|&g| g < disp);
        Some(disp - ghosts_before + 1)
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
    Review,
    Activity,
}

fn is_md_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
        .unwrap_or(false)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Paths the live watcher ignores (VCS + build junk).
fn watch_noise(p: &Path) -> bool {
    p.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(
                ".git"
                    | "target"
                    | "node_modules"
                    | ".idea"
                    | "build"
                    | "dist"
                    | ".gradle"
                    | "__pycache__"
                    | ".next"
                    | ".venv"
                    | ".DS_Store"
            )
        )
    })
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
    // PR review session (left REVIEW panel + editor overlays).
    review: Option<crate::review::ReviewSession>,
    review_rx: Option<Receiver<Result<crate::review::ReviewSession, String>>>,
    review_input: String,
    review_err: Option<String>,
    /// Start reviews in a dedicated worktree (whole project = PR state).
    review_worktree: bool,
    // Open-PR picker (REVIEW panel, before a session starts).
    pr_list: Vec<crate::review::PrListItem>,
    pr_list_rx: Option<Receiver<Result<Vec<crate::review::PrListItem>, String>>>,
    pr_list_err: Option<String>,
    /// Only PRs whose review is requested from me (default) vs all open PRs.
    pr_list_mine: bool,
    /// Collapsed directory keys of the REVIEW panel's changed-file tree.
    review_collapsed: std::collections::HashSet<String>,
    // Line-comment composer (review tabs).
    comment_open: bool,
    comment_path: String,
    comment_line: usize, // 1-based head line
    comment_disp: usize, // 0-based display line
    comment_text: String,
    comment_focus: bool,
    // Review submission (approve / comment / request changes).
    submit_open: bool,
    submit_body: String,
    submit_event: usize, // 0 = COMMENT, 1 = APPROVE, 2 = REQUEST_CHANGES
    submit_rx: Option<Receiver<Result<String, String>>>,
    // AI Watch: live file watcher + activity feed (newest first).
    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: Option<Receiver<notify::Result<notify::Event>>>,
    feed: Vec<FeedEntry>,
    /// Last watcher-triggered git-status refresh (throttle).
    watch_refresh_at: Option<f64>,
}

/// One row of the AI-Watch activity feed.
struct FeedEntry {
    path: PathBuf,
    rel: String,
    /// Unix seconds of the LAST event on this path (bursts coalesce).
    at: i64,
    kind: u8, // 0 = modified, 1 = created, 2 = removed
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
            review: None,
            review_rx: None,
            review_input: String::new(),
            review_err: None,
            review_worktree: true,
            pr_list: Vec::new(),
            pr_list_rx: None,
            pr_list_err: None,
            pr_list_mine: true,
            review_collapsed: std::collections::HashSet::new(),
            comment_open: false,
            comment_path: String::new(),
            comment_line: 0,
            comment_disp: 0,
            comment_text: String::new(),
            comment_focus: false,
            submit_open: false,
            submit_body: String::new(),
            submit_event: 0,
            submit_rx: None,
            watcher: None,
            watch_rx: None,
            feed: Vec::new(),
            watch_refresh_at: None,
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
    /// Native macOS menu bar — kept alive for the app's lifetime.
    _menu: Option<muda::Menu>,
    /// In-flight PR review worktree creation → (worktree path, PR number).
    /// App-level: completion opens a NEW project workspace.
    worktree_rx: Option<Receiver<Result<(PathBuf, u64), String>>>,
    /// AI Watch follow mode: the editor jumps to whatever was just changed.
    follow_ai: bool,
    /// Markdown renderer cache (images, syntax-highlighted code blocks).
    md_cache: egui_commonmark::CommonMarkCache,
}

impl CodeLookApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial: Option<PathBuf>,
        pr: Option<u64>,
        shot: Option<crate::ShotConfig>,
    ) -> Self {
        setup_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

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
            _menu: None,
            worktree_rx: None,
            follow_ai: false,
            md_cache: egui_commonmark::CommonMarkCache::default(),
        };

        // Native macOS menu bar (open / search / zoom / settings live there,
        // not in the toolbar). Skipped in capture mode.
        #[cfg(target_os = "macos")]
        if app.shot.is_none() {
            let menu = app.build_menu();
            menu.init_for_nsapp();
            app._menu = Some(menu);
        }

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
            // PR review session, loaded synchronously for the screenshot.
            // `--pr 0` shows the empty REVIEW panel with the open-PR picker.
            if let (Some(n), Some(root)) = (s.pr, app.ws.project_root.clone()) {
                if n == 0 {
                    app.left_view = LeftView::Review;
                    app.ws.pr_list =
                        crate::review::list_prs(&root, app.ws.pr_list_mine).unwrap_or_default();
                }
            }
            if let (Some(n), Some(root)) = (s.pr.filter(|&n| n > 0), app.ws.project_root.clone()) {
                match crate::review::load(&root, n) {
                    Ok(sess) => {
                        app.ws.review = Some(sess);
                        app.left_view = LeftView::Review;
                        // Open the first modified file with its overlay.
                        if let Some(i) = app
                            .ws
                            .review
                            .as_ref()
                            .and_then(|s| s.files.iter().position(|f| !f.overlay.changed.is_empty()))
                        {
                            app.open_review_file(i);
                        }
                    }
                    Err(e) => app.ws.review_err = Some(e),
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
        // `--pr N`: start a review right away — in a dedicated worktree, so
        // the whole opened project ends up at the PR state.
        if let Some(n) = pr {
            if app.ws.project_root.is_some() {
                app.left_view = LeftView::Review;
                app.tree_open = true;
                app.start_review_worktree(&cc.egui_ctx, n);
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

        // Live file watcher (AI Watch): instant reaction to outside edits —
        // an agent in a terminal, another editor, a checkout. FSEvents-based
        // on macOS, so a huge root is fine. Skipped in capture mode.
        if self.shot.is_none() {
            let (wtx, wrx) = std::sync::mpsc::channel();
            let wctx = ctx.clone();
            let watcher = notify::recommended_watcher(move |res| {
                let _ = wtx.send(res);
                wctx.request_repaint();
            })
            .ok()
            .and_then(|mut w| {
                use notify::Watcher;
                w.watch(&path, notify::RecursiveMode::Recursive).ok()?;
                Some(w)
            });
            self.ws.watch_rx = watcher.is_some().then_some(wrx);
            self.ws.watcher = watcher;
        }

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
        // Prefer a regular (disk) tab; fall back to a review tab of the same
        // path so Back/Forward through review locations doesn't duplicate tabs.
        if let Some(i) = self
            .ws
            .tabs
            .iter()
            .position(|t| t.path == path && !t.is_review)
            .or_else(|| self.ws.tabs.iter().position(|t| t.path == path))
        {
            self.ws.active = Some(i);
            self.ws.diff_view = None; // an open diff would keep covering the editor
            return;
        }
        self.open_tab_from_disk(path);
    }

    /// Read `path` from disk into a new active tab; false when unreadable.
    fn open_tab_from_disk(&mut self, path: PathBuf) -> bool {
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
                true
            }
            Err(_) => {
                self.ws.status = format!(
                    "열 수 없는 파일(바이너리 또는 권한): {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                false
            }
        }
    }

    /// Open the on-disk (working-tree) version of a file at a line — never
    /// matches a review tab, so it always shows what's actually on disk.
    fn open_disk_file(&mut self, path: PathBuf, line: usize) {
        self.record_origin();
        if let Some(i) = self
            .ws
            .tabs
            .iter()
            .position(|t| !t.is_review && t.path == path)
        {
            self.ws.active = Some(i);
            self.ws.diff_view = None;
        } else {
            self.ws.diff_view = None;
            if !self.open_tab_from_disk(path.clone()) {
                return;
            }
        }
        self.set_jump_target(line);
        self.record_nav(path, line);
    }

    /// From a review tab, jump to the file the PR is actually changing: the
    /// same path (and line) in the MAIN working tree. Reviewing in a
    /// worktree, this switches to the original project's workspace.
    fn open_source_file(&mut self, ctx: &egui::Context) {
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let Some(t) = self.ws.active.and_then(|i| self.ws.tabs.get(i)) else {
            return;
        };
        let line = t.caret_line.or(t.flash_line).unwrap_or(0);
        let Ok(rel) = t.path.strip_prefix(&root).map(|p| p.to_path_buf()) else {
            return;
        };
        match crate::git::main_worktree_root(&root) {
            Some(main) if main != root => {
                self.open_project(ctx, main.clone());
                self.open_disk_file(main.join(&rel), line);
            }
            _ => self.open_disk_file(root.join(&rel), line),
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

    // ---- Native macOS menu bar ----------------------------------------------

    /// Build the menu bar. Shortcut hints are shown in the labels (functional
    /// key handling stays in the in-app keymap, so rebinding keeps working).
    fn build_menu(&self) -> muda::Menu {
        use crate::keymap::Action as A;
        use muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};
        let item = |id: &str, label: &str, a: Option<A>| {
            let text = match a {
                Some(a) => format!("{label}   {}", self.keymap.text(a)),
                None => label.to_string(),
            };
            MenuItem::with_id(id, text, true, None)
        };

        let menu = Menu::new();
        let app_m = Submenu::new("CodeLook", true);
        let _ = app_m.append_items(&[
            &item("settings", "설정 · 단축키…", None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(Some("CodeLook 종료")),
        ]);
        let file_m = Submenu::new("파일", true);
        let _ = file_m.append_items(&[
            &item("open_project", "프로젝트 열기…", None),
            &PredefinedMenuItem::separator(),
            &item("close_tab", "탭 닫기", Some(A::CloseTab)),
        ]);
        let find_m = Submenu::new("이동 / 검색", true);
        let _ = find_m.append_items(&[
            &item("find_in_file", "파일 내 검색", Some(A::FindInFile)),
            &item("find_in_project", "전체 검색 (Find in Files)", Some(A::FindInProject)),
            &PredefinedMenuItem::separator(),
            &item("go_to_file", "파일로 이동", Some(A::GoToFile)),
            &item("go_to_symbol", "심볼로 이동", Some(A::GoToSymbol)),
            &item("recent_files", "최근 파일", Some(A::RecentFiles)),
            &item("go_to_line", "줄로 이동", Some(A::GoToLine)),
        ]);
        let view_m = Submenu::new("보기", true);
        let _ = view_m.append_items(&[
            &item("zoom_in", "글자 크게", Some(A::ZoomIn)),
            &item("zoom_out", "글자 작게", Some(A::ZoomOut)),
        ]);
        let _ = menu.append_items(&[&app_m, &file_m, &find_m, &view_m]);
        menu
    }

    /// Handle clicks coming from the native menu bar.
    fn poll_menu(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            match ev.id().as_ref() {
                "settings" => self.settings_open = true,
                "open_project" => {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.open_project(ctx, dir);
                    }
                }
                "close_tab" => self.close_active_tab(),
                "find_in_file" => {
                    if self.ws.active.is_some() {
                        self.ws.search_open = true;
                        self.ws.search_focus = true;
                    }
                }
                "find_in_project" => {
                    if self.ws.project_root.is_some() {
                        self.ws.gsearch_open = true;
                        self.ws.gsearch_focus = true;
                        self.gsearch_warm(ctx);
                    }
                }
                "go_to_file" => self.open_finder(FinderMode::File),
                "go_to_symbol" => self.open_finder(FinderMode::Symbol),
                "recent_files" => self.open_finder(FinderMode::Recent),
                "go_to_line" => {
                    if self.ws.active.is_some() {
                        self.open_finder(FinderMode::Line);
                    }
                }
                "zoom_in" => self.zoom(1.0),
                "zoom_out" => self.zoom(-1.0),
                _ => {}
            }
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
            // ⌘1 focuses/collapses the Project view specifically.
            if self.tree_open && self.left_view == LeftView::Project {
                self.tree_open = false;
            } else {
                self.left_view = LeftView::Project;
                self.tree_open = true;
            }
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
        if self.keymap.pressed(ctx, A::NextChange) {
            self.jump_change(1);
        }
        if self.keymap.pressed(ctx, A::PrevChange) {
            self.jump_change(-1);
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
            if tab.is_review {
                // Review tabs read from the PR head commit — disk is irrelevant.
                rebuilt.push(tab);
                continue;
            }
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
                        let matches = |b: &&crate::git::BranchInfo| {
                            filt.is_empty() || b.name.to_lowercase().contains(&filt)
                        };
                        let locals: Vec<_> =
                            self.ws.branches.iter().filter(|b| !b.is_remote).filter(matches).collect();
                        let remotes: Vec<_> =
                            self.ws.branches.iter().filter(|b| b.is_remote).filter(matches).collect();

                        let section = |ui: &mut egui::Ui, title: &str| {
                            let (hr, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), 22.0),
                                Sense::hover(),
                            );
                            ui.painter().text(
                                egui::pos2(hr.left() + 8.0, hr.center().y + 2.0),
                                Align2::LEFT_CENTER,
                                title,
                                FontId::proportional(10.5),
                                C_HEADER,
                            );
                        };
                        let mut row = |ui: &mut egui::Ui, b: &crate::git::BranchInfo| {
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
                                egui::pos2(rr.left() + 16.0, rr.center().y),
                                if b.is_current { C_ACCENT } else { C_TEXT_DIM },
                            );
                            p.text(
                                egui::pos2(rr.left() + 28.0, rr.center().y),
                                Align2::LEFT_CENTER,
                                &b.name,
                                FontId::proportional(13.0),
                                if b.is_current { Color32::WHITE } else { C_TEXT },
                            );
                            if rresp.clicked() && !b.is_current && !op_running {
                                checkout = Some(b.name.clone());
                            }
                        };

                        if !locals.is_empty() {
                            section(ui, "LOCAL");
                            for b in &locals {
                                row(ui, b);
                            }
                        }
                        if !remotes.is_empty() {
                            ui.add_space(4.0);
                            section(ui, "REMOTE");
                            for b in &remotes {
                                row(ui, b);
                            }
                        }
                        if locals.is_empty() && remotes.is_empty() {
                            ui.add_space(14.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new("일치하는 브랜치 없음").color(C_TEXT_DIM),
                                );
                            });
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

    // ---- PR review -----------------------------------------------------

    /// Load a PR review session in the background (fetch + diff — the
    /// working tree is never touched).
    fn start_review(&mut self, ctx: &egui::Context, number: u64) {
        if self.ws.review_rx.is_some() {
            return;
        }
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        self.ws.review_err = None;
        let (tx, rx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::review::load(&root, number));
            c.request_repaint();
        });
        self.ws.review_rx = Some(rx);
        self.ws.status = format!("PR #{number} 불러오는 중…");
    }

    /// Fetch the open-PR list for the REVIEW panel picker (background, gh CLI).
    fn kick_pr_list(&mut self, ctx: &egui::Context) {
        if self.ws.pr_list_rx.is_some() {
            return;
        }
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        self.ws.pr_list_err = None;
        let mine = self.ws.pr_list_mine;
        let (tx, rx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::review::list_prs(&root, mine));
            c.request_repaint();
        });
        self.ws.pr_list_rx = Some(rx);
    }

    /// Review in a dedicated worktree: check the PR head out into
    /// `~/.codelook/worktrees/…` (background — checkouts of big repos take a
    /// while) and open it as its own project workspace when ready.
    fn start_review_worktree(&mut self, ctx: &egui::Context, number: u64) {
        if self.worktree_rx.is_some() {
            return;
        }
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        self.ws.review_err = None;
        let (tx, rx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::review::ensure_worktree(&root, number).map(|p| (p, number)));
            c.request_repaint();
        });
        self.worktree_rx = Some(rx);
        self.ws.status = format!("PR #{number} 워크트리 준비 중… (PR 시점으로 체크아웃)");
    }

    /// Jump to the next/previous change block in the active tab (review
    /// overlay or the plain vs-HEAD gutter changes), wrapping around.
    fn jump_change(&mut self, dir: isize) {
        let Some(i) = self.ws.active.filter(|&i| i < self.ws.tabs.len()) else {
            return;
        };
        let blocks = change_block_starts(&self.ws.tabs[i].git_changes);
        if blocks.is_empty() {
            return;
        }
        let cur = self.ws.tabs[i]
            .caret_line
            .or(self.ws.tabs[i].flash_line)
            .unwrap_or(0);
        let target = if dir > 0 {
            blocks.iter().copied().find(|&b| b > cur).unwrap_or(blocks[0])
        } else {
            blocks
                .iter()
                .rev()
                .copied()
                .find(|&b| b < cur)
                .unwrap_or(*blocks.last().unwrap())
        };
        self.record_origin();
        self.set_jump_target(target);
        let p = self.ws.tabs[i].path.clone();
        self.record_nav(p, target);
    }

    /// Open a reviewed file as the FULL file (PR-head content) with its
    /// change overlay — review with complete context, not a clipped diff.
    fn open_review_file(&mut self, idx: usize) {
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let Some(sess) = self.ws.review.as_ref() else {
            return;
        };
        let Some(rf) = sess.files.get(idx) else {
            return;
        };
        // A deleted file has no head content — its unified diff is the view.
        if rf.status == crate::git::FileStatus::Deleted {
            self.open_review_diff(idx);
            return;
        }
        let path = root.join(&rf.path);
        let overlay = rf.overlay.clone();
        let ghosts = rf.deleted_text.clone();
        let rel = rf.path.clone();
        let head_id = sess.head_id.clone();
        let from_disk = sess.head_is_workdir;

        self.record_origin();
        self.ws.diff_view = None;
        // The file under review always sits at the FIRST tab slot, so the
        // eye never hunts for it in a long tab strip.
        if let Some(i) = self
            .ws
            .tabs
            .iter()
            .position(|t| t.is_review && t.path == path)
        {
            let t = self.ws.tabs.remove(i);
            self.ws.tabs.insert(0, t);
            self.ws.active = Some(0);
        } else {
            let content = if from_disk {
                std::fs::read_to_string(&path).ok()
            } else {
                crate::review::file_at(&root, &head_id, &rel)
            };
            let Some(content) = content else {
                self.ws.status = format!("PR 파일을 읽을 수 없음: {rel}");
                return;
            };
            // Deleted code comes back as inline ghost rows (struck through),
            // and the overlay is remapped to the merged document's lines.
            let (content, overlay, ghost_rows, ghost_ranges) =
                build_ghost_doc(&content, &overlay, &ghosts);
            let mut job = self
                .highlighter
                .highlight(path.to_str().unwrap_or(""), &content, self.font_size);
            stylize_ghosts(&mut job, &ghost_ranges);
            let lang = ast::Lang::from_path(&path);
            let outline = lang
                .map(|l| ast::document_symbols(l, &content))
                .unwrap_or_default();
            let mut tab = Tab::new(path.clone(), content, job, self.font_size, lang, outline);
            tab.git_changes = overlay;
            tab.is_review = true;
            // Review defaults to raw text — the change overlay lives there.
            tab.md_preview = false;
            tab.ghost_rows = ghost_rows;
            tab.ghost_ranges = ghost_ranges;
            self.ws.tabs.insert(0, tab);
            self.ws.active = Some(0);
            self.refresh_search();
        }
        // First change in DISPLAY lines (the tab's overlay is already
        // remapped over any inline ghost rows).
        let first_change = self
            .ws
            .active
            .and_then(|i| self.ws.tabs.get(i))
            .map(|t| {
                let a = t.git_changes.changed.first().map(|&(l, _)| l);
                let b = t.git_changes.deleted_before.first().copied();
                match (a, b) {
                    (Some(x), Some(y)) => x.min(y),
                    (x, y) => x.or(y).unwrap_or(0),
                }
            })
            .unwrap_or(0);
        self.set_jump_target(first_change);
        self.record_nav(path, first_change);
    }

    /// Open the classic unified diff (merge-base vs PR head) of a reviewed file.
    fn open_review_diff(&mut self, idx: usize) {
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let Some(sess) = self.ws.review.as_ref() else {
            return;
        };
        let Some(rf) = sess.files.get(idx) else {
            return;
        };
        let lines = crate::git::range_file_diff(&root, &sess.base_id, &sess.head_id, &rf.path);
        let (text, job) = build_diff_doc(&self.highlighter, &rf.path, &lines, self.font_size);
        self.ws.diff_view = Some(DiffView {
            commit_short: format!("PR #{}", sess.pr.number),
            file: rf.path.clone(),
            lines,
            edit_buf: text.clone(),
            text,
            job,
            jobs_font: self.font_size,
        });
    }

    // ---- AI Watch (live file watcher) -----------------------------------

    /// Pump the live watcher every frame: fold events into the activity
    /// feed, hot-reload open tabs, refresh git status (throttled) and drive
    /// follow mode.
    fn pump_watcher(&mut self, ctx: &egui::Context) {
        // Parked workspaces: keep their channels drained (feed bookkeeping
        // only, so nothing accumulates unbounded).
        for w in &mut self.parked {
            Self::drain_ws_events(w);
        }
        let touched = Self::drain_ws_events(&mut self.ws);
        if touched.is_empty() {
            return;
        }
        for p in &touched {
            self.reload_tab_from_disk(p);
        }
        let now = ctx.input(|i| i.time);
        if self.ws.watch_refresh_at.is_none_or(|t| now - t > 3.0) {
            self.ws.watch_refresh_at = Some(now);
            self.refresh_git_status(ctx);
        }
        if self.follow_ai {
            if let Some(p) = touched.iter().rev().find(|p| p.is_file()) {
                self.follow_open(p.clone());
            }
        }
    }

    /// Drain one workspace's watcher channel into its feed (one row per
    /// path, newest first). Returns the created/modified files.
    fn drain_ws_events(ws: &mut Workspace) -> Vec<PathBuf> {
        let Some(rx) = &ws.watch_rx else {
            return Vec::new();
        };
        let root = ws.project_root.clone().unwrap_or_default();
        let mut touched: Vec<(PathBuf, u8)> = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(Ok(ev)) => {
                    use notify::EventKind::*;
                    let kind = match ev.kind {
                        Create(_) => 1u8,
                        Remove(_) => 2,
                        Modify(_) | Any | Other => 0,
                        Access(_) => continue,
                    };
                    for p in ev.paths {
                        if !p.starts_with(&root) || watch_noise(&p) {
                            continue;
                        }
                        if kind != 2 && !p.is_file() {
                            continue; // directory churn is noise
                        }
                        if let Some(t) = touched.iter_mut().find(|(q, _)| *q == p) {
                            t.1 = t.1.max(kind);
                        } else {
                            touched.push((p, kind));
                        }
                    }
                }
                Ok(Err(_)) => {}
                Err(_) => break,
            }
        }
        let mut out = Vec::new();
        if touched.is_empty() {
            return out;
        }
        let now = unix_now();
        for (p, kind) in touched {
            let rel = p
                .strip_prefix(&root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            ws.feed.retain(|e| e.path != p);
            ws.feed.insert(
                0,
                FeedEntry {
                    path: p.clone(),
                    rel,
                    at: now,
                    kind,
                },
            );
            if kind != 2 {
                out.push(p);
            }
        }
        ws.feed.truncate(200);
        out
    }

    /// Hot-reload an open (non-review) tab whose file changed on disk,
    /// keeping the caret roughly in place.
    fn reload_tab_from_disk(&mut self, path: &Path) {
        let Some(i) = self
            .ws
            .tabs
            .iter()
            .position(|t| !t.is_review && t.path == path)
        else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        if content == self.ws.tabs[i].content {
            return;
        }
        let job = self
            .highlighter
            .highlight(path.to_str().unwrap_or(""), &content, self.font_size);
        let lang = ast::Lang::from_path(path);
        let outline = lang
            .map(|l| ast::document_symbols(l, &content))
            .unwrap_or_default();
        let root = self.ws.project_root.clone().unwrap_or_default();
        let git_changes = crate::git::file_line_changes(&root, path).unwrap_or_default();
        let t = &mut self.ws.tabs[i];
        let lc = content.lines().count().max(1);
        t.line_count = lc;
        t.edit_buf = content.clone();
        t.content = content;
        t.job = job;
        t.job_font = self.font_size;
        t.outline = outline;
        t.git_changes = git_changes;
        t.render_font = -1.0; // rebuild gutter + code width
        t.caret_line = t.caret_line.map(|l| l.min(lc - 1));
        t.caret_ci = None;
        t.last_cursor_ci = None;
        if self.ws.active == Some(i) {
            self.refresh_search();
        }
    }

    /// Follow mode: surface the just-changed file at its latest change,
    /// WITHOUT touching nav history (an agent editing 30 files must not
    /// bury the user's own trail).
    fn follow_open(&mut self, path: PathBuf) {
        let line = self
            .ws
            .project_root
            .as_ref()
            .and_then(|r| crate::git::file_line_changes(r, &path))
            .and_then(|d| d.changed.last().map(|&(l, _)| l))
            .unwrap_or(0);
        if let Some(i) = self
            .ws
            .tabs
            .iter()
            .position(|t| !t.is_review && t.path == path)
        {
            self.ws.active = Some(i);
            self.ws.diff_view = None;
            self.set_jump_target(line);
        } else if self.open_tab_from_disk(path) {
            self.set_jump_target(line);
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
                stylize_ghosts(&mut tab.job, &tab.ghost_ranges);
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

        self.pump_watcher(ctx);

        // A finished review-worktree checkout opens as its own project.
        if let Some(rx) = &self.worktree_rx {
            if let Ok(res) = rx.try_recv() {
                self.worktree_rx = None;
                match res {
                    Ok((path, n)) => {
                        self.open_project(ctx, path);
                        self.left_view = LeftView::Review;
                        self.tree_open = true;
                        self.start_review(ctx, n);
                    }
                    Err(e) => {
                        self.ws.status = "PR 워크트리 실패".to_string();
                        self.ws.review_err = Some(e);
                    }
                }
            }
        }

        if let Some(rx) = &self.ws.submit_rx {
            if let Ok(res) = rx.try_recv() {
                self.ws.submit_rx = None;
                match res {
                    Ok(m) => {
                        let wt = self
                            .ws
                            .project_root
                            .as_deref()
                            .is_some_and(crate::review::is_managed_worktree);
                        self.ws.status = if wt {
                            format!("리뷰 제출 완료: {m} — 세션 종료 시 워크트리 자동 정리")
                        } else {
                            format!("리뷰 제출 완료: {m}")
                        };
                        self.ws.submit_open = false;
                        self.ws.submit_body.clear();
                        if let Some(s) = self.ws.review.as_mut() {
                            s.pending.clear();
                        }
                    }
                    Err(e) => self.ws.status = format!("리뷰 제출 실패: {e}"),
                }
            }
        }

        if let Some(rx) = &self.ws.pr_list_rx {
            if let Ok(res) = rx.try_recv() {
                self.ws.pr_list_rx = None;
                match res {
                    Ok(list) => self.ws.pr_list = list,
                    Err(e) => self.ws.pr_list_err = Some(e),
                }
            }
        }

        if let Some(rx) = &self.ws.review_rx {
            if let Ok(res) = rx.try_recv() {
                self.ws.review_rx = None;
                match res {
                    Ok(sess) => {
                        self.ws.status = format!(
                            "PR #{} 로드 · 변경 파일 {}개",
                            sess.pr.number,
                            sess.files.len()
                        );
                        self.ws.review = Some(sess);
                        self.left_view = LeftView::Review;
                        self.tree_open = true;
                    }
                    Err(e) => {
                        self.ws.status = "PR 로드 실패".to_string();
                        self.ws.review_err = Some(e);
                    }
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
        self.poll_menu(ctx);
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
        self.right_strip(ctx);
        self.structure_panel(ctx);
        self.central(ctx);
        self.global_search_window(ctx);
        self.finder_window(ctx);
        self.settings_window(ctx);
        self.comment_window(ctx);
        self.submit_window(ctx);
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

                    // Panel toggles live on the window edges (tool strips),
                    // and actions in the macOS menu bar / shortcuts — the
                    // toolbar stays navigation-only.
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
        // Collapsed: keep the tool strip on the edge so reopening is obvious
        // (IntelliJ New UI behavior — the strip never disappears).
        if !self.tree_open {
            egui::SidePanel::left("tree_panel_strip")
                .resizable(false)
                .exact_width(36.0)
                .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
                .show(ctx, |ui| {
                    ui.horizontal_top(|ui| self.tool_strip(ui));
                });
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
                        LeftView::Review => self.review_sidebar(ui),
                        LeftView::Activity => self.activity_sidebar(ui),
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
        let review_tip = match &self.ws.review {
            Some(s) => format!(
                "코드리뷰 — PR #{} · {}/{} 검토",
                s.pr.number,
                s.viewed_count(),
                s.files.len()
            ),
            None => "코드리뷰 (Pull Request)".to_string(),
        };
        for (i, (view, tip)) in [
            (LeftView::Project, "프로젝트".to_string()),
            (LeftView::Commit, format!("변경사항 (커밋 전) — {n_changes}개 파일")),
            (LeftView::Review, review_tip),
            (LeftView::Activity, "AI 활동 (라이브 감시)".to_string()),
        ]
        .into_iter()
        .enumerate()
        {
            let r = Rect::from_min_size(
                egui::pos2(rect.left() + 5.0, rect.top() + 8.0 + i as f32 * 36.0),
                Vec2::splat(26.0),
            );
            let resp = ui.interact(r, ui.id().with(("tool_strip", i)), Sense::click());
            let active = self.tree_open && self.left_view == view;
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
                LeftView::Review => {
                    draw_pr_icon(p, r, if active { C_TEXT } else { C_TEXT_DIM });
                    // Purple dot = a review session is loaded.
                    if self.ws.review.is_some() {
                        p.circle_filled(
                            egui::pos2(r.right() - 3.0, r.top() + 3.0),
                            3.0,
                            Color32::from_rgb(0xc5, 0x95, 0xff),
                        );
                    }
                }
                LeftView::Activity => {
                    // Pulse (heartbeat) glyph.
                    let c = if active { C_TEXT } else { C_TEXT_DIM };
                    let st = Stroke::new(1.4, c);
                    let cy = r.center().y;
                    let cx = r.center().x;
                    let pts = [
                        (r.left() + 3.0, cy),
                        (cx - 4.0, cy),
                        (cx - 2.0, cy - 5.0),
                        (cx + 2.0, cy + 5.0),
                        (cx + 4.0, cy),
                        (r.right() - 3.0, cy),
                    ];
                    for w in pts.windows(2) {
                        p.line_segment(
                            [egui::pos2(w[0].0, w[0].1), egui::pos2(w[1].0, w[1].1)],
                            st,
                        );
                    }
                    // Green dot = activity within the last minute.
                    if self
                        .ws
                        .feed
                        .first()
                        .is_some_and(|e| unix_now() - e.at < 60)
                    {
                        p.circle_filled(
                            egui::pos2(r.right() - 3.0, r.top() + 3.0),
                            3.0,
                            Color32::from_rgb(0x62, 0xb5, 0x43),
                        );
                    }
                }
            }
            // Click the active icon to collapse the panel; any other click
            // opens (or switches) it — IntelliJ tool-window semantics.
            if resp.on_hover_text(tip).clicked() {
                if active {
                    self.tree_open = false;
                } else {
                    self.left_view = view;
                    self.tree_open = true;
                }
            }
        }

        // Bottom-anchored: commit-log panel toggle (bottom tool window).
        {
            let r = Rect::from_min_size(
                egui::pos2(rect.left() + 5.0, rect.bottom() - 34.0),
                Vec2::splat(26.0),
            );
            let resp = ui.interact(r, ui.id().with("strip_commits"), Sense::click());
            let active = self.ws.log_open;
            if active {
                p.rect_filled(r, 6.0, C_HOVER);
            } else if resp.hovered() {
                p.rect_filled(r, 6.0, C_HOVER.gamma_multiply(0.5));
            }
            // Git-commit glyph: a node on a line.
            let c = if active { C_TEXT } else { C_TEXT_DIM };
            let cy = r.center().y;
            p.line_segment(
                [egui::pos2(r.left() + 3.0, cy), egui::pos2(r.center().x - 5.0, cy)],
                Stroke::new(1.4, c),
            );
            p.line_segment(
                [egui::pos2(r.center().x + 5.0, cy), egui::pos2(r.right() - 3.0, cy)],
                Stroke::new(1.4, c),
            );
            p.circle_stroke(r.center(), 4.5, Stroke::new(1.4, c));
            if resp.on_hover_text("커밋 로그 (하단 패널)").clicked() {
                let c = ui.ctx().clone();
                self.toggle_log(&c);
            }
        }
    }

    /// Always-visible right edge strip: the Structure panel toggle.
    fn right_strip(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("right_strip")
            .resizable(false)
            .exact_width(36.0)
            .frame(egui::Frame::default().fill(C_PANEL).inner_margin(0.0))
            .show(ctx, |ui| {
                let h = ui.available_height();
                let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, h), Sense::hover());
                let p = ui.painter();
                p.vline(
                    rect.left() + 0.5,
                    rect.y_range(),
                    Stroke::new(1.0, C_BORDER),
                );
                let r = Rect::from_min_size(
                    egui::pos2(rect.left() + 5.0, rect.top() + 8.0),
                    Vec2::splat(26.0),
                );
                let resp = ui.interact(r, ui.id().with("strip_structure"), Sense::click());
                let active = self.structure_open;
                if active {
                    p.rect_filled(r, 6.0, C_HOVER);
                } else if resp.hovered() {
                    p.rect_filled(r, 6.0, C_HOVER.gamma_multiply(0.5));
                }
                // Structure glyph: bulleted outline rows (second one indented).
                let c = if active { C_TEXT } else { C_TEXT_DIM };
                for (i, indent) in [0.0_f32, 4.0, 0.0].iter().enumerate() {
                    let y = r.top() + 7.0 + i as f32 * 6.0;
                    let x = r.left() + 5.0 + indent;
                    p.circle_filled(egui::pos2(x, y), 1.4, c);
                    p.line_segment(
                        [egui::pos2(x + 4.0, y), egui::pos2(r.right() - 4.0, y)],
                        Stroke::new(1.3, c),
                    );
                }
                if resp.on_hover_text("구조 (STRUCTURE)").clicked() {
                    self.structure_open = !self.structure_open;
                }
            });
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

    /// Left REVIEW panel: full-context PR review. Load a PR by number; the
    /// changed-file list opens FULL files with the PR's changes overlaid,
    /// per-file unified diffs, and viewed-tracking with progress.
    fn review_sidebar(&mut self, ui: &mut egui::Ui) {
        self.left_header(ui, "REVIEW", false);
        let loading = self.ws.review_rx.is_some() || self.worktree_rx.is_some();

        // No session yet: the PR-number prompt.
        if self.ws.review.is_none() {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("PR 번호로 리뷰 시작")
                        .color(C_TEXT_DIM)
                        .size(12.0),
                );
            });
            ui.add_space(4.0);
            let mut go = false;
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.ws.review_input)
                        .hint_text("예: 50460")
                        .desired_width(90.0),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    go = true;
                }
                if loading {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("불러오는 중…").color(C_TEXT_DIM).size(12.0),
                    );
                } else if ui.button("불러오기").clicked() {
                    go = true;
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.checkbox(&mut self.ws.review_worktree, "")
                    .on_hover_text("별도 워크트리에 PR을 체크아웃해 프로젝트 전체를 PR 시점으로 엽니다.\n지금 작업 트리는 건드리지 않습니다.");
                ui.label(
                    egui::RichText::new("전용 워크트리에서 (전체 코드 = PR 시점)")
                        .color(C_TEXT_DIM)
                        .size(11.5),
                );
            });
            if let Some(e) = self.ws.review_err.clone() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(e)
                                .color(Color32::from_rgb(0xe0, 0x6c, 0x75))
                                .size(11.5),
                        )
                        .wrap(),
                    );
                });
            }
            // Open-PR picker: choose from the repo's open PRs instead of
            // typing a number. Loaded once per panel visit via gh.
            if self.ws.pr_list.is_empty()
                && self.ws.pr_list_rx.is_none()
                && self.ws.pr_list_err.is_none()
                && self.shot.is_none()
            {
                let ctx = ui.ctx().clone();
                self.kick_pr_list(&ctx);
            }
            let mut chosen: Option<u64> = None;
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                let what = if self.ws.pr_list_mine {
                    "내 리뷰 요청"
                } else {
                    "열린 PR"
                };
                ui.label(
                    egui::RichText::new(if self.ws.pr_list.is_empty() {
                        what.to_string()
                    } else {
                        format!("{what} {}개", self.ws.pr_list.len())
                    })
                    .color(C_HEADER)
                    .size(11.5),
                );
                if self.ws.pr_list_rx.is_some() {
                    ui.spinner();
                } else {
                    let toggle = if self.ws.pr_list_mine {
                        "전체 보기"
                    } else {
                        "내 리뷰만"
                    };
                    if ui.small_button(toggle).clicked() {
                        self.ws.pr_list_mine = !self.ws.pr_list_mine;
                        self.ws.pr_list.clear();
                        self.ws.pr_list_err = None;
                        let ctx = ui.ctx().clone();
                        self.kick_pr_list(&ctx);
                    }
                    if ui.small_button("새로고침").clicked() {
                        self.ws.pr_list.clear();
                        self.ws.pr_list_err = None;
                        let ctx = ui.ctx().clone();
                        self.kick_pr_list(&ctx);
                    }
                }
            });
            if let Some(e) = &self.ws.pr_list_err {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("PR 목록 불가 (gh CLI): {e}"))
                                .color(C_TEXT_DIM)
                                .size(11.0),
                        )
                        .wrap(),
                    );
                });
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt(("pr_list", self.ws.seq))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for pr in &self.ws.pr_list {
                        let (rr, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 40.0),
                            Sense::click(),
                        );
                        let p = ui.painter();
                        if resp.hovered() {
                            p.rect_filled(rr, 0.0, C_HOVER);
                        }
                        let y1 = rr.top() + 13.0;
                        let y2 = rr.top() + 29.0;
                        let num_r = p.text(
                            egui::pos2(rr.left() + 10.0, y1),
                            Align2::LEFT_CENTER,
                            format!("#{}", pr.number),
                            FontId::proportional(12.0),
                            Color32::from_rgb(0x61, 0xaf, 0xef),
                        );
                        let clip = Rect::from_min_max(
                            rr.min,
                            egui::pos2(rr.right() - 8.0, rr.bottom()),
                        );
                        let pc = ui.painter().with_clip_rect(clip);
                        pc.text(
                            egui::pos2(num_r.right() + 8.0, y1),
                            Align2::LEFT_CENTER,
                            &pr.title,
                            FontId::proportional(12.5),
                            C_TEXT,
                        );
                        pc.text(
                            egui::pos2(rr.left() + 10.0, y2),
                            Align2::LEFT_CENTER,
                            format!("{} · {}", pr.author, pr.head_branch),
                            FontId::proportional(10.5),
                            C_TEXT_DIM,
                        );
                        if resp
                            .on_hover_text(format!("#{} {}", pr.number, pr.title))
                            .clicked()
                        {
                            chosen = Some(pr.number);
                        }
                    }
                    if self.ws.pr_list.is_empty() && self.ws.pr_list_rx.is_none() {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new("표시할 열린 PR이 없습니다")
                                    .color(C_TEXT_DIM)
                                    .size(11.5),
                            );
                        });
                    }
                    ui.add_space(6.0);
                });

            if go && !loading {
                match self.ws.review_input.trim().parse::<u64>() {
                    Ok(n) => chosen = Some(n),
                    Err(_) => self.ws.review_err = Some("숫자 PR 번호를 입력하세요".into()),
                }
            }
            if let Some(n) = chosen.filter(|_| !loading) {
                self.ws.review_input = n.to_string();
                let ctx = ui.ctx().clone();
                if self.ws.review_worktree {
                    self.start_review_worktree(&ctx, n);
                } else {
                    self.start_review(&ctx, n);
                }
            }
            return;
        }

        // Session view. Reads first, mutations deferred to the end.
        let root = self.ws.project_root.clone().unwrap_or_default();
        let active_rel: Option<String> = self
            .ws
            .active
            .and_then(|i| self.ws.tabs.get(i))
            .filter(|t| t.is_review)
            .and_then(|t| t.path.strip_prefix(&root).ok())
            .map(|p| p.to_string_lossy().to_string());
        let diff_open: Option<String> = self
            .ws
            .diff_view
            .as_ref()
            .filter(|d| d.commit_short.starts_with("PR #"))
            .map(|d| d.file.clone());
        let open_file_idx: Option<usize>;
        let open_diff_idx: Option<usize>;
        let toggle_viewed: Option<usize>;
        let toggle_dir: Option<String>;
        let mut close = false;
        let mut reload: Option<u64> = None;
        let mut to_worktree: Option<u64> = None;
        let mut del_comment: Option<usize> = None;
        let mut go_comment: Option<(usize, usize)> = None; // (file idx, display line)

        {
            let sess = self.ws.review.as_ref().unwrap();
            let pr = &sess.pr;
            let (viewed, total) = (sess.viewed_count(), sess.files.len());

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(format!("#{}", pr.number))
                        .color(Color32::from_rgb(0x61, 0xaf, 0xef))
                        .strong()
                        .size(13.0),
                );
                if !pr.author.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("by {}", pr.author))
                            .color(C_TEXT_DIM)
                            .size(11.5),
                    );
                }
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(&pr.title).color(C_TEXT).size(12.5))
                        .wrap(),
                );
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                let branches = if pr.head_branch.is_empty() {
                    pr.base_branch.clone()
                } else {
                    format!("{} ← {}", pr.base_branch, pr.head_branch)
                };
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(branches).color(C_TEXT_DIM).size(11.0),
                    )
                    .truncate(),
                );
            });
            if !sess.head_is_workdir {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("파일은 PR head 시점 내용 (체크아웃 없음)")
                            .color(C_TEXT_DIM)
                            .italics()
                            .size(10.5),
                    );
                });
            }

            // Progress: n/m viewed + a thin bar.
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(format!("검토 {viewed}/{total}"))
                        .color(C_TEXT_DIM)
                        .size(11.5),
                );
                let w = (ui.available_width() - 14.0).max(20.0);
                let (br, _) = ui.allocate_exact_size(Vec2::new(w, 4.0), Sense::hover());
                let br = Rect::from_center_size(br.center(), Vec2::new(w, 4.0));
                ui.painter().rect_filled(br, 2.0, C_BORDER);
                if total > 0 && viewed > 0 {
                    let f = viewed as f32 / total as f32;
                    ui.painter().rect_filled(
                        Rect::from_min_size(br.min, Vec2::new(br.width() * f, 4.0)),
                        2.0,
                        Color32::from_rgb(0x62, 0xb5, 0x43),
                    );
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                if loading {
                    ui.spinner();
                } else {
                    if ui.small_button("다시 로드").clicked() {
                        reload = Some(pr.number);
                    }
                    if !sess.head_is_workdir
                        && ui
                            .small_button("워크트리에서 열기")
                            .on_hover_text(
                                "PR을 별도 워크트리에 체크아웃해 프로젝트 전체를 PR 시점으로 엽니다",
                            )
                            .clicked()
                    {
                        to_worktree = Some(pr.number);
                    }
                }
                if ui.small_button("세션 종료").clicked() {
                    close = true;
                }
            });
            ui.add_space(6.0);

            // Pending line comments — submitted together with the review.
            if !sess.pending.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(format!("코멘트 {}개 (제출 대기)", sess.pending.len()))
                            .color(Color32::from_rgb(0xe5, 0xc0, 0x7b))
                            .size(11.5),
                    );
                });
                for (ci, c) in sess.pending.iter().enumerate() {
                    let (rr, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 20.0),
                        Sense::click(),
                    );
                    let p = ui.painter();
                    if resp.hovered() {
                        p.rect_filled(rr, 0.0, C_HOVER);
                    }
                    let file = c.path.rsplit('/').next().unwrap_or(&c.path);
                    let first = c.body.lines().next().unwrap_or("");
                    let clip = Rect::from_min_max(rr.min, egui::pos2(rr.right() - 24.0, rr.bottom()));
                    let pc = ui.painter().with_clip_rect(clip);
                    pc.text(
                        egui::pos2(rr.left() + 12.0, rr.center().y),
                        Align2::LEFT_CENTER,
                        format!("{file}:{}  {first}", c.line),
                        FontId::proportional(11.5),
                        C_TEXT_DIM,
                    );
                    let xr = Rect::from_center_size(
                        egui::pos2(rr.right() - 13.0, rr.center().y),
                        Vec2::splat(14.0),
                    );
                    let xresp = ui.interact(xr, ui.id().with(("rvw_cdel", ci)), Sense::click());
                    if xresp.hovered() {
                        ui.painter().rect_filled(xr, 3.0, C_HOVER.gamma_multiply(1.4));
                    }
                    draw_x(ui.painter(), xr, if xresp.hovered() { C_TEXT } else { C_TEXT_DIM });
                    if xresp.on_hover_text("코멘트 삭제").clicked() {
                        del_comment = Some(ci);
                    } else if resp.on_hover_text(&c.body).clicked() {
                        if let Some(fi) = sess.files.iter().position(|f| f.path == c.path) {
                            go_comment = Some((fi, c.disp_line));
                        }
                    }
                }
                ui.add_space(4.0);
            }

            // Changed files as the repo's real hierarchy: collapsible dirs
            // (single-child chains merged), files beneath.
            let tree = build_review_tree(&sess.files);
            let mut acts = RvActs::default();
            egui::ScrollArea::vertical()
                .id_salt(("review_sidebar", self.ws.seq))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    review_tree_rows(
                        ui,
                        &tree,
                        0,
                        &sess.files,
                        &self.ws.review_collapsed,
                        active_rel.as_deref(),
                        diff_open.as_deref(),
                        &self.icons,
                        &mut acts,
                    );
                    ui.add_space(6.0);
                });
            toggle_viewed = acts.toggle_viewed;
            open_file_idx = acts.open_file;
            open_diff_idx = acts.open_diff;
            toggle_dir = acts.toggle_dir;
        }

        // Deferred mutations.
        if let Some(k) = toggle_dir {
            if !self.ws.review_collapsed.remove(&k) {
                self.ws.review_collapsed.insert(k);
            }
        }
        if let Some(i) = toggle_viewed {
            if let Some(s) = self.ws.review.as_mut() {
                if let Some(f) = s.files.get_mut(i) {
                    f.viewed = !f.viewed;
                }
            }
        }
        if let Some(i) = open_file_idx {
            self.open_review_file(i);
        }
        if let Some(i) = open_diff_idx {
            self.open_review_diff(i);
        }
        if let Some(ci) = del_comment {
            if let Some(s) = self.ws.review.as_mut() {
                if ci < s.pending.len() {
                    s.pending.remove(ci);
                }
            }
        }
        if let Some((fi, disp)) = go_comment {
            self.open_review_file(fi);
            self.set_jump_target(disp);
        }
        if let Some(n) = reload {
            let ctx = ui.ctx().clone();
            self.start_review(&ctx, n);
        }
        if let Some(n) = to_worktree {
            let ctx = ui.ctx().clone();
            self.start_review_worktree(&ctx, n);
        }
        if close {
            self.ws.review = None;
            self.ws.review_err = None;
            let active_path = self
                .ws
                .active
                .and_then(|i| self.ws.tabs.get(i))
                .map(|t| t.path.clone());
            self.ws.tabs.retain(|t| !t.is_review);
            self.ws.active = active_path
                .and_then(|p| self.ws.tabs.iter().position(|t| t.path == p))
                .or(if self.ws.tabs.is_empty() { None } else { Some(0) });
            if self
                .ws
                .diff_view
                .as_ref()
                .is_some_and(|d| d.commit_short.starts_with("PR #"))
            {
                self.ws.diff_view = None;
            }
            // Reviewing in a dedicated worktree: closing the session also
            // tears the worktree down (checkout copy only) and returns to
            // the original project — no leftovers on disk.
            if let Some(root) = self.ws.project_root.clone() {
                if crate::review::is_managed_worktree(&root) {
                    let main = crate::git::main_worktree_root(&root).filter(|m| *m != root);
                    let ctx = ui.ctx().clone();
                    self.close_project(&root);
                    if let Some(m) = main {
                        self.open_project(&ctx, m);
                    }
                    self.ws.status = "리뷰 워크트리 정리 중…".to_string();
                    std::thread::spawn(move || {
                        let _ = crate::review::remove_worktree(&root);
                    });
                }
            }
        }
    }

    /// AI-Watch panel: live feed of outside edits (an agent in a terminal,
    /// another editor), with follow mode to watch the work happen.
    fn activity_sidebar(&mut self, ui: &mut egui::Ui) {
        self.left_header(ui, "AI WATCH", false);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.checkbox(&mut self.follow_ai, "");
            ui.label(
                egui::RichText::new("팔로우 모드 — 바뀐 곳으로 자동 이동")
                    .color(C_TEXT_DIM)
                    .size(11.5),
            );
        });
        let mut clear = false;
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("터미널 AI 등 외부 편집을 실시간 감지")
                    .color(C_TEXT_DIM)
                    .size(10.5),
            );
            if !self.ws.feed.is_empty() && ui.small_button("비우기").clicked() {
                clear = true;
            }
        });
        ui.add_space(6.0);

        let now = unix_now();
        let mut open: Option<PathBuf> = None;
        egui::ScrollArea::vertical()
            .id_salt(("activity_feed", self.ws.seq))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if self.ws.feed.is_empty() {
                    ui.add_space(16.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("아직 감지된 외부 변경이 없습니다")
                                .color(C_TEXT_DIM)
                                .size(11.5),
                        );
                    });
                }
                for (i, e) in self.ws.feed.iter().enumerate() {
                    let (rr, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 34.0),
                        Sense::click(),
                    );
                    let p = ui.painter();
                    if resp.hovered() {
                        p.rect_filled(rr, 0.0, C_HOVER);
                    }
                    let (letter, col) = match e.kind {
                        1 => ("A", Color32::from_rgb(0x62, 0xb5, 0x43)),
                        2 => ("D", Color32::from_rgb(0xe0, 0x6c, 0x75)),
                        _ => ("M", Color32::from_rgb(0x6c, 0x9c, 0xd2)),
                    };
                    let y1 = rr.top() + 11.0;
                    let y2 = rr.top() + 25.0;
                    p.text(
                        egui::pos2(rr.left() + 10.0, y1),
                        Align2::LEFT_CENTER,
                        letter,
                        FontId::monospace(11.5),
                        col,
                    );
                    // Right-aligned "n초 전".
                    let when = rel_time(now - e.at);
                    let wg = p.layout_no_wrap(when, FontId::proportional(10.5), C_TEXT_DIM);
                    p.galley(
                        egui::pos2(rr.right() - 8.0 - wg.size().x, y1 - wg.size().y / 2.0),
                        wg.clone(),
                        C_TEXT_DIM,
                    );
                    let name = e.rel.rsplit('/').next().unwrap_or(&e.rel);
                    let dir = e.rel[..e.rel.len() - name.len()].trim_end_matches('/');
                    let clip = Rect::from_min_max(
                        rr.min,
                        egui::pos2(rr.right() - 14.0 - wg.size().x, rr.bottom()),
                    );
                    let pc = ui.painter().with_clip_rect(clip);
                    pc.text(
                        egui::pos2(rr.left() + 26.0, y1),
                        Align2::LEFT_CENTER,
                        name,
                        FontId::proportional(12.5),
                        C_TEXT,
                    );
                    let pd = ui.painter().with_clip_rect(Rect::from_min_max(
                        rr.min,
                        egui::pos2(rr.right() - 8.0, rr.bottom()),
                    ));
                    pd.text(
                        egui::pos2(rr.left() + 26.0, y2),
                        Align2::LEFT_CENTER,
                        dir,
                        FontId::proportional(10.5),
                        C_TEXT_DIM,
                    );
                    let _ = i;
                    if e.kind != 2 && resp.on_hover_text(&e.rel).clicked() {
                        open = Some(e.path.clone());
                    }
                }
                ui.add_space(6.0);
            });

        if clear {
            self.ws.feed.clear();
        }
        if let Some(p) = open {
            // A deliberate click IS a navigation point (unlike follow mode).
            let line = self
                .ws
                .project_root
                .as_ref()
                .and_then(|r| crate::git::file_line_changes(r, &p))
                .and_then(|d| d.changed.last().map(|&(l, _)| l))
                .unwrap_or(0);
            self.record_origin();
            self.open_file(p.clone());
            self.set_jump_target(line);
            self.record_nav(p, line);
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
                                                .pos_from_cursor(egui::text::CCursor::new(ci));
                                            let b = c_galley
                                                .pos_from_cursor(egui::text::CCursor::new(ci + len));
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
                                                .pos_from_cursor(egui::text::CCursor::new(focus_ci));
                                            let b = c_galley.pos_from_cursor(
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
                self.review_bar(ui);
                self.editor(ui, bg);
            });
    }

    /// Slim review header above the editor for PR tabs (IntelliJ diff-header
    /// style): jump to the source file, step through changes, and mark the
    /// file viewed / move on — the whole review loop without the panel.
    fn review_bar(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.ws.active.filter(|&i| i < self.ws.tabs.len()) else {
            return;
        };
        if !self.ws.tabs[idx].is_review {
            return;
        }
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let Ok(rel) = self.ws.tabs[idx]
            .path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().to_string())
        else {
            return;
        };
        let Some(sess) = self.ws.review.as_ref() else {
            return;
        };
        let pr_no = sess.pr.number;
        let fi = sess.files.iter().position(|f| f.path == rel);
        let (viewed, added, deleted) = fi
            .map(|i| {
                let f = &sess.files[i];
                (f.viewed, f.added, f.deleted)
            })
            .unwrap_or((false, 0, 0));
        let (viewed_files, total_files) = (sess.viewed_count(), sess.files.len());

        let blocks = change_block_starts(&self.ws.tabs[idx].git_changes);
        let cur = self.ws.tabs[idx]
            .caret_line
            .or(self.ws.tabs[idx].flash_line)
            .unwrap_or(0);
        let k = blocks.partition_point(|&b| b <= cur);
        let up_tip = format!("이전 변경 ({})", self.keymap.text(crate::keymap::Action::PrevChange));
        let down_tip = format!("다음 변경 ({})", self.keymap.text(crate::keymap::Action::NextChange));

        let n_comments = sess.pending.len();
        let mut dir = 0isize;
        let mut to_source = false;
        let mut flip_viewed = false;
        let mut next_file = false;
        let mut add_comment = false;
        let mut open_submit = false;

        let resp = egui::Frame::default()
            .fill(C_PANEL)
            .inner_margin(egui::Margin::symmetric(10, 3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        egui::RichText::new(format!("PR #{pr_no}"))
                            .color(Color32::from_rgb(0xc5, 0x95, 0xff))
                            .size(11.5),
                    );
                    sep_dot(ui);
                    if crosshair_button(ui, "원본 파일로 이동 (작업 트리의 이 파일)") {
                        to_source = true;
                    }
                    sep_dot(ui);
                    if chevron_button(ui, false, &up_tip) {
                        dir = -1;
                    }
                    if chevron_button(ui, true, &down_tip) {
                        dir = 1;
                    }
                    ui.label(
                        egui::RichText::new(format!("변경 {k}/{}", blocks.len()))
                            .color(C_TEXT_DIM)
                            .size(11.5),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("+{added}"))
                            .color(Color32::from_rgb(0x62, 0xb5, 0x43))
                            .size(11.0),
                    );
                    ui.label(
                        egui::RichText::new(format!("−{deleted}"))
                            .color(Color32::from_rgb(0xe0, 0x6c, 0x75))
                            .size(11.0),
                    );
                    sep_dot(ui);
                    let c_label = if n_comments > 0 {
                        format!("코멘트 {n_comments}")
                    } else {
                        "코멘트".to_string()
                    };
                    if ui
                        .small_button(c_label)
                        .on_hover_text("커서 라인에 리뷰 코멘트 달기 (제출 시 함께 전송)")
                        .clicked()
                    {
                        add_comment = true;
                    }

                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let all_done = viewed_files == total_files && total_files > 0;
                        let submit = egui::Button::new(
                            egui::RichText::new("리뷰 제출").size(11.5).color(
                                if all_done {
                                    Color32::from_rgb(0x1e, 0x1f, 0x22)
                                } else {
                                    C_TEXT
                                },
                            ),
                        )
                        .fill(if all_done {
                            Color32::from_rgb(0x62, 0xb5, 0x43)
                        } else {
                            C_HOVER
                        });
                        if ui
                            .add(submit)
                            .on_hover_text("승인 / 코멘트 / 변경 요청을 GitHub에 제출")
                            .clicked()
                        {
                            open_submit = true;
                        }
                        if fi.is_some() {
                            if ui
                                .small_button("다음 파일")
                                .on_hover_text("이 파일을 검토됨으로 표시하고 다음 미검토 파일 열기")
                                .clicked()
                            {
                                next_file = true;
                            }
                            let label = if viewed { "검토됨" } else { "검토 완료" };
                            let btn = egui::Button::new(
                                egui::RichText::new(label)
                                    .size(11.5)
                                    .color(if viewed {
                                        Color32::from_rgb(0x1e, 0x1f, 0x22)
                                    } else {
                                        C_TEXT
                                    }),
                            )
                            .fill(if viewed {
                                Color32::from_rgb(0x62, 0xb5, 0x43)
                            } else {
                                C_HOVER
                            });
                            if ui.add(btn).on_hover_text("검토 상태 토글").clicked() {
                                flip_viewed = true;
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!("{viewed_files}/{total_files} 파일"))
                                .color(C_TEXT_DIM)
                                .size(11.0),
                        );
                    });
                });
            })
            .response;
        // Hairline between the bar and the code.
        ui.painter().line_segment(
            [
                egui::pos2(resp.rect.left(), resp.rect.bottom() - 0.5),
                egui::pos2(resp.rect.right(), resp.rect.bottom() - 0.5),
            ],
            Stroke::new(1.0, C_BORDER),
        );

        if dir != 0 {
            self.jump_change(dir);
        }
        if to_source {
            let ctx = ui.ctx().clone();
            self.open_source_file(&ctx);
        }
        if flip_viewed {
            if let (Some(s), Some(i)) = (self.ws.review.as_mut(), fi) {
                s.files[i].viewed = !s.files[i].viewed;
            }
        }
        if next_file {
            let mut open_next: Option<usize> = None;
            if let (Some(s), Some(i)) = (self.ws.review.as_mut(), fi) {
                s.files[i].viewed = true;
                let n = s.files.len();
                open_next = (1..n)
                    .map(|d| (i + d) % n)
                    .find(|&j| !s.files[j].viewed);
            }
            match open_next {
                Some(j) => self.open_review_file(j),
                None => {
                    self.ws.status =
                        "모든 파일 검토 완료 — [리뷰 제출]로 마무리하세요".to_string();
                    self.ws.submit_open = true;
                }
            }
        }
        if add_comment {
            let t = &self.ws.tabs[idx];
            match t.caret_line.or(t.flash_line) {
                None => self.ws.status = "코멘트할 라인을 먼저 클릭하세요".to_string(),
                Some(disp) => match t.head_line_of(disp) {
                    None => {
                        self.ws.status =
                            "삭제된(고스트) 라인에는 코멘트를 달 수 없습니다".to_string()
                    }
                    Some(hl) => {
                        self.ws.comment_open = true;
                        self.ws.comment_path = rel.clone();
                        self.ws.comment_line = hl;
                        self.ws.comment_disp = disp;
                        self.ws.comment_text.clear();
                        self.ws.comment_focus = true;
                    }
                },
            }
        }
        if open_submit {
            self.ws.submit_open = true;
        }
    }

    /// Line-comment composer: writes into the session's pending list; the
    /// comments go to GitHub together with the review submission.
    fn comment_window(&mut self, ctx: &egui::Context) {
        if !self.ws.comment_open {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("라인 코멘트")
            .id(egui::Id::new(("comment_win", self.ws.seq)))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(440.0)
            .default_pos(ctx.screen_rect().center() - Vec2::new(220.0, 140.0))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{}:{}",
                        self.ws.comment_path, self.ws.comment_line
                    ))
                    .color(C_TEXT_DIM)
                    .size(11.5),
                );
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut self.ws.comment_text)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .hint_text("리뷰 코멘트…"),
                );
                if self.ws.comment_focus {
                    resp.request_focus();
                    self.ws.comment_focus = false;
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("추가").clicked() {
                        save = true;
                    }
                    if ui.button("취소").clicked() {
                        cancel = true;
                    }
                    ui.label(
                        egui::RichText::new("제출 전까지 로컬에만 보관됩니다")
                            .color(C_TEXT_DIM)
                            .size(10.5),
                    );
                });
            });
        if save && !self.ws.comment_text.trim().is_empty() {
            let c = crate::review::PendingComment {
                path: self.ws.comment_path.clone(),
                line: self.ws.comment_line,
                disp_line: self.ws.comment_disp,
                body: self.ws.comment_text.trim().to_string(),
            };
            if let Some(s) = self.ws.review.as_mut() {
                s.pending.push(c);
                self.ws.status = format!("코멘트 추가 (대기 {}개)", s.pending.len());
            }
            self.ws.comment_open = false;
        }
        if cancel || !open {
            self.ws.comment_open = false;
        }
    }

    /// Review submission: approve / comment / request-changes with a summary
    /// body, sending the queued line comments along.
    fn submit_window(&mut self, ctx: &egui::Context) {
        if !self.ws.submit_open {
            return;
        }
        let Some((pr_no, viewed, total, n_comments)) = self
            .ws
            .review
            .as_ref()
            .map(|s| (s.pr.number, s.viewed_count(), s.files.len(), s.pending.len()))
        else {
            self.ws.submit_open = false;
            return;
        };
        let mut open = true;
        let mut do_submit = false;
        egui::Window::new("리뷰 제출")
            .id(egui::Id::new(("submit_win", self.ws.seq)))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(480.0)
            .default_pos(ctx.screen_rect().center() - Vec2::new(240.0, 170.0))
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "PR #{pr_no} · 검토 {viewed}/{total} 파일 · 라인 코멘트 {n_comments}개 포함"
                    ))
                    .color(C_TEXT_DIM)
                    .size(11.5),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    for (i, label) in ["코멘트만", "승인 (Approve)", "변경 요청"].iter().enumerate()
                    {
                        if ui
                            .selectable_label(self.ws.submit_event == i, *label)
                            .clicked()
                        {
                            self.ws.submit_event = i;
                        }
                    }
                });
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(&mut self.ws.submit_body)
                        .desired_rows(5)
                        .desired_width(f32::INFINITY)
                        .hint_text("리뷰 총평 — 승인은 비워도 됩니다"),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if self.ws.submit_rx.is_some() {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("제출 중…").color(C_TEXT_DIM).size(11.5),
                        );
                    } else if ui.button("GitHub에 제출").clicked() {
                        do_submit = true;
                    }
                });
            });
        if !open {
            self.ws.submit_open = false;
        }
        if do_submit {
            self.do_submit(ctx);
        }
    }

    fn do_submit(&mut self, ctx: &egui::Context) {
        if self.ws.submit_rx.is_some() {
            return;
        }
        let Some(sess) = self.ws.review.as_ref() else {
            return;
        };
        let Some(root) = self.ws.project_root.clone() else {
            return;
        };
        let event = match self.ws.submit_event {
            1 => "APPROVE",
            2 => "REQUEST_CHANGES",
            _ => "COMMENT",
        };
        // GitHub rejects an empty COMMENT / REQUEST_CHANGES review.
        if event != "APPROVE" && self.ws.submit_body.trim().is_empty() && sess.pending.is_empty()
        {
            self.ws.status = "본문 또는 라인 코멘트가 필요합니다".to_string();
            return;
        }
        let number = sess.pr.number;
        let head = sess.head_id.clone();
        let body = self.ws.submit_body.trim().to_string();
        let comments = sess.pending.clone();
        let event = event.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let c = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::review::submit_review(
                &root, number, &head, &event, &body, &comments,
            ));
            c.request_repaint();
        });
        self.ws.submit_rx = Some(rx);
        self.ws.status = "리뷰 제출 중…".to_string();
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
                    let mut layouter = move |ui: &egui::Ui, _buf: &dyn egui::TextBuffer, _w: f32| {
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

                                let is_review = self.ws.tabs[i].is_review;
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
                                let badge_w = if is_review { 22.0 } else { 0.0 };
                                let tab_w =
                                    16.0 + 16.0 + 6.0 + text_w + badge_w + 8.0 + 16.0 + 12.0;
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
                                let name_r = p.text(
                                    egui::pos2(icon.right() + 6.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    &name,
                                    FontId::proportional(13.0),
                                    if selected { C_TEXT } else { C_TEXT_DIM },
                                );
                                // Review tabs show PR-head content, not disk.
                                if is_review {
                                    p.text(
                                        egui::pos2(name_r.right() + 5.0, rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        "PR",
                                        FontId::proportional(9.5),
                                        Color32::from_rgb(0xc5, 0x95, 0xff),
                                    );
                                }

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

        // Markdown in rendered mode: draw the document, not the source.
        if self.ws.tabs[idx].md_preview {
            self.md_view(ui, idx);
            return;
        }

        let cmd_held = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        let gutter_color = Color32::from_rgb(0x4b, 0x50, 0x58);
        let font = FontId::monospace(self.font_size);

        // Build (or reuse cached) gutter + code width — only rebuilt on font change.
        let ctx = ui.ctx().clone();
        self.ws.tabs[idx].ensure_render(&ctx, self.font_size);
        let pane = ui.max_rect();

        // Pending review comments on this file (display rows, for markers).
        let comment_rows: Vec<usize> = if self.ws.tabs[idx].is_review {
            let rel = self
                .ws
                .project_root
                .as_ref()
                .and_then(|r| self.ws.tabs[idx].path.strip_prefix(r).ok())
                .map(|p| p.to_string_lossy().to_string());
            match (rel, self.ws.review.as_ref()) {
                (Some(rel), Some(s)) => s
                    .pending
                    .iter()
                    .filter(|c| c.path == rel)
                    .map(|c| c.disp_line)
                    .collect(),
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // Layouter feeds the cached highlighted job to the TextEdit.
        let job_for_layouter = self.ws.tabs[idx].job.clone();
        let mut layouter = move |ui: &egui::Ui, _buf: &dyn egui::TextBuffer, _w: f32| {
            ui.fonts(|f| f.layout_job(job_for_layouter.clone()))
        };

        let scroll_target = self.ws.tabs[idx].scroll_to_line;
        let mut goto: Option<String> = None;
        let mut clear_scroll = false;
        let mut clicked_nothing = false;
        let mut caret_clicked: Option<usize> = None;
        let mut comment_at: Option<usize> = None;

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
                        // Deletion markers: a small gray triangle at the
                        // boundary. Review tabs show ghost rows instead.
                        if !tab.is_review {
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
                        let ci = cur.primary.index;
                        let row = ((out.galley.pos_from_cursor(cur.primary).top()
                            / row_h.max(1.0))
                            .round()
                            .max(0.0)) as usize;
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

                    // PR-review overlay: the diff painted over the COMPLETE
                    // file — full-width bands on changed lines (green =
                    // added, blue = modified), red seams where lines were
                    // deleted.
                    if tab.is_review {
                        let p = ui.painter();
                        for &(line, kind) in &tab.git_changes.changed {
                            let y = out.galley_pos.y + row_h * line as f32;
                            let col = match kind {
                                crate::git::LineChange::Added => {
                                    Color32::from_rgba_unmultiplied(0x62, 0xb5, 0x43, 26)
                                }
                                crate::git::LineChange::Modified => {
                                    Color32::from_rgba_unmultiplied(0x6c, 0x9c, 0xd2, 24)
                                }
                            };
                            p.rect_filled(
                                Rect::from_min_max(
                                    egui::pos2(band_left, y),
                                    egui::pos2(band_right, y + row_h),
                                ),
                                0.0,
                                col,
                            );
                        }
                        // Ghost rows (PR-deleted code, struck through in the
                        // text): a reddish band marks them as not-in-head.
                        for &row in &tab.ghost_rows {
                            let y = out.galley_pos.y + row_h * row as f32;
                            p.rect_filled(
                                Rect::from_min_max(
                                    egui::pos2(band_left, y),
                                    egui::pos2(band_right, y + row_h),
                                ),
                                0.0,
                                Color32::from_rgba_unmultiplied(0xe0, 0x6c, 0x75, 20),
                            );
                        }
                        // Pending-comment lines: a soft yellow band.
                        for &row in &comment_rows {
                            let y = out.galley_pos.y + row_h * row as f32;
                            p.rect_filled(
                                Rect::from_min_max(
                                    egui::pos2(band_left, y),
                                    egui::pos2(band_right, y + row_h),
                                ),
                                0.0,
                                Color32::from_rgba_unmultiplied(0xe5, 0xc0, 0x7b, 18),
                            );
                        }
                        // Fallback seam when a deletion has no ghost text
                        // (e.g. undecodable content).
                        if tab.ghost_rows.is_empty() {
                            for &line in &tab.git_changes.deleted_before {
                                let y = out.galley_pos.y + row_h * line as f32;
                                p.line_segment(
                                    [egui::pos2(band_left, y), egui::pos2(band_right, y)],
                                    Stroke::new(
                                        1.5,
                                        Color32::from_rgba_unmultiplied(0xe0, 0x6c, 0x75, 150),
                                    ),
                                );
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

                    // Review tabs: hovering a line shows a "+" on its gutter
                    // number (GitHub-style) — click to comment that line.
                    if tab.is_review {
                        if let Some(pos) = ui.ctx().pointer_hover_pos() {
                            let row = ((pos.y - out.galley_pos.y) / row_h).floor();
                            if ui.clip_rect().contains(pos)
                                && pos.x >= band_left
                                && row >= 0.0
                                && (row as usize) < tab.line_count
                            {
                                let row = row as usize;
                                let y = out.galley_pos.y + row_h * row as f32;
                                let cr = Rect::from_center_size(
                                    egui::pos2(g_rect.right() - 7.0, y + row_h / 2.0),
                                    Vec2::splat(14.0),
                                );
                                let resp = ui.interact(
                                    cr,
                                    ui.id().with(("cmt_add", row)),
                                    Sense::click(),
                                );
                                let p = ui.painter();
                                let bg = if resp.hovered() {
                                    C_ACCENT
                                } else {
                                    C_ACCENT.gamma_multiply(0.72)
                                };
                                p.rect_filled(cr, 3.0, bg);
                                let c = cr.center();
                                let st = Stroke::new(1.6, Color32::WHITE);
                                p.line_segment(
                                    [c + Vec2::new(-3.5, 0.0), c + Vec2::new(3.5, 0.0)],
                                    st,
                                );
                                p.line_segment(
                                    [c + Vec2::new(0.0, -3.5), c + Vec2::new(0.0, 3.5)],
                                    st,
                                );
                                if resp.hovered() {
                                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                }
                                if resp.on_hover_text("이 라인에 코멘트").clicked() {
                                    comment_at = Some(row);
                                }
                            }
                        }
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
                            .map(|r| r.primary.index)
                            .or_else(|| {
                                out.response.interact_pointer_pos().map(|p| {
                                    out.galley.cursor_from_pos(p - out.galley_pos).index
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

        // Overview ruler (IntelliJ error-stripe style): every change's
        // position over the WHOLE file, painted along the right edge so the
        // scrollbar area shows where the changes live.
        {
            let t = &self.ws.tabs[idx];
            let has_marks = !t.git_changes.changed.is_empty()
                || !t.ghost_rows.is_empty()
                || !t.git_changes.deleted_before.is_empty()
                || !comment_rows.is_empty();
            if has_marks {
                let p = ui.painter();
                let total = t.line_count.max(1) as f32;
                let (x1, x2) = (pane.right() - 6.0, pane.right() - 1.0);
                let mh = (pane.height() / total).max(2.0);
                let y_of = |line: usize| pane.top() + pane.height() * (line as f32 / total);
                let mark = |line: usize, col: Color32| {
                    let y = y_of(line);
                    p.rect_filled(
                        Rect::from_min_max(egui::pos2(x1, y), egui::pos2(x2, y + mh)),
                        0.0,
                        col,
                    );
                };
                for &(line, kind) in &t.git_changes.changed {
                    let col = match kind {
                        crate::git::LineChange::Added => {
                            Color32::from_rgba_unmultiplied(0x62, 0xb5, 0x43, 190)
                        }
                        crate::git::LineChange::Modified => {
                            Color32::from_rgba_unmultiplied(0x6c, 0x9c, 0xd2, 190)
                        }
                    };
                    mark(line, col);
                }
                let del = Color32::from_rgba_unmultiplied(0xe0, 0x6c, 0x75, 190);
                if t.is_review {
                    for &row in &t.ghost_rows {
                        mark(row, del);
                    }
                } else {
                    for &line in &t.git_changes.deleted_before {
                        mark(line, del);
                    }
                }
                for &row in &comment_rows {
                    mark(row, Color32::from_rgba_unmultiplied(0xe5, 0xc0, 0x7b, 210));
                }
            }
        }

        // Every plain click is a navigation point (IntelliJ-style): Back walks
        // through previous click positions, not only through jumps. Clicks on
        // nearby lines refine the current entry instead of flooding history.
        if let Some(line) = caret_clicked {
            if !ctx.input(|i| i.modifiers.command || i.modifiers.ctrl) {
                self.note_click_nav(line);
            }
        }

        // Gutter "+" clicked: open the comment composer for that line.
        if let Some(row) = comment_at {
            let root = self.ws.project_root.clone().unwrap_or_default();
            let t = &mut self.ws.tabs[idx];
            t.caret_line = Some(row);
            match t.head_line_of(row) {
                None => {
                    self.ws.status =
                        "삭제된(고스트) 라인에는 코멘트를 달 수 없습니다".to_string()
                }
                Some(hl) => {
                    if let Ok(rel) = t.path.strip_prefix(&root) {
                        self.ws.comment_path = rel.to_string_lossy().to_string();
                        self.ws.comment_line = hl;
                        self.ws.comment_disp = row;
                        self.ws.comment_text.clear();
                        self.ws.comment_open = true;
                        self.ws.comment_focus = true;
                    }
                }
            }
        }

        if is_md_path(&self.ws.tabs[idx].path) {
            self.md_toggle(ui, pane, idx);
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

/// Merge PR-deleted lines back into the head document as inline "ghost"
/// rows, so a reviewer sees WHAT was removed in place. Returns the merged
/// doc, the overlay remapped to display lines (with ghost group starts in
/// `deleted_before` for change navigation), the ghost row indices, and the
/// ghost byte ranges (for strikethrough styling).
fn build_ghost_doc(
    head: &str,
    overlay: &crate::git::FileDiff,
    ghosts: &[(usize, Vec<String>)],
) -> (String, crate::git::FileDiff, Vec<usize>, Vec<(usize, usize)>) {
    if ghosts.is_empty() {
        return (head.to_string(), overlay.clone(), Vec::new(), Vec::new());
    }
    let head_lines: Vec<&str> = head.lines().collect();
    let mut sorted: Vec<(usize, &Vec<String>)> = ghosts.iter().map(|(a, v)| (*a, v)).collect();
    sorted.sort_by_key(|g| g.0);

    let mut out = String::new();
    let mut ghost_rows = Vec::new();
    let mut ghost_ranges = Vec::new();
    let mut group_starts = Vec::new();
    // For head line h: how many ghost rows were inserted above it.
    let mut offset_at = vec![0usize; head_lines.len() + 1];
    let (mut gi, mut off, mut disp) = (0usize, 0usize, 0usize);
    for h in 0..=head_lines.len() {
        while gi < sorted.len() && sorted[gi].0.min(head_lines.len()) == h {
            group_starts.push(disp);
            for t in sorted[gi].1 {
                if !out.is_empty() {
                    out.push('\n');
                }
                let start = out.len();
                out.push_str(t);
                ghost_ranges.push((start, out.len()));
                ghost_rows.push(disp);
                disp += 1;
                off += 1;
            }
            gi += 1;
        }
        offset_at[h] = off;
        if h < head_lines.len() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(head_lines[h]);
            disp += 1;
        }
    }
    let changed = overlay
        .changed
        .iter()
        .map(|&(l, k)| (l + offset_at[l.min(head_lines.len())], k))
        .collect();
    let fd = crate::git::FileDiff {
        changed,
        deleted_before: group_starts,
    };
    (out, fd, ghost_rows, ghost_ranges)
}

/// Restyle a highlighted job so the ghost byte ranges render dimmed and
/// struck through. Sections are split at range boundaries; both lists are
/// sorted, ranges never overlap.
fn stylize_ghosts(job: &mut LayoutJob, ranges: &[(usize, usize)]) {
    if ranges.is_empty() {
        return;
    }
    let strike = Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xe0, 0x6c, 0x75, 170));
    let mut out: Vec<egui::text::LayoutSection> = Vec::with_capacity(job.sections.len() + ranges.len());
    for sec in &job.sections {
        let (s, e) = (sec.byte_range.start, sec.byte_range.end);
        let mut cur = s;
        for &(gs, ge) in ranges {
            if ge <= cur || gs >= e {
                continue;
            }
            let (a, b) = (gs.max(cur), ge.min(e));
            if a > cur {
                let mut plain = sec.clone();
                plain.byte_range = cur..a;
                out.push(plain);
            }
            let mut ghost = sec.clone();
            ghost.byte_range = a..b;
            ghost.format.color = ghost.format.color.gamma_multiply(0.55);
            ghost.format.strikethrough = strike;
            out.push(ghost);
            cur = b;
        }
        if cur < e || s == e {
            let mut rest = sec.clone();
            rest.byte_range = cur..e;
            out.push(rest);
        }
    }
    job.sections = out;
}

/// Directory node of the REVIEW panel's changed-file tree. Single-child
/// directory chains collapse into one "a/b/c" row (IntelliJ-style), so deep
/// monorepo paths stay one or two levels of indentation.
#[derive(Default)]
struct RvNode {
    name: String,
    /// Full relative dir path — the collapse-state id.
    key: String,
    children: Vec<RvNode>,
    /// Indices into `ReviewSession::files` located directly in this dir.
    files: Vec<usize>,
}

#[derive(Default)]
struct RvActs {
    open_file: Option<usize>,
    open_diff: Option<usize>,
    toggle_viewed: Option<usize>,
    toggle_dir: Option<String>,
}

fn build_review_tree(files: &[crate::review::ReviewFile]) -> RvNode {
    let mut root = RvNode::default();
    for (i, f) in files.iter().enumerate() {
        let mut comps: Vec<&str> = f.path.split('/').collect();
        comps.pop(); // file name
        let mut cur = &mut root;
        let mut key = String::new();
        for c in comps {
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(c);
            let pos = match cur.children.iter().position(|n| n.name == c) {
                Some(p) => p,
                None => {
                    cur.children.push(RvNode {
                        name: c.to_string(),
                        key: key.clone(),
                        ..Default::default()
                    });
                    cur.children.len() - 1
                }
            };
            cur = &mut cur.children[pos];
        }
        cur.files.push(i);
    }
    collapse_chains(&mut root);
    sort_tree(&mut root);
    root
}

fn collapse_chains(node: &mut RvNode) {
    for c in &mut node.children {
        collapse_chains(c);
    }
    for c in &mut node.children {
        while c.files.is_empty() && c.children.len() == 1 {
            let g = c.children.remove(0);
            c.name = format!("{}/{}", c.name, g.name);
            c.key = g.key;
            c.children = g.children;
            c.files = g.files;
        }
    }
}

fn sort_tree(node: &mut RvNode) {
    node.children.sort_by(|a, b| a.name.cmp(&b.name));
    for c in &mut node.children {
        sort_tree(c);
    }
}

fn count_rv_files(node: &RvNode) -> usize {
    node.files.len() + node.children.iter().map(count_rv_files).sum::<usize>()
}

/// Render the changed-file tree: directory rows (collapsible) first, then
/// this dir's files. Clicks land in `acts` and are applied by the caller.
#[allow(clippy::too_many_arguments)]
fn review_tree_rows(
    ui: &mut egui::Ui,
    node: &RvNode,
    depth: usize,
    files: &[crate::review::ReviewFile],
    collapsed: &std::collections::HashSet<String>,
    active_rel: Option<&str>,
    diff_open: Option<&str>,
    icons: &icons::IconSet,
    acts: &mut RvActs,
) {
    let indent = 8.0 + depth as f32 * 12.0;
    for c in &node.children {
        let (rr, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rr, 0.0, C_HOVER);
        }
        let open = !collapsed.contains(&c.key);
        let tc = C_TEXT_DIM;
        let cx = rr.left() + indent + 4.0;
        let cy = rr.center().y;
        let tri = if open {
            vec![
                egui::pos2(cx - 3.5, cy - 2.0),
                egui::pos2(cx + 3.5, cy - 2.0),
                egui::pos2(cx, cy + 3.0),
            ]
        } else {
            vec![
                egui::pos2(cx - 2.0, cy - 3.5),
                egui::pos2(cx - 2.0, cy + 3.5),
                egui::pos2(cx + 3.0, cy),
            ]
        };
        p.add(egui::Shape::convex_polygon(tri, tc, Stroke::NONE));
        let ir = Rect::from_center_size(egui::pos2(rr.left() + indent + 17.0, cy), Vec2::splat(14.0));
        icons.folder(p, ir, &c.name, open);
        let clip = Rect::from_min_max(rr.min, egui::pos2(rr.right() - 8.0, rr.bottom()));
        let pc = ui.painter().with_clip_rect(clip);
        let name_r = pc.text(
            egui::pos2(rr.left() + indent + 28.0, cy),
            Align2::LEFT_CENTER,
            &c.name,
            FontId::proportional(12.0),
            C_TEXT_DIM,
        );
        pc.text(
            egui::pos2(name_r.right() + 6.0, cy),
            Align2::LEFT_CENTER,
            count_rv_files(c).to_string(),
            FontId::proportional(10.5),
            C_TEXT_DIM.gamma_multiply(0.7),
        );
        if resp.on_hover_text(&c.key).clicked() {
            acts.toggle_dir = Some(c.key.clone());
        }
        if open {
            review_tree_rows(ui, c, depth + 1, files, collapsed, active_rel, diff_open, icons, acts);
        }
    }

    for &i in &node.files {
        let f = &files[i];
        let (rr, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 22.0), Sense::click());
        let is_open =
            active_rel == Some(f.path.as_str()) || diff_open == Some(f.path.as_str());
        {
            let p = ui.painter();
            if is_open {
                p.rect_filled(rr, 0.0, C_SEL);
            } else if resp.hovered() {
                p.rect_filled(rr, 0.0, C_HOVER);
            }
        }
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let cy = rr.center().y;
        let text_c = if f.viewed { C_TEXT_DIM } else { C_TEXT };
        let p = ui.painter();
        p.text(
            egui::pos2(rr.left() + indent + 2.0, cy),
            Align2::LEFT_CENTER,
            status_letter(f.status),
            FontId::monospace(11.5),
            status_color(f.status),
        );
        // Right-aligned counts, then the ± and viewed buttons.
        let counts_right = rr.right() - 52.0;
        let del_g = p.layout_no_wrap(
            format!("−{}", f.deleted),
            FontId::monospace(10.0),
            Color32::from_rgb(0xe0, 0x6c, 0x75),
        );
        p.galley(
            egui::pos2(counts_right - del_g.size().x, cy - del_g.size().y / 2.0),
            del_g.clone(),
            Color32::from_rgb(0xe0, 0x6c, 0x75),
        );
        let add_g = p.layout_no_wrap(
            format!("+{} ", f.added),
            FontId::monospace(10.0),
            Color32::from_rgb(0x62, 0xb5, 0x43),
        );
        p.galley(
            egui::pos2(
                counts_right - del_g.size().x - add_g.size().x,
                cy - add_g.size().y / 2.0,
            ),
            add_g.clone(),
            Color32::from_rgb(0x62, 0xb5, 0x43),
        );
        let name_clip = Rect::from_min_max(
            rr.min,
            egui::pos2(counts_right - del_g.size().x - add_g.size().x - 4.0, rr.bottom()),
        );
        let pc = ui.painter().with_clip_rect(name_clip);
        pc.text(
            egui::pos2(rr.left() + indent + 16.0, cy),
            Align2::LEFT_CENTER,
            name,
            FontId::proportional(12.5),
            if is_open { Color32::WHITE } else { text_c },
        );

        let diff_r = Rect::from_center_size(
            egui::pos2(rr.right() - 38.0, cy),
            Vec2::splat(16.0),
        );
        let check_r = Rect::from_center_size(
            egui::pos2(rr.right() - 15.0, cy),
            Vec2::splat(16.0),
        );
        let diff_resp = ui.interact(diff_r, ui.id().with(("rvw_diff", i)), Sense::click());
        let check_resp = ui.interact(check_r, ui.id().with(("rvw_check", i)), Sense::click());
        let p = ui.painter();
        if diff_resp.hovered() {
            p.rect_filled(diff_r, 4.0, C_HOVER);
        }
        p.text(
            diff_r.center(),
            Align2::CENTER_CENTER,
            "±",
            FontId::monospace(12.0),
            if diff_resp.hovered() { C_TEXT } else { C_TEXT_DIM },
        );
        if check_resp.hovered() {
            p.rect_filled(check_r, 4.0, C_HOVER);
        }
        let cc = check_r.center();
        if f.viewed {
            p.circle_filled(cc, 6.5, Color32::from_rgb(0x62, 0xb5, 0x43));
            let st = Stroke::new(1.5, Color32::from_rgb(0x1e, 0x1f, 0x22));
            p.line_segment([cc + Vec2::new(-3.0, 0.2), cc + Vec2::new(-1.0, 2.2)], st);
            p.line_segment([cc + Vec2::new(-1.0, 2.2), cc + Vec2::new(3.2, -2.2)], st);
        } else {
            p.circle_stroke(
                cc,
                6.5,
                Stroke::new(1.2, if check_resp.hovered() { C_TEXT } else { C_TEXT_DIM }),
            );
        }

        if check_resp.on_hover_text("검토 완료 표시").clicked() {
            acts.toggle_viewed = Some(i);
        } else if diff_resp.on_hover_text("diff로 보기").clicked() {
            acts.open_diff = Some(i);
        } else if resp.on_hover_text(&f.path).clicked() {
            acts.open_file = Some(i);
        }
    }
}

impl CodeLookApp {
    /// Rendered Markdown document (read-only preview, centered column).
    fn md_view(&mut self, ui: &mut egui::Ui, idx: usize) {
        let pane = ui.max_rect();
        egui::ScrollArea::vertical()
            .id_salt(("md_scroll", self.ws.seq, idx))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    ui.vertical(|ui| {
                        ui.set_max_width((pane.width() - 72.0).min(880.0));
                        egui_commonmark::CommonMarkViewer::new().show(
                            ui,
                            &mut self.md_cache,
                            &self.ws.tabs[idx].content,
                        );
                    });
                });
                ui.add_space(28.0);
            });
        self.md_toggle(ui, pane, idx);
    }

    /// Raw ↔ rendered toggle for Markdown tabs (floating, pane top-right).
    fn md_toggle(&mut self, ui: &mut egui::Ui, pane: Rect, idx: usize) {
        let preview = self.ws.tabs[idx].md_preview;
        let mut set: Option<bool> = None;
        egui::Area::new(ui.id().with(("md_toggle", self.ws.seq)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(pane.right() - 140.0, pane.top() + 6.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::default()
                    .fill(C_PANEL)
                    .stroke(Stroke::new(1.0, C_BORDER))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(4, 2))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 2.0;
                            if ui
                                .selectable_label(
                                    !preview,
                                    egui::RichText::new("원본").size(11.0),
                                )
                                .clicked()
                            {
                                set = Some(false);
                            }
                            if ui
                                .selectable_label(
                                    preview,
                                    egui::RichText::new("미리보기").size(11.0),
                                )
                                .clicked()
                            {
                                set = Some(true);
                            }
                        });
                    });
            });
        if let Some(v) = set {
            self.ws.tabs[idx].md_preview = v;
        }
    }
}

/// Small crosshair (locate) button, painter-drawn like the tree header's.
fn crosshair_button(ui: &mut egui::Ui, tip: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(r, 4.0, C_HOVER);
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let c = if resp.hovered() { C_TEXT } else { C_TEXT_DIM };
    let center = r.center();
    p.circle_stroke(center, 4.5, Stroke::new(1.3, c));
    p.circle_filled(center, 1.4, c);
    for (dx, dy) in [(0.0, -1.0), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)] {
        let v = Vec2::new(dx, dy);
        p.line_segment([center + v * 4.5, center + v * 7.0], Stroke::new(1.3, c));
    }
    resp.on_hover_text(tip).clicked()
}

/// Small painter-drawn chevron button (▲/▼ without relying on font glyphs).
fn chevron_button(ui: &mut egui::Ui, down: bool, tip: &str) -> bool {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(r, 4.0, C_HOVER);
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let c = if resp.hovered() { C_TEXT } else { C_TEXT_DIM };
    let cx = r.center();
    let (a, b, m) = if down {
        (Vec2::new(-4.0, -2.0), Vec2::new(4.0, -2.0), Vec2::new(0.0, 2.5))
    } else {
        (Vec2::new(-4.0, 2.0), Vec2::new(4.0, 2.0), Vec2::new(0.0, -2.5))
    };
    let st = Stroke::new(1.6, c);
    p.line_segment([cx + a, cx + m], st);
    p.line_segment([cx + m, cx + b], st);
    resp.on_hover_text(tip).clicked()
}

/// First line of every change block: consecutive changed lines collapse to
/// their first line, and each deletion point counts as a block. Sorted.
fn change_block_starts(fd: &crate::git::FileDiff) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut lines: Vec<usize> = fd.changed.iter().map(|&(l, _)| l).collect();
    lines.sort_unstable();
    let mut prev = usize::MAX;
    for l in lines {
        if prev == usize::MAX || l != prev + 1 {
            starts.push(l);
        }
        prev = l;
    }
    starts.extend(fd.deleted_before.iter().copied());
    starts.sort_unstable();
    starts.dedup();
    starts
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
/// GitHub-style pull-request glyph: base column (circle-line-circle) plus a
/// head column flowing in at the top and down to its merge circle.
fn draw_pr_icon(p: &egui::Painter, r: Rect, color: Color32) {
    let st = Stroke::new(1.4, color);
    let lx = r.left() + 8.5;
    let rx = r.right() - 8.5;
    let ty = r.top() + 7.0;
    let by = r.bottom() - 7.0;
    p.circle_stroke(egui::pos2(lx, ty), 2.6, st);
    p.circle_stroke(egui::pos2(lx, by), 2.6, st);
    p.line_segment([egui::pos2(lx, ty + 2.6), egui::pos2(lx, by - 2.6)], st);
    p.circle_stroke(egui::pos2(rx, by), 2.6, st);
    p.line_segment([egui::pos2(rx, by - 2.6), egui::pos2(rx, ty)], st);
    p.line_segment([egui::pos2(rx, ty), egui::pos2(lx + 4.5, ty)], st);
    // Arrowhead pointing into the base branch.
    p.line_segment(
        [egui::pos2(lx + 7.5, ty - 3.0), egui::pos2(lx + 4.5, ty)],
        st,
    );
    p.line_segment(
        [egui::pos2(lx + 7.5, ty + 3.0), egui::pos2(lx + 4.5, ty)],
        st,
    );
}

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
    use super::{build_ghost_doc, word_at};

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

    #[test]
    fn ghost_doc_merges_and_remaps() {
        use crate::git::{FileDiff, LineChange};
        // head: a / b / c — one modified line (b), two lines deleted after a.
        let overlay = FileDiff {
            changed: vec![(1, LineChange::Modified)],
            deleted_before: vec![0],
        };
        let ghosts = vec![(1usize, vec!["X".to_string(), "Y".to_string()])];
        let (doc, fd, rows, ranges) = build_ghost_doc("a\nb\nc", &overlay, &ghosts);
        assert_eq!(doc, "a\nX\nY\nb\nc");
        // Ghosts occupy display rows 1-2; the modified head line 1 shifts to 3.
        assert_eq!(rows, vec![1, 2]);
        assert_eq!(fd.changed, vec![(3, LineChange::Modified)]);
        assert_eq!(fd.deleted_before, vec![1]); // block-nav stop at the group
        // Byte ranges point at exactly "X" and "Y".
        let texts: Vec<&str> = ranges.iter().map(|&(s, e)| &doc[s..e]).collect();
        assert_eq!(texts, vec!["X", "Y"]);
        // Display→head mapping skips ghosts: rows 3,4 are head lines 2,3.
        let mut tab_rows = rows.clone();
        tab_rows.sort_unstable();
        let head_of = |disp: usize| {
            if tab_rows.binary_search(&disp).is_ok() {
                return None;
            }
            Some(disp - tab_rows.partition_point(|&g| g < disp) + 1)
        };
        assert_eq!(head_of(0), Some(1));
        assert_eq!(head_of(1), None);
        assert_eq!(head_of(3), Some(2));
        assert_eq!(head_of(4), Some(3));
    }
}
