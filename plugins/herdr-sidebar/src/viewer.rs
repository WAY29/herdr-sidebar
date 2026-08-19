//! File contents and git diffs shown beside the sidebar. Opening a document
//! zooms the existing sidebar pane and renders the sidebar plus viewer in one
//! TUI, so the viewer owns the whole editor area without moving the tab's
//! other panes. `q`/Esc closes the viewer and restores the original layout.
//!
//! The tail of this module is the client side — the request handoff shared by
//! both sidebar views.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::CellWidth;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ansi;
use crate::icons::{IconTheme, icon};
use crate::ipc;
use crate::syntax::DiffTheme;

/// Metadata source/token that marks the viewer pane, so the sidebar can find
/// and reuse it (distinct from the sidebar's own identity tokens).
pub const METADATA_SOURCE: &str = "herdr-sidebar-preview";

/// How often the control file is re-checked while idle.
const POLL: Duration = Duration::from_millis(250);

/// Preview size guards: don't slurp huge files into a pane.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

/// Enough room for the viewer to remain useful on a narrow terminal.
const MIN_PREVIEW_WIDTH: u16 = 24;

/// Shared search-hit highlight colors used by both Search results and Preview.
pub const SEARCH_HIGHLIGHT_BG: Color = Color::Rgb(0x51, 0x58, 0x00);
pub const SEARCH_HIGHLIGHT_FG: Color = Color::Rgb(0xff, 0xf6, 0xb0);

/// Control-file header used by the in-process viewer. The width is the
/// sidebar's pre-zoom width, which keeps the left column stable after zoom.
const INLINE_CONTROL_PREFIX: &str = "inline\t";

/// Directory for the sidebar's private scratch files (control/park files).
/// `std::env::temp_dir()` can be a shared, world-writable directory (unix
/// `/tmp`) where our filenames are predictable from the pane id; scope our
/// files into a private, mode-0700 subdirectory so another local user can't
/// plant a symlink at a path we're about to `fs::write` through. Windows'
/// per-user `%TEMP%` needs no extra scoping.
fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("herdr-sidebar-scratch");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// Write `contents` to `path`, refusing to follow a pre-existing symlink at
/// that location (defense in depth alongside `scratch_dir`'s 0700 perms).
fn write_scratch_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, contents)
}

/// The control file the sidebar writes requests into, unique per sidebar
/// pane (tab) so tabs don't steer each other's viewers.
pub fn control_path(sidebar_pane_id: &str) -> PathBuf {
    scratch_dir().join(format!(
        "herdr-sidebar-preview-{}.ctl",
        sidebar_pane_id.replace(':', "_")
    ))
}

/// What the sidebar asked the viewer to show.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Request {
    File(PathBuf),
    SearchFile {
        path: PathBuf,
        line: usize,
        query: String,
        regex: bool,
        case_sensitive: bool,
        whole_word: bool,
    },
    Diff {
        root: PathBuf,
        rel: String,
        /// "staged" | "worktree" | "untracked" — which diff to run.
        kind: String,
    },
    RefDiff {
        root: PathBuf,
        old_spec: String,
        new_spec: String,
        rel: String,
        old_rel: Option<String>,
    },
    /// `git show <spec>` — a commit, stash, tag, or branch tip, optionally
    /// narrowed to one file.
    Show {
        root: PathBuf,
        spec: String,
        path: Option<String>,
    },
}

/// Control-file payload for a file preview.
pub fn file_request(path: &Path) -> String {
    format!("file\t{}", path.display())
}

/// Control-file payload for a file preview anchored to one search hit.
pub fn search_file_request(
    path: &Path,
    line: usize,
    query: &str,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
) -> String {
    let payload = serde_json::json!({
        "path": path,
        "line": line,
        "query": query,
        "regex": regex,
        "case_sensitive": case_sensitive,
        "whole_word": whole_word,
    });
    format!("searchfile\t{payload}")
}

/// Control-file payload for a git diff (`kind`: staged | worktree | untracked).
pub fn diff_request(root: &Path, rel: &str, kind: &str) -> String {
    format!("diff\t{}\t{rel}\t{kind}", root.display())
}

/// Immutable structured diff between two revisions for one historical file.
pub fn ref_diff_request(
    root: &Path,
    old_spec: &str,
    new_spec: &str,
    rel: &str,
    old_rel: Option<&str>,
) -> String {
    format!(
        "refdiff\t{}\t{old_spec}\t{new_spec}\t{rel}\t{}",
        root.display(),
        old_rel.unwrap_or("")
    )
}

/// Control-file payload for `git show <spec>` (commit hash, stash@{n}, tag…),
/// optionally narrowed to one file.
pub fn show_request(root: &Path, spec: &str, path: Option<&str>) -> String {
    format!("show\t{}\t{spec}\t{}", root.display(), path.unwrap_or(""))
}

fn parse_request(raw: &str) -> Option<Request> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.split('\t');
    match parts.next() {
        Some("diff") => {
            let root = PathBuf::from(parts.next()?);
            let rel = parts.next()?.to_string();
            let kind = parts.next().unwrap_or("worktree").to_string();
            Some(Request::Diff { root, rel, kind })
        }
        Some("show") => {
            let root = PathBuf::from(parts.next()?);
            let spec = parts.next()?.to_string();
            let path = parts.next().filter(|p| !p.is_empty()).map(str::to_string);
            Some(Request::Show { root, spec, path })
        }
        Some("refdiff") => {
            let root = PathBuf::from(parts.next()?);
            let old_spec = parts.next()?.to_string();
            let new_spec = parts.next()?.to_string();
            let rel = parts.next()?.to_string();
            let old_rel = parts.next().filter(|path| !path.is_empty()).map(str::to_string);
            Some(Request::RefDiff { root, old_spec, new_spec, rel, old_rel })
        }
        Some("searchfile") => {
            #[derive(serde::Deserialize)]
            struct Payload {
                path: PathBuf,
                line: usize,
                query: String,
                regex: bool,
                case_sensitive: bool,
                whole_word: bool,
            }
            let payload: Payload = serde_json::from_str(parts.next()?).ok()?;
            Some(Request::SearchFile {
                path: payload.path,
                line: payload.line,
                query: payload.query,
                regex: payload.regex,
                case_sensitive: payload.case_sensitive,
                whole_word: payload.whole_word,
            })
        }
        Some("file") => Some(Request::File(PathBuf::from(parts.next()?))),
        // Legacy: a bare path.
        _ => Some(Request::File(PathBuf::from(raw))),
    }
}

struct Doc {
    name: String,
    context: String,
    lines: Vec<Line<'static>>,
    /// File previews get a line-number gutter; diffs carry their own gutter.
    numbered: bool,
    scroll: usize,
    /// Expandable fold owned by each rendered row; empty for non-diff docs.
    folds: Vec<Option<crate::diffview::FoldId>>,
    /// Width-cached visual rows for structured diffs, plus their source rows.
    visual_lines: Vec<Line<'static>>,
    visual_rows: Vec<usize>,
    visual_width: u16,
    first_change: Option<usize>,
    center_first_change_pending: bool,
}

impl Doc {
    fn new(
        name: String,
        context: String,
        lines: Vec<Line<'static>>,
        numbered: bool,
        folds: Vec<Option<crate::diffview::FoldId>>,
    ) -> Self {
        Self {
            name,
            context,
            lines,
            numbered,
            scroll: 0,
            folds,
            visual_lines: Vec::new(),
            visual_rows: Vec::new(),
            visual_width: 0,
            first_change: None,
            center_first_change_pending: false,
        }
    }

    fn ensure_visual_lines(&mut self, width: u16) {
        let width = width.max(1);
        if self.folds.is_empty() || self.visual_width == width {
            return;
        }
        (self.visual_lines, self.visual_rows) = reflow_diff_lines(&self.lines, width);
        self.visual_width = width;
    }

    fn visual_len(&self) -> usize {
        if self.visual_width == 0 {
            self.lines.len()
        } else {
            self.visual_lines.len()
        }
    }

    fn source_row_at(&self, visual_row: usize) -> Option<usize> {
        if self.visual_width == 0 {
            (visual_row < self.lines.len()).then_some(visual_row)
        } else {
            self.visual_rows.get(visual_row).copied()
        }
    }

    fn request_first_change_center(&mut self) {
        self.center_first_change_pending = self.first_change.is_some();
    }

    fn apply_first_change_center(&mut self, height: u16) {
        if !std::mem::take(&mut self.center_first_change_pending) {
            return;
        }
        let Some(source_row) = self.first_change else { return };
        let visual_row = if self.visual_width == 0 {
            source_row
        } else {
            self.visual_rows
                .iter()
                .position(|row| *row == source_row)
                .unwrap_or(source_row)
        };
        let height = usize::from(height).max(1);
        let max_scroll = self.visual_len().saturating_sub(height);
        self.scroll = visual_row.saturating_sub(height / 2).min(max_scroll);
    }
}

fn flush_wrapped_span(
    spans: &mut Vec<Span<'static>>,
    text: &mut String,
    style: &mut Option<Style>,
) {
    if let Some(style) = style.take()
        && !text.is_empty()
    {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

fn finish_wrapped_line(
    mut spans: Vec<Span<'static>>,
    text: &mut String,
    style: &mut Option<Style>,
    line_style: Style,
    width: u16,
    used: u16,
) -> Line<'static> {
    flush_wrapped_span(&mut spans, text, style);
    if line_style.bg.is_some() && used < width {
        spans.push(Span::raw(" ".repeat(usize::from(width - used))));
    }
    Line::from(spans).style(line_style)
}

/// Hard-wrap one styled diff row by terminal cells. Continuations keep an
/// empty copy of the original line-number/change-bar gutter.
fn wrap_diff_line(line: &Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let indent = if line.spans.len() >= 2 {
        line.spans.iter().take(2).map(Span::width).sum::<usize>()
    } else {
        0
    };
    let indent = u16::try_from(indent)
        .unwrap_or(u16::MAX)
        .min(width.saturating_sub(1));
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = None;
    let mut used = 0u16;
    let mut content_start = 0u16;

    for grapheme in line.styled_graphemes(Style::default()) {
        let grapheme_width = grapheme.symbol.cell_width();
        if used > content_start
            && grapheme_width > 0
            && used.saturating_add(grapheme_width) > width
        {
            rows.push(finish_wrapped_line(
                std::mem::take(&mut spans),
                &mut text,
                &mut style,
                line.style,
                width,
                used,
            ));
            if indent > 0 {
                spans.push(Span::raw(" ".repeat(usize::from(indent))));
            }
            used = indent;
            content_start = indent;
        }
        if style != Some(grapheme.style) {
            flush_wrapped_span(&mut spans, &mut text, &mut style);
            style = Some(grapheme.style);
        }
        text.push_str(grapheme.symbol);
        used = used.saturating_add(grapheme_width);
    }

    rows.push(finish_wrapped_line(
        spans,
        &mut text,
        &mut style,
        line.style,
        width,
        used,
    ));
    rows
}

fn reflow_diff_lines(
    lines: &[Line<'static>],
    width: u16,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut visual_lines = Vec::new();
    let mut visual_rows = Vec::new();
    for (source_row, line) in lines.iter().enumerate() {
        let wrapped = wrap_diff_line(line, width);
        visual_rows.extend(std::iter::repeat_n(source_row, wrapped.len()));
        visual_lines.extend(wrapped);
    }
    (visual_lines, visual_rows)
}

fn load(
    request: &Request,
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: &HashSet<crate::diffview::FoldId>,
) -> Doc {
    match request {
        Request::File(path) => load_file(path, diff_theme),
        Request::SearchFile { path, line, query, regex, case_sensitive, whole_word } => {
            load_search_file(path, *line, query, *regex, *case_sensitive, *whole_word, diff_theme)
        }
        Request::Diff { root, rel, kind } => {
            load_diff(root, rel, kind, diff_theme, hide_unmodified, expanded_folds)
        }
        Request::RefDiff { root, old_spec, new_spec, rel, old_rel } => load_ref_diff(
            root,
            (old_spec, new_spec),
            rel,
            old_rel.as_deref(),
            diff_theme,
            hide_unmodified,
            expanded_folds,
        ),
        Request::Show { root, spec, path } => load_show(root, spec, path.as_deref()),
    }
}

/// `git show` with stat + patch, colored — what a click on a commit, stash,
/// tag, or branch line renders. Immutable content: no refresh loop needed.
fn load_show(root: &Path, spec: &str, path: Option<&str>) -> Doc {
    let mut args: Vec<String> = vec![
        "-c".into(),
        "color.ui=always".into(),
        "show".into(),
        "--color=always".into(),
        "--stat".into(),
        "--patch".into(),
        "--no-ext-diff".into(),
        spec.to_string(),
    ];
    if let Some(p) = path {
        args.push("--".into());
        args.push(p.replace('/', std::path::MAIN_SEPARATOR_STR));
    }
    let output = std::process::Command::new("git").args(&args).current_dir(root).output();
    let lines = match output {
        Err(e) => vec![Line::raw(format!("(git failed: {e})"))],
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.trim().is_empty() {
                let err = String::from_utf8_lossy(&out.stderr);
                if err.trim().is_empty() {
                    vec![Line::raw("(nothing to show)")]
                } else {
                    vec![Line::raw(format!("({})", err.trim()))]
                }
            } else {
                ansi::to_lines(&text)
            }
        }
    };
    Doc::new(
        spec.to_string(),
        format!("git show {spec} — {}", root.display()),
        lines,
        false,
        Vec::new(),
    )
}

/// Render markdown text via `glow`. Returns `None` when glow is not installed
/// or exits non-zero (caller falls back to syntax highlight).
///
/// Receives the already-read `text` buffer so the MAX_BYTES guard in
/// `load_file` is honoured — glow would otherwise re-read the full file.
/// Pipes via stdin (`-`) to avoid treating filenames starting with `-` as
/// flags. Width is a best-effort approximation; the ideal fix would pass
/// `body.width` from `draw_doc` once that is available at load time.
fn glow_markdown(text: &str, width: u16) -> Option<Vec<Line<'static>>> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("glow")
        .args(["--style", "dark", "--width", &width.to_string(), "-"])
        .env("CLICOLOR_FORCE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        let _ = stdin.write_all(text.as_bytes());
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let rendered = String::from_utf8_lossy(&output.stdout);
    if rendered.trim().is_empty() {
        return None;
    }
    let mut lines = ansi::to_lines(&rendered);
    lines.truncate(MAX_LINES);
    if lines.is_empty() {
        return None;
    }
    Some(lines)
}

fn load_file(target: &Path, diff_theme: DiffTheme) -> Doc {
    load_file_inner(target, diff_theme, None)
}

fn load_search_file(
    target: &Path,
    line: usize,
    query: &str,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    diff_theme: DiffTheme,
) -> Doc {
    let mut doc = load_file_inner(
        target,
        diff_theme,
        Some((query, regex, case_sensitive, whole_word)),
    );
    doc.scroll = line.saturating_sub(5).saturating_sub(1);
    doc
}

fn load_file_inner(
    target: &Path,
    diff_theme: DiffTheme,
    search: Option<(&str, bool, bool, bool)>,
) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lower = name.to_lowercase();
    let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
    let (lines, numbered) = match std::fs::read(target) {
        Err(e) => (vec![Line::raw(format!("(unreadable: {e})"))], true),
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8192)];
            if head.contains(&0) {
                (vec![Line::raw(format!("(binary file — {} bytes)", bytes.len()))], false)
            } else {
                let truncated = bytes.len() > MAX_BYTES;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]);
                let search_re = search
                    .and_then(|(query, regex, case_sensitive, whole_word)| {
                        build_search_regex(query, regex, case_sensitive, whole_word).ok()
                    });
                // Markdown: render via glow; fall back to syntax highlight on failure.
                // Width is approximated by subtracting 6 for the sidebar share and
                // line-number gutter; ideal fix is to pass body.width from draw_doc.
                let glow_width =
                    crossterm::terminal::size().map(|(w, _)| w.saturating_sub(6)).unwrap_or(74);
                let glow_rendered = search_re
                    .is_none()
                    .then(|| is_markdown.then(|| glow_markdown(&text, glow_width)).flatten())
                    .flatten();
                // Glow-rendered markdown gets no line numbers (it formats its own layout).
                let numbered = glow_rendered.is_none();
                let mut lines: Vec<Line<'static>> = if let Some(rendered) = glow_rendered {
                    rendered
                } else {
                    crate::syntax::highlight(&name, &text, MAX_LINES, diff_theme)
                        .unwrap_or_else(|| {
                            text.lines()
                                .take(MAX_LINES)
                                .map(|l| Line::raw(l.to_string()))
                                .collect()
                        })
                };
                if let Some(search_re) = search_re.as_ref() {
                    apply_search_highlights(&mut lines, &text, search_re);
                }
                for line in &mut lines {
                    line.spans = crate::syntax::expand_tabs(std::mem::take(&mut line.spans));
                }
                if truncated || text.lines().count() > MAX_LINES {
                    lines.push(Line::raw("… (truncated)"));
                }
                if lines.is_empty() {
                    lines.push(Line::raw("(empty file)"));
                }
                (lines, numbered)
            }
        }
    };
    Doc::new(name, target.display().to_string(), lines, numbered, Vec::new())
}

fn build_search_regex(
    query: &str,
    regex_mode: bool,
    case_sensitive: bool,
    whole_word: bool,
) -> Result<regex::Regex, regex::Error> {
    let pattern = if regex_mode {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let pattern = if whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    let mut builder = regex::RegexBuilder::new(&pattern);
    builder.case_insensitive(!case_sensitive);
    builder.build()
}

fn apply_search_highlights(lines: &mut [Line<'static>], text: &str, regex: &regex::Regex) {
    for (line, raw) in lines.iter_mut().zip(text.lines().take(MAX_LINES)) {
        let matches: Vec<(usize, usize)> = regex.find_iter(raw).map(|m| (m.start(), m.end())).collect();
        if matches.is_empty() {
            continue;
        }
        *line = highlight_line(
            line,
            &matches,
            Style::default().bg(SEARCH_HIGHLIGHT_BG).fg(SEARCH_HIGHLIGHT_FG),
        );
    }
}

fn highlight_line(
    line: &Line<'static>,
    matches: &[(usize, usize)],
    highlight: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for span in &line.spans {
        let content = span.content.as_ref();
        let start = offset;
        let end = start + content.len();
        let mut cursor = start;
        for (ms, me) in matches.iter().copied() {
            let from = ms.max(start);
            let to = me.min(end);
            if from >= to {
                continue;
            }
            if cursor < from {
                spans.push(Span::styled(content[cursor - start..from - start].to_string(), span.style));
            }
            spans.push(Span::styled(
                content[from - start..to - start].to_string(),
                span.style.patch(highlight),
            ));
            cursor = to;
        }
        if cursor < end {
            spans.push(Span::styled(content[cursor - start..].to_string(), span.style));
        }
        offset = end;
    }
    Line::from(spans).style(line.style)
}

fn load_diff(
    root: &Path,
    rel: &str,
    kind: &str,
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: &HashSet<crate::diffview::FoldId>,
) -> Doc {
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    // Plain (uncolored) diff: crate::diffview parses it and renders the
    // editor look — line gutter, tinted rows, syntax-highlighted code.
    // ponytail: keep Git context inside the viewer's 5k display ceiling;
    // show-all mode splits that budget around each change.
    let context = if hide_unmodified { MAX_LINES } else { MAX_LINES / 2 };
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-ext-diff".into(),
        format!("--unified={context}"),
    ];
    match kind {
        "staged" => args.push("--cached".into()),
        // An untracked file has no diff; --no-index against the null device
        // renders it as one big addition, like VS Code does.
        "untracked" => {
            args.push("--no-index".into());
            args.push(if cfg!(windows) { "NUL".into() } else { "/dev/null".into() });
        }
        _ => {}
    }
    args.push("--".into());
    args.push(rel.replace('/', std::path::MAIN_SEPARATOR_STR));

    let (lines, folds, first_change) = run_structured_diff(
        root,
        rel,
        &args,
        diff_theme,
        hide_unmodified,
        expanded_folds,
    );
    let what = match kind {
        "staged" => "staged",
        "untracked" => "untracked",
        _ => "working tree",
    };
    let mut doc = Doc::new(
        name,
        format!("{} — {what} diff", root.join(rel).display()),
        lines,
        false,
        folds,
    );
    doc.first_change = first_change;
    doc
}

fn load_ref_diff(
    root: &Path,
    specs: (&str, &str),
    rel: &str,
    old_rel: Option<&str>,
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: &HashSet<crate::diffview::FoldId>,
) -> Doc {
    let (old_spec, new_spec) = specs;
    let context = if hide_unmodified { MAX_LINES } else { MAX_LINES / 2 };
    let mut args = vec![
        "diff".to_string(),
        "--no-ext-diff".to_string(),
        "-M".to_string(),
        format!("--unified={context}"),
        old_spec.to_string(),
        new_spec.to_string(),
        "--".to_string(),
    ];
    if let Some(old_rel) = old_rel.filter(|old_rel| *old_rel != rel) {
        args.push(old_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    }
    args.push(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    let (lines, folds, first_change) = run_structured_diff(
        root,
        rel,
        &args,
        diff_theme,
        hide_unmodified,
        expanded_folds,
    );
    let mut doc = Doc::new(
        rel.rsplit('/').next().unwrap_or(rel).to_string(),
        format!("{} — {new_spec}", root.join(rel).display()),
        lines,
        false,
        folds,
    );
    doc.first_change = first_change;
    doc
}

fn run_structured_diff(
    root: &Path,
    rel: &str,
    args: &[String],
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: &HashSet<crate::diffview::FoldId>,
) -> (
    Vec<Line<'static>>,
    Vec<Option<crate::diffview::FoldId>>,
    Option<usize>,
) {
    match std::process::Command::new("git").args(args).current_dir(root).output() {
        Err(e) => (vec![Line::raw(format!("(git failed: {e})"))], Vec::new(), None),
        Ok(out) => {
            // --no-index exits 1 when the files differ; that's not an error.
            let text = String::from_utf8_lossy(&out.stdout);
            if text.trim().is_empty() {
                let err = String::from_utf8_lossy(&out.stderr);
                if err.trim().is_empty() {
                    (vec![Line::raw("(no changes)")], Vec::new(), None)
                } else {
                    (vec![Line::raw(format!("({})", err.trim()))], Vec::new(), None)
                }
            } else {
                let mut rendered =
                    crate::diffview::render_expanded(
                        rel,
                        &text,
                        diff_theme,
                        expanded_folds,
                        hide_unmodified,
                    );
                if rendered.lines.len() > MAX_LINES {
                    rendered.lines.truncate(MAX_LINES);
                    rendered.folds.truncate(MAX_LINES);
                    rendered.first_change =
                        rendered.first_change.filter(|row| *row < MAX_LINES);
                    rendered.lines.push(Line::raw("… (truncated)"));
                    rendered.folds.push(None);
                }
                (rendered.lines, rendered.folds, rendered.first_change)
            }
        }
    }
}

fn control_request(raw: &str) -> (Option<u16>, &str) {
    let Some(rest) = raw.strip_prefix(INLINE_CONTROL_PREFIX) else {
        return (None, raw);
    };
    let Some((width, payload)) = rest.split_once('\n') else {
        return (None, raw);
    };
    (width.parse().ok(), payload)
}

fn read_control(control: &Path) -> Option<Request> {
    let mut buf = String::new();
    std::fs::File::open(control).ok()?.read_to_string(&mut buf).ok()?;
    parse_request(control_request(&buf).1)
}

fn read_inline_control(control: &Path) -> Option<(u16, Request)> {
    let mut buf = String::new();
    std::fs::File::open(control).ok()?.read_to_string(&mut buf).ok()?;
    let (width, payload) = control_request(&buf);
    Some((width?, parse_request(payload)?))
}

/// Tag our pane (heartbeat-stamped, see launch::HEARTBEAT_STALE_SECS) and
/// title it with the shown document's name.
fn report_identity(doc_name: &str) {
    let Ok(pane_id) = std::env::var("HERDR_PANE_ID") else { return };
    if pane_id.is_empty() {
        return;
    }
    let _ = ipc::call_text(
        "pane.report_metadata",
        serde_json::json!({
            "pane_id": pane_id,
            "source": METADATA_SOURCE,
            "tokens": { METADATA_SOURCE: crate::state::unix_now().to_string() },
        }),
    );
    let _ = ipc::call_text(
        "pane.rename",
        serde_json::json!({ "pane_id": pane_id, "label": doc_name }),
    );
}

/// Close our own pane (ends this process with it), handing focus back to
/// the sidebar that spawned us — its pane id is baked into the control-file
/// name, so a full-screen (zoomed) preview drops the user exactly where
/// they were.
fn close_own_pane(control: &Path) {
    if let Some(owner) = owner_pane_id(control) {
        // One-way compatibility for full-size previews opened by older builds.
        restore_parked(&owner);
        let _ = ipc::call_text("pane.focus", serde_json::json!({ "pane_id": owner }));
    }
    if let Ok(pane_id) = std::env::var("HERDR_PANE_ID")
        && !pane_id.is_empty()
    {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane_id }));
    }
}

/// The sidebar pane that owns this viewer, recovered from the control-file
/// name (`herdr-sidebar-preview-<id with ':' as '_'>.ctl`).
fn owner_pane_id(control: &Path) -> Option<String> {
    let stem = control.file_stem()?.to_str()?;
    let id = stem.strip_prefix("herdr-sidebar-preview-")?.replace('_', ":");
    (!id.is_empty()).then_some(id)
}

/// In-process viewer hosted by the sidebar pane while that pane is zoomed.
pub struct InlinePreview {
    owner: Option<String>,
    restore_focus: Option<String>,
    control: Option<PathBuf>,
    current: Option<Request>,
    doc: Option<Doc>,
    theme: IconTheme,
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: HashSet<crate::diffview::FoldId>,
    sidebar_width: u16,
    area: Rect,
    body: Rect,
    last_refresh: Instant,
}

impl InlinePreview {
    pub fn for_current_pane() -> Self {
        let owner = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty());
        let restore_focus = owner.as_deref().and_then(focused_peer);
        let control = owner.as_deref().map(control_path);
        if let Some(owner) = owner.as_deref() {
            restore_parked(owner);
            close_legacy_viewer(owner);
        }
        if let Some(control) = &control {
            let _ = std::fs::remove_file(control);
        }
        let state = crate::state::load_state();
        Self {
            owner,
            restore_focus,
            control,
            current: None,
            doc: None,
            theme: IconTheme::resolve(
                std::env::var("HERDR_SIDEBAR_ICONS")
                    .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                    .ok()
                    .as_deref(),
                state.icons,
            ),
            diff_theme: state.diff_theme,
            hide_unmodified: state.hide_unmodified,
            expanded_folds: HashSet::new(),
            sidebar_width: 30,
            area: Rect::default(),
            body: Rect::default(),
            last_refresh: Instant::now(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.doc.is_some()
    }

    /// Remember the focused peer while the pointer approaches an unfocused
    /// sidebar. Herdr forwards the mouse press before focusing the pane, so a
    /// direct file click keeps this target for Preview close.
    pub fn observe_mouse(&mut self) {
        if self.is_open() {
            return;
        }
        if let Some(peer) = self.owner.as_deref().and_then(focused_peer) {
            self.restore_focus = Some(peer);
        }
    }

    /// A normal interaction with the unzoomed Sidebar makes it the intended
    /// return target; don't retain an older peer from a previous focus visit.
    pub fn claim_focus(&mut self) {
        if !self.is_open() {
            self.restore_focus = None;
        }
    }

    /// Focus reporting reaches the old pane after Herdr has selected the new
    /// one, so pane.list now identifies the exact peer to restore later.
    pub fn on_focus_lost(&mut self) {
        self.observe_mouse();
    }

    /// Follow requests written synchronously by `open_in_pane`.
    pub fn sync(&mut self) {
        let Some(control) = &self.control else { return };
        let Some((width, request)) = read_inline_control(control) else {
            if self.current.is_some() {
                self.current = None;
                self.doc = None;
                self.expanded_folds.clear();
            }
            return;
        };
        if self.current.as_ref() == Some(&request) {
            return;
        }
        self.sidebar_width = width.max(1);
        let state = crate::state::load_state();
        self.diff_theme = state.diff_theme;
        self.hide_unmodified = state.hide_unmodified;
        self.expanded_folds.clear();
        let mut doc = load(
            &request,
            self.diff_theme,
            self.hide_unmodified,
            &self.expanded_folds,
        );
        if !self.hide_unmodified {
            doc.request_first_change_center();
        }
        self.doc = Some(doc);
        self.current = Some(request);
        self.last_refresh = Instant::now();
    }

    pub fn areas(&self, area: Rect) -> Option<(Rect, Rect)> {
        self.is_open().then(|| {
            let min_preview = MIN_PREVIEW_WIDTH.min(area.width.saturating_sub(1)).max(1);
            let sidebar_width = self
                .sidebar_width
                .min(area.width.saturating_sub(min_preview))
                .max(1)
                .min(area.width);
            (
                Rect::new(area.x, area.y, sidebar_width, area.height),
                Rect::new(
                    area.x + sidebar_width,
                    area.y,
                    area.width.saturating_sub(sidebar_width),
                    area.height,
                ),
            )
        })
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let Some(doc) = self.doc.as_mut() else { return };
        self.area = area;
        frame.render_widget(Clear, area);
        let border = Block::default().borders(Borders::LEFT).border_style(Style::default().dim());
        let inner = border.inner(area);
        frame.render_widget(border, area);
        let (_, body) = draw_doc(frame, doc, self.theme, inner);
        self.body = body;
    }

    pub fn owns_mouse(&self, mouse: &MouseEvent) -> bool {
        self.is_open()
            && mouse.column >= self.area.x
            && mouse.column < self.area.x + self.area.width
            && mouse.row >= self.area.y
            && mouse.row < self.area.y + self.area.height
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.close();
            return;
        }
        let Some(doc) = self.doc.as_mut() else { return };
        let max = doc.visual_len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => doc.scroll = doc.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => doc.scroll = (doc.scroll + 1).min(max),
            KeyCode::PageUp => doc.scroll = doc.scroll.saturating_sub(20),
            KeyCode::PageDown => doc.scroll = (doc.scroll + 20).min(max),
            KeyCode::Home | KeyCode::Char('g') => doc.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => doc.scroll = max,
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && mouse.row == self.area.y
            && mouse.column < self.area.x + 5
        {
            self.close();
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(visual_row) =
                doc_row_at(self.body, self.doc.as_ref().map_or(0, |doc| doc.scroll), &mouse)
            && let Some(row) = self.doc.as_ref().and_then(|doc| doc.source_row_at(visual_row))
            && let Some(fold) = self
                .doc
                .as_ref()
                .and_then(|doc| doc.folds.get(row))
                .copied()
                .flatten()
        {
            if self.expanded_folds.insert(fold) {
                self.reload_current(false);
            }
            return;
        }
        let Some(doc) = self.doc.as_mut() else { return };
        let max = doc.visual_len().saturating_sub(1);
        match mouse.kind {
            MouseEventKind::ScrollUp => doc.scroll = doc.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => doc.scroll = (doc.scroll + 3).min(max),
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        let state = crate::state::load_state();
        let fold_setting_changed = state.hide_unmodified != self.hide_unmodified;
        if state.diff_theme != self.diff_theme || fold_setting_changed {
            let center_first_change = fold_setting_changed && !state.hide_unmodified;
            self.diff_theme = state.diff_theme;
            self.hide_unmodified = state.hide_unmodified;
            if fold_setting_changed {
                self.expanded_folds.clear();
            }
            self.reload_current(center_first_change);
        }
        if self.last_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_refresh = Instant::now();
        let Some(request @ Request::Diff { .. }) = self.current.as_ref() else {
            return;
        };
        let keep = self.doc.as_ref().map(|doc| doc.scroll).unwrap_or(0);
        let mut doc = load(
            request,
            self.diff_theme,
            self.hide_unmodified,
            &self.expanded_folds,
        );
        doc.scroll = keep;
        self.doc = Some(doc);
    }

    fn reload_current(&mut self, center_first_change: bool) {
        let Some(request) = self.current.as_ref() else { return };
        let keep = self.doc.as_ref().map(|doc| doc.scroll).unwrap_or(0);
        let mut doc = load(
            request,
            self.diff_theme,
            self.hide_unmodified,
            &self.expanded_folds,
        );
        if center_first_change {
            doc.request_first_change_center();
        } else {
            doc.scroll = keep;
        }
        self.doc = Some(doc);
        self.last_refresh = Instant::now();
    }

    pub fn close(&mut self) {
        if !self.is_open() {
            return;
        }
        if let Some(control) = &self.control {
            let _ = std::fs::remove_file(control);
        }
        self.current = None;
        self.doc = None;
        self.expanded_folds.clear();
        if let Some(owner) = &self.owner {
            let _ = ipc::call_text(
                "pane.zoom",
                serde_json::json!({ "pane_id": owner, "mode": "off" }),
            );
        }
        if let Some(pane_id) = self.restore_focus.take() {
            let _ = ipc::call_text("pane.focus", serde_json::json!({ "pane_id": pane_id }));
        }
    }
}

impl Drop for InlinePreview {
    fn drop(&mut self) {
        self.close();
    }
}

/// The viewer's event loop; returns when the user closes it.
pub fn run(control: &Path) -> std::io::Result<()> {
    let state = crate::state::load_state();
    let theme = IconTheme::resolve(
        std::env::var("HERDR_SIDEBAR_ICONS")
            .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
            .ok()
            .as_deref(),
        state.icons,
    );
    let mut diff_theme = state.diff_theme;
    let mut hide_unmodified = state.hide_unmodified;
    let mut current = read_control(control);
    let mut expanded_folds = HashSet::new();
    let mut doc = current
        .as_ref()
        .map(|request| {
            let mut doc = load(request, diff_theme, hide_unmodified, &expanded_folds);
            if !hide_unmodified {
                doc.request_first_change_center();
            }
            doc
        })
        .unwrap_or_else(|| {
            Doc::new(
                "(nothing to show)".into(),
                String::new(),
                vec![Line::raw("(waiting for a click in the sidebar)")],
                false,
                Vec::new(),
            )
        });
    report_identity(&doc.name);

    // Blank the primary screen so pane handoffs never flash the shell.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0),
    );
    crossterm::style::force_color_output(true); // TUI colors ≠ pipeable output
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let mut page: usize = 20;
    let mut body = Rect::default();
    let mut beat: u64 = 0;
    let result = loop {
        let draw = terminal.draw(|frame| {
            let area = frame.area();
            (page, body) = draw_doc(frame, &mut doc, theme, area);
        });
        if let Err(e) = draw {
            break Err(e);
        }
        let max = doc.visual_len().saturating_sub(1);
        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        close_own_pane(control);
                        break Ok(());
                    }
                    KeyCode::Up | KeyCode::Char('k') => doc.scroll = doc.scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => doc.scroll = (doc.scroll + 1).min(max),
                    KeyCode::PageUp => doc.scroll = doc.scroll.saturating_sub(page),
                    KeyCode::PageDown => doc.scroll = (doc.scroll + page).min(max),
                    KeyCode::Home | KeyCode::Char('g') => doc.scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => doc.scroll = max,
                    _ => {}
                },
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => doc.scroll = doc.scroll.saturating_sub(3),
                    MouseEventKind::ScrollDown => doc.scroll = (doc.scroll + 3).min(max),
                    MouseEventKind::Down(MouseButton::Left) if mouse.row == 0 => {
                        close_own_pane(control);
                        break Ok(());
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(visual_row) = doc_row_at(body, doc.scroll, &mouse)
                            && let Some(row) = doc.source_row_at(visual_row)
                            && let Some(fold) = doc.folds.get(row).copied().flatten()
                            && expanded_folds.insert(fold)
                            && let Some(request) = &current
                        {
                            let keep = doc.scroll;
                            doc = load(
                                request,
                                diff_theme,
                                hide_unmodified,
                                &expanded_folds,
                            );
                            doc.scroll = keep;
                        }
                    }
                    _ => {}
                },
                _ => {} // resize etc: redraw
            }
        } else {
            // Idle: heartbeat, follow the control file, and live-refresh diffs.
            beat += 1;
            if beat.is_multiple_of(20) {
                report_identity(&doc.name);
            }
            let target = read_control(control);
            if target != current {
                current = target;
                expanded_folds.clear();
                if let Some(request) = &current {
                    let state = crate::state::load_state();
                    diff_theme = state.diff_theme;
                    hide_unmodified = state.hide_unmodified;
                    doc = load(request, diff_theme, hide_unmodified, &expanded_folds);
                    if !hide_unmodified {
                        doc.request_first_change_center();
                    }
                    report_identity(&doc.name);
                }
            } else if beat.is_multiple_of(8)
                && let Some(request @ Request::Diff { .. }) = &current
            {
                let keep = doc.scroll;
                let state = crate::state::load_state();
                let fold_setting_changed = state.hide_unmodified != hide_unmodified;
                if fold_setting_changed {
                    expanded_folds.clear();
                }
                diff_theme = state.diff_theme;
                hide_unmodified = state.hide_unmodified;
                doc = load(request, diff_theme, hide_unmodified, &expanded_folds);
                if fold_setting_changed && !hide_unmodified {
                    doc.request_first_change_center();
                } else {
                    doc.scroll = keep;
                }
            }
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn doc_row_at(body: Rect, scroll: usize, mouse: &MouseEvent) -> Option<usize> {
    (mouse.column >= body.x
        && mouse.column < body.x.saturating_add(body.width)
        && mouse.row >= body.y
        && mouse.row < body.y.saturating_add(body.height))
        .then(|| scroll + usize::from(mouse.row - body.y))
}

/// Header (✕ close + name + context), body, hint footer. Returns the page
/// stride for PageUp/Down and the body hit-test rectangle.
fn draw_doc(frame: &mut Frame, doc: &mut Doc, theme: IconTheme, area: Rect) -> (usize, Rect) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    doc.ensure_visual_lines(body.width);
    doc.apply_first_change_center(body.height);
    doc.scroll = doc
        .scroll
        .min(doc.visual_len().saturating_sub(usize::from(body.height).max(1)));

    let file_icon = icon(theme, &doc.name, false, false);
    let icon_style = match file_icon.rgb {
        Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
        None => Style::default(),
    };
    let left = vec![
        Span::styled(" ✕ ", Style::default().bold().fg(Color::LightBlue)),
        Span::styled(format!("{} ", file_icon.glyph), icon_style),
        Span::styled(doc.name.clone(), Style::default().bold()),
    ];
    let used: usize = left.iter().map(Span::width).sum();
    let avail = usize::from(area.width).saturating_sub(used + 2);
    let shown = if doc.context.chars().count() > avail {
        let tail: String = doc
            .context
            .chars()
            .skip(doc.context.chars().count().saturating_sub(avail.saturating_sub(1)))
            .collect();
        format!("…{tail}")
    } else {
        doc.context.clone()
    };
    let mut spans = left;
    spans.push(Span::styled(format!("  {shown}"), Style::default().dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), header);

    let text: Vec<Line> = if doc.visual_width > 0 {
        doc.visual_lines
            .iter()
            .skip(doc.scroll)
            .take(usize::from(body.height))
            .cloned()
            .collect()
    } else {
        let number_width = doc.lines.len().to_string().len();
        doc.lines
            .iter()
            .enumerate()
            .skip(doc.scroll)
            .take(usize::from(body.height))
            .map(|(n, line)| {
                if doc.numbered {
                    let mut spans = vec![Span::styled(
                        format!("{:>number_width$} ", n + 1),
                        Style::default().dim(),
                    )];
                    spans.extend(line.spans.iter().cloned());
                    Line::from(spans)
                } else {
                    let mut line = line.clone();
                    if line.style.bg.is_some() {
                        let pad = usize::from(body.width).saturating_sub(line.width());
                        if pad > 0 {
                            line.spans.push(Span::raw(" ".repeat(pad)));
                        }
                    }
                    line
                }
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(text), body);

    frame.render_widget(
        Paragraph::new(Line::from(" ↑↓ scroll  ⇞⇟ page  g G ends  q close".dim())),
        footer,
    );
    (usize::from(body.height).saturating_sub(1).max(1), body)
}

// ---------------------------------------------------------------------------
// Client side: how the sidebar views open things in the viewer pane.
// ---------------------------------------------------------------------------

/// Open or update the in-process viewer, then zoom and focus the sidebar pane.
pub fn open_in_pane(my_pane_id: &str, _spawn_cwd: &Path, payload: &str) -> Result<(), String> {
    let control = control_path(my_pane_id);
    // One-way compatibility for full-size previews opened by older builds.
    restore_parked(my_pane_id);
    close_legacy_viewer(my_pane_id);

    let width = read_inline_control(&control)
        .map(|(width, _)| width)
        .unwrap_or_else(|| crossterm::terminal::size().map(|(width, _)| width).unwrap_or(30));
    let request = format!("{INLINE_CONTROL_PREFIX}{width}\n{payload}");
    write_scratch_file(&control, &request).map_err(|e| format!("preview failed: {e}"))?;
    if let Err(e) = ipc::call_text(
        "pane.zoom",
        serde_json::json!({ "pane_id": my_pane_id, "mode": "on" }),
    ) {
        let _ = std::fs::remove_file(control);
        return Err(format!("preview failed to focus: {e}"));
    }
    Ok(())
}

/// Close the in-process viewer and any viewer pane left by an older build.
pub fn close_in_tab(my_pane_id: &str) {
    restore_parked(my_pane_id);
    let control = control_path(my_pane_id);
    if control.exists() {
        let _ = std::fs::remove_file(control);
        let _ = ipc::call_text(
            "pane.zoom",
            serde_json::json!({ "pane_id": my_pane_id, "mode": "off" }),
        );
    }
    close_legacy_viewer(my_pane_id);
}

fn focused_peer(owner: &str) -> Option<String> {
    let panes = ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    focused_peer_in_tab(&panes, owner)
}

fn focused_peer_in_tab(pane_list_json: &str, owner: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        tab_id: Option<String>,
        #[serde(default)]
        focused: bool,
    }

    let msg: Msg = serde_json::from_str(pane_list_json.trim_start_matches('\u{feff}')).ok()?;
    let tab = msg
        .result
        .panes
        .iter()
        .find(|pane| pane.pane_id.as_deref() == Some(owner))?
        .tab_id
        .as_deref()?;
    msg.result
        .panes
        .iter()
        .find(|pane| {
            pane.focused
                && pane.pane_id.as_deref() != Some(owner)
                && pane.tab_id.as_deref() == Some(tab)
        })?
        .pane_id
        .clone()
}

fn close_legacy_viewer(my_pane_id: &str) {
    let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    if let Some((id, _)) = viewer_pane_in_tab(&json, my_pane_id) {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
    }
}

/// The viewer pane in the same tab, by metadata token, plus whether its
/// heartbeat says it is DEAD (`(pane_id, stale)`).
fn viewer_pane_in_tab(pane_list_json: &str, my_pane_id: &str) -> Option<(String, bool)> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        tab_id: Option<String>,
        label: Option<String>,
        #[serde(default)]
        tokens: serde_json::Map<String, serde_json::Value>,
    }
    let msg: Msg = serde_json::from_str(pane_list_json.trim_start_matches('\u{feff}')).ok()?;
    let panes = &msg.result.panes;
    let my_tab = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(my_pane_id))?
        .tab_id
        .clone()?;
    // Token match finds a live viewer; a "Preview"-labeled pane WITHOUT the
    // token is a resumed corpse (labels survive server restarts, tokens
    // don't) — report it too, with a missing token, so the stale check
    // below flags it and the caller closes it instead of spawning a twin.
    let viewer = panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .find(|p| {
            p.tokens.contains_key(METADATA_SOURCE) || p.label.as_deref() == Some("Preview")
        })?;
    let id = viewer.pane_id.clone()?;
    let now = crate::state::unix_now();
    let stale = viewer
        .tokens
        .get(METADATA_SOURCE)
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ts| now.saturating_sub(ts) > crate::launch::HEARTBEAT_STALE_SECS)
        .unwrap_or(true);
    Some((id, stale))
}

// ---------------------------------------------------------------------------
// Compatibility with park plans written by pre-same-tab builds.
// ---------------------------------------------------------------------------

/// Park plan for `owner`'s tab, recorded beside the control file so either
/// process (sidebar or viewer) can restore.
fn park_path(owner: &str) -> PathBuf {
    scratch_dir().join(format!("herdr-sidebar-preview-{}.park.json", owner.replace(':', "_")))
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
struct RectJ {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ParkPlan {
    /// The tab the panes came from (and go back to).
    tab: String,
    /// Sidebar's share of the tab width at park time, for the re-split.
    owner_ratio: f64,
    /// Parked panes with their ORIGINAL rects, reading order.
    panes: Vec<(String, RectJ)>,
}

/// Bring legacy parked panes home, rebuilding their grid from the recorded rects
/// (each pane re-splits the recorded left/top neighbor at the recorded
/// proportions). Returns whether a plan existed.
pub fn restore_parked(owner: &str) -> bool {
    let path = park_path(owner);
    let Ok(json) = std::fs::read_to_string(&path) else { return false };
    let _ = std::fs::remove_file(&path);
    let Ok(plan) = serde_json::from_str::<ParkPlan>(&json) else { return false };

    let viewer = ipc::call_text("pane.list", serde_json::json!({}))
        .ok()
        .and_then(|l| viewer_pane_in_tab(&l, owner).map(|(id, _)| id));

    let tree = build_tree(&plan.panes);
    // The tree's representative (top-left-most) pane comes home first,
    // splitting whatever holds the region: the preview (which closes right
    // after, handing everything over) or the sidebar itself.
    let (anchor, ratio) = match &viewer {
        Some(v) => (v.clone(), 0.1),
        None => (owner.to_string(), plan.owner_ratio),
    };
    move_into(&plan.tab, rep(&tree), &anchor, "right", ratio);
    replay(&plan.tab, &tree);
    // And the sidebar's own width, which the park inflated.
    let _ = ipc::call_text(
        "layout.set_split_ratio",
        serde_json::json!({ "pane_id": owner, "path": [], "ratio": plan.owner_ratio }),
    );
    true
}

/// A recovered split tree: exactly the rects the panes had at park time.
enum Node {
    Leaf(String),
    Split {
        dir: &'static str,
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// The subtree's representative: its top-left-most pane, which stands in
/// for the whole region until the subtree's own splits are replayed.
fn rep(node: &Node) -> &str {
    match node {
        Node::Leaf(id) => id,
        Node::Split { first, .. } => rep(first),
    }
}

/// Rebuild the split tree from pane rects by guillotine recovery: find a
/// full-height (or full-width) cut line that cleanly partitions the panes,
/// recurse on both sides. Binary-split layouts always admit one; if none is
/// found (foreign layout), fall back to a degenerate right-stack.
fn build_tree(panes: &[(String, RectJ)]) -> Node {
    if panes.len() == 1 {
        return Node::Leaf(panes[0].0.clone());
    }
    let min_x = panes.iter().map(|(_, r)| r.x).min().unwrap_or(0);
    let max_x = panes.iter().map(|(_, r)| r.x + r.width).max().unwrap_or(0);
    let min_y = panes.iter().map(|(_, r)| r.y).min().unwrap_or(0);
    let max_y = panes.iter().map(|(_, r)| r.y + r.height).max().unwrap_or(0);

    // Vertical cut candidates: every pane's left edge strictly inside.
    for (_, r) in panes {
        let cut = r.x;
        if cut <= min_x || cut >= max_x {
            continue;
        }
        if panes.iter().all(|(_, q)| q.x + q.width <= cut || q.x >= cut) {
            let (a, b): (Vec<_>, Vec<_>) =
                panes.iter().cloned().partition(|(_, q)| q.x + q.width <= cut);
            if !a.is_empty() && !b.is_empty() {
                return Node::Split {
                    dir: "right",
                    ratio: (cut - min_x) as f64 / (max_x - min_x).max(1) as f64,
                    first: Box::new(build_tree(&a)),
                    second: Box::new(build_tree(&b)),
                };
            }
        }
    }
    for (_, r) in panes {
        let cut = r.y;
        if cut <= min_y || cut >= max_y {
            continue;
        }
        if panes.iter().all(|(_, q)| q.y + q.height <= cut || q.y >= cut) {
            let (a, b): (Vec<_>, Vec<_>) =
                panes.iter().cloned().partition(|(_, q)| q.y + q.height <= cut);
            if !a.is_empty() && !b.is_empty() {
                return Node::Split {
                    dir: "down",
                    ratio: (cut - min_y) as f64 / (max_y - min_y).max(1) as f64,
                    first: Box::new(build_tree(&a)),
                    second: Box::new(build_tree(&b)),
                };
            }
        }
    }
    // No clean cut (shouldn't happen for herdr layouts): stack them.
    let rest = build_tree(&panes[1..]);
    Node::Split {
        dir: "right",
        ratio: 0.5,
        first: Box::new(Node::Leaf(panes[0].0.clone())),
        second: Box::new(rest),
    }
}

/// Pre-order replay: at each split, the region is currently held entirely
/// by rep(first); moving rep(second) in with the recorded direction/ratio
/// carves the region correctly before either side's inner splits run.
fn replay(tab: &str, node: &Node) {
    let Node::Split { dir, ratio, first, second } = node else { return };
    move_into(tab, rep(second), rep(first), dir, *ratio);
    replay(tab, first);
    replay(tab, second);
}

fn move_into(tab: &str, pane: &str, target: &str, split: &str, ratio: f64) {
    let _ = ipc::call_text(
        "pane.move",
        serde_json::json!({
            "pane_id": pane,
            "destination": {
                "type": "tab",
                "tab_id": tab,
                "split": split,
                "target_pane_id": target,
                "ratio": ratio.clamp(0.1, 0.9),
            },
            "focus": false,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scratch_dir_is_private_to_the_owning_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "scratch dir must not be group/world readable or writable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_scratch_file_refuses_to_follow_a_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        let dir = scratch_dir();
        let victim = dir.join(format!("aa-victim-{}.txt", std::process::id()));
        let link = dir.join(format!("aa-link-{}.ctl", std::process::id()));
        std::fs::write(&victim, "original victim contents").unwrap();
        let _ = std::fs::remove_file(&link);
        symlink(&victim, &link).unwrap();

        write_scratch_file(&link, "payload").unwrap();

        // The symlink must have been replaced by a real file, and the
        // victim it used to point at must be untouched.
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "payload");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original victim contents");

        let _ = std::fs::remove_file(&victim);
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn requests_roundtrip() {
        let f = file_request(Path::new("C:/x/y.rs"));
        assert_eq!(parse_request(&f), Some(Request::File(PathBuf::from("C:/x/y.rs"))));
        let sf = search_file_request(Path::new("C:/x/y.rs"), 42, "main", false, false, true);
        assert_eq!(
            parse_request(&sf),
            Some(Request::SearchFile {
                path: PathBuf::from("C:/x/y.rs"),
                line: 42,
                query: "main".into(),
                regex: false,
                case_sensitive: false,
                whole_word: true,
            })
        );
        let s = show_request(Path::new("C:/repo"), "stash@{1}", None);
        assert_eq!(
            parse_request(&s),
            Some(Request::Show {
                root: PathBuf::from("C:/repo"),
                spec: "stash@{1}".into(),
                path: None,
            })
        );
        let s = show_request(Path::new("C:/repo"), "a1b2c3d", Some("src/a.rs"));
        assert_eq!(
            parse_request(&s),
            Some(Request::Show {
                root: PathBuf::from("C:/repo"),
                spec: "a1b2c3d".into(),
                path: Some("src/a.rs".into()),
            })
        );
        let d = diff_request(Path::new("C:/repo"), "src/a.rs", "staged");
        assert_eq!(
            parse_request(&d),
            Some(Request::Diff {
                root: PathBuf::from("C:/repo"),
                rel: "src/a.rs".into(),
                kind: "staged".into()
            })
        );
        let d = ref_diff_request(
            Path::new("C:/repo"),
            "parent",
            "commit",
            "src/new.rs",
            Some("old.rs"),
        );
        assert_eq!(
            parse_request(&d),
            Some(Request::RefDiff {
                root: PathBuf::from("C:/repo"),
                old_spec: "parent".into(),
                new_spec: "commit".into(),
                rel: "src/new.rs".into(),
                old_rel: Some("old.rs".into()),
            })
        );
        // Legacy bare path still works.
        assert_eq!(
            parse_request("C:/plain.txt"),
            Some(Request::File(PathBuf::from("C:/plain.txt")))
        );
        assert_eq!(parse_request("  "), None);
    }

    #[test]
    fn go_file_preview_expands_tabs_without_reformatting_source() {
        let path = std::env::temp_dir().join(format!(
            "herdr-sidebar-go-preview-tabs-{}.go",
            std::process::id()
        ));
        let source = "func main() {\n\tif true {\n\t\tprintln(\"ok\")\n\t}\n}\n";
        std::fs::write(&path, source).unwrap();

        let doc = load_file(&path, crate::syntax::DEFAULT_DIFF_THEME);

        assert_eq!(doc.lines[1].to_string(), "    if true {");
        assert_eq!(doc.lines[2].to_string(), "        println(\"ok\")");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn inline_control_keeps_the_pre_zoom_sidebar_width() {
        let request = file_request(Path::new("/tmp/main.rs"));
        let raw = format!("{INLINE_CONTROL_PREFIX}37\n{request}");
        let (width, payload) = control_request(&raw);
        assert_eq!(width, Some(37));
        assert_eq!(
            parse_request(payload),
            Some(Request::File(PathBuf::from("/tmp/main.rs")))
        );
    }

    #[test]
    fn focused_peer_is_scoped_to_the_sidebar_tab() {
        let panes = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","focused":true},
            {"pane_id":"w1:p3","tab_id":"w1:t2"}
        ]}}"#;
        assert_eq!(focused_peer_in_tab(panes, "w1:p1").as_deref(), Some("w1:p2"));
        assert_eq!(focused_peer_in_tab(panes, "w1:p3"), None);

        let owner_focused = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","focused":true},
            {"pane_id":"w1:p2","tab_id":"w1:t1"}
        ]}}"#;
        assert_eq!(focused_peer_in_tab(owner_focused, "w1:p1"), None);
        assert_eq!(focused_peer_in_tab("not json", "w1:p1"), None);
    }

    #[test]
    fn long_diff_rows_wrap_with_gutter_style_and_source_mapping() {
        let row_bg = Color::Rgb(40, 20, 20);
        let word_bg = Color::Rgb(80, 30, 30);
        let code = "abcdefghij-WRAP_TAIL_VISIBLE";
        let long = Line::from(vec![
            Span::raw("12 "),
            Span::raw("▌ "),
            Span::styled(
                code,
                Style::default().fg(Color::White).bg(word_bg),
            ),
        ])
        .style(Style::default().bg(row_bg));
        let short = Line::raw("short");

        let (lines, rows) = reflow_diff_lines(&[long, short], 12);
        let wrapped = rows.iter().take_while(|&&row| row == 0).count();
        assert!(wrapped > 1);
        assert!(lines[..wrapped]
            .iter()
            .skip(1)
            .all(|line| line.to_string().starts_with("     ")));
        let rebuilt = lines[..wrapped]
            .iter()
            .map(|line| line.to_string().chars().skip(5).collect::<String>())
            .map(|chunk| chunk.trim_end().to_string())
            .collect::<String>();
        assert_eq!(rebuilt, code);
        assert!(lines[..wrapped].iter().all(|line| line.width() == 12));
        assert!(lines[..wrapped]
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.style.bg == Some(word_bg)));
        assert!(rows[..wrapped].iter().all(|&row| row == 0));
        assert_eq!(rows[wrapped], 1);
    }

    #[test]
    fn first_change_centers_once_after_diff_reflow() {
        let mut lines = vec![
            Line::from(vec![Span::raw(" 1 "), Span::raw("▌ "), Span::raw("abcdefghij")]),
            Line::raw("context 1"),
            Line::raw("first change"),
        ];
        lines.extend((2..=7).map(|n| Line::raw(format!("context {n}"))));
        let folds = vec![None; lines.len()];
        let mut doc = Doc::new("x.rs".into(), String::new(), lines, false, folds);
        doc.first_change = Some(2);
        doc.request_first_change_center();
        doc.ensure_visual_lines(10);

        assert_eq!(doc.visual_rows.iter().position(|row| *row == 2), Some(3));
        doc.apply_first_change_center(5);
        assert_eq!(doc.scroll, 1);

        doc.scroll = 4;
        doc.apply_first_change_center(5);
        assert_eq!(doc.scroll, 4, "centering must not repeat after user scrolling");
    }

    #[test]
    fn diff_body_click_maps_to_the_scrolled_document_row() {
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 7,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(doc_row_at(Rect::new(10, 5, 20, 4), 10, &mouse), Some(12));
        assert_eq!(doc_row_at(Rect::new(10, 8, 20, 4), 10, &mouse), None);
    }

    #[test]
    fn glow_markdown_returns_styled_spans() {
        // Skip if glow is not installed
        if std::process::Command::new("glow").arg("--version").output().is_err() {
            return;
        }
        let md = "# Heading\n\n**bold** and `code`\n";
        let lines = glow_markdown(md, 80);
        assert!(lines.is_some(), "glow_markdown returned None");
        let lines = lines.unwrap();
        assert!(!lines.is_empty(), "glow_markdown returned empty lines");
        // At least one span must have a non-default style (proof that ANSI was parsed)
        let has_styled = lines.iter().any(|l| {
            l.spans.iter().any(|s| s.style != ratatui::style::Style::default())
        });
        assert!(has_styled, "glow_markdown returned no styled spans — ANSI not parsed");
    }

    #[test]
    fn viewer_lookup_reports_staleness() {
        let now = crate::state::unix_now();
        let json = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 2
        );
        assert_eq!(viewer_pane_in_tab(&json, "w1:p1"), Some(("w1:p2".into(), false)));
        let stale = format!(
            r#"{{"result":{{"panes":[
                {{"pane_id":"w1:p1","tab_id":"w1:t1"}},
                {{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-sidebar-preview":"{}"}}}}
            ]}}}}"#,
            now - 999
        );
        assert_eq!(viewer_pane_in_tab(&stale, "w1:p1"), Some(("w1:p2".into(), true)));
    }
}
