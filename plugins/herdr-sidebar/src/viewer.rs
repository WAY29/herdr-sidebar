//! File contents and git diffs shown beside the sidebar. Opening a document
//! zooms the existing sidebar pane and renders the sidebar plus viewer in one
//! TUI, so the viewer owns the whole editor area without moving the tab's
//! other panes. `q`/Esc closes the viewer and restores the original layout.
//!
//! The tail of this module is the client side — the request handoff shared by
//! both sidebar views.

use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::CellWidth;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::ansi;
use crate::icons::{IconTheme, icon};
use crate::ipc;
use crate::syntax::{DiffTheme, PreviewHighlightStyles};

/// Metadata source/token that marks the viewer pane, so the sidebar can find
/// and reuse it (distinct from the sidebar's own identity tokens).
pub const METADATA_SOURCE: &str = "herdr-sidebar-preview";

/// How often the control file is re-checked while idle.
const POLL: Duration = Duration::from_millis(250);
const SOURCE_LINE_HIGHLIGHT: Duration = Duration::from_secs(3);
const NOTICE_DURATION: Duration = Duration::from_secs(3);

/// Preview size guards: don't slurp huge files into a pane.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

/// Enough room for the viewer to remain useful on a narrow terminal.
const MIN_PREVIEW_WIDTH: u16 = 24;

/// Shared search-hit highlight colors used by both Search results and Preview.
pub const SEARCH_HIGHLIGHT_BG: Color = Color::Rgb(0x51, 0x58, 0x00);
pub const SEARCH_HIGHLIGHT_FG: Color = Color::Rgb(0xff, 0xf6, 0xb0);
const TAB_WIDTH: usize = 4;
const COPY_SELECTION_LABEL: &str = if cfg!(target_os = "macos") {
    " ⌘C Copy "
} else {
    " ^C Copy "
};

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
    FileLine {
        path: PathBuf,
        line: usize,
    },
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
        /// Current-file line requested by a hyperlink, when present.
        line: Option<usize>,
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

/// Control-file payload for a file preview anchored to one source line.
pub fn file_line_request(path: &Path, line: usize) -> String {
    let payload = serde_json::json!({ "path": path, "line": line });
    format!("fileline\t{payload}")
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

/// Control-file payload for a git diff anchored to one current-file line.
pub fn diff_line_request(root: &Path, rel: &str, kind: &str, line: usize) -> String {
    format!("diff\t{}\t{rel}\t{kind}\t{line}", root.display())
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
            let line = parts.next().and_then(|line| line.parse().ok());
            Some(Request::Diff { root, rel, kind, line })
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
        Some("fileline") => {
            #[derive(serde::Deserialize)]
            struct Payload {
                path: PathBuf,
                line: usize,
            }
            let payload: Payload = serde_json::from_str(parts.next()?).ok()?;
            Some(Request::FileLine {
                path: payload.path,
                line: payload.line,
            })
        }
        Some("file") => Some(Request::File(PathBuf::from(parts.next()?))),
        // Legacy: a bare path.
        _ => Some(Request::File(PathBuf::from(raw))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    File {
        line: usize,
    },
    Diff {
        old_line: Option<usize>,
        new_line: Option<usize>,
        kind: crate::diffview::DiffSourceKind,
    },
}

#[derive(Clone, Debug)]
struct DisplayUnit {
    raw_start: usize,
    raw_end: usize,
    cell_start: usize,
    cell_end: usize,
}

#[derive(Clone, Debug)]
struct SelectableRow {
    raw: String,
    kind: SourceKind,
    units: Vec<DisplayUnit>,
}

impl SelectableRow {
    fn new(raw: String, kind: SourceKind) -> Self {
        let mut units = Vec::new();
        let mut cell = 0usize;
        for (raw_start, grapheme) in raw.grapheme_indices(true) {
            let raw_end = raw_start + grapheme.len();
            if grapheme == "\t" {
                let width = TAB_WIDTH - cell % TAB_WIDTH;
                for _ in 0..width {
                    units.push(DisplayUnit {
                        raw_start,
                        raw_end,
                        cell_start: cell,
                        cell_end: cell + 1,
                    });
                    cell += 1;
                }
            } else {
                let width = grapheme.cell_width() as usize;
                units.push(DisplayUnit {
                    raw_start,
                    raw_end,
                    cell_start: cell,
                    cell_end: cell + width,
                });
                cell += width;
            }
        }
        Self { raw, kind, units }
    }

    fn file(raw: &str, line: usize) -> Self {
        Self::new(raw.to_string(), SourceKind::File { line })
    }

    fn diff(source: crate::diffview::DiffSource) -> Self {
        Self::new(
            source.text,
            SourceKind::Diff {
                old_line: source.old_line,
                new_line: source.new_line,
                kind: source.kind,
            },
        )
    }

    fn byte_column(&self, byte: usize) -> usize {
        self.raw[..byte.min(self.raw.len())].graphemes(true).count()
    }
}

#[derive(Clone, Copy, Debug)]
struct VisualFragment {
    source_row: usize,
    unit_start: usize,
    unit_end: usize,
    content_x: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    row: usize,
    byte: usize,
}

#[derive(Clone, Copy, Debug)]
struct HitPoint {
    before: TextPoint,
    after: TextPoint,
}

#[derive(Clone, Copy, Debug)]
struct TextSelection {
    anchor: TextPoint,
    cursor: TextPoint,
}

impl TextSelection {
    fn normalized(self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    fn contains_hit(self, hit: HitPoint) -> bool {
        let (start, end) = self.normalized();
        hit.before < end && start < hit.after
    }
}

struct SelectionExport {
    snippet: String,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
    removed: bool,
    range: TextSelection,
}

struct PendingComment {
    location: String,
    snippet: String,
    removed: bool,
    document: Request,
    range: TextSelection,
}

struct SavedComment {
    pending: PendingComment,
    body: String,
}

struct CommentDraft {
    pending: PendingComment,
    input: String,
    caret: usize,
}

impl CommentDraft {
    fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.caret, &text);
        self.caret += text.len();
    }

    fn move_left(&mut self) {
        self.caret = previous_grapheme_boundary(&self.input, self.caret);
    }

    fn move_right(&mut self) {
        self.caret = next_grapheme_boundary(&self.input, self.caret);
    }

    fn backspace(&mut self) {
        let previous = previous_grapheme_boundary(&self.input, self.caret);
        if previous < self.caret {
            self.input.replace_range(previous..self.caret, "");
            self.caret = previous;
        }
    }

    fn delete(&mut self) {
        let next = next_grapheme_boundary(&self.input, self.caret);
        if next > self.caret {
            self.input.replace_range(self.caret..next, "");
        }
    }

    fn line_start(&self) -> usize {
        self.input[..self.caret]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.input[self.caret..]
            .find('\n')
            .map_or(self.input.len(), |index| self.caret + index)
    }

    fn move_vertical(&mut self, down: bool) {
        let start = self.line_start();
        let column = self.input[start..self.caret].graphemes(true).count();
        let target = if down {
            let end = self.line_end();
            (end < self.input.len()).then(|| {
                let target_start = end + 1;
                let target_end = self.input[target_start..]
                    .find('\n')
                    .map_or(self.input.len(), |index| target_start + index);
                (target_start, target_end)
            })
        } else if start > 0 {
            let target_end = start - 1;
            let target_start = self.input[..target_end]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            Some((target_start, target_end))
        } else {
            None
        };
        if let Some((target_start, target_end)) = target {
            self.caret = self.input[target_start..target_end]
                .grapheme_indices(true)
                .nth(column)
                .map_or(target_end, |(offset, _)| target_start + offset);
        }
    }
}

#[derive(Clone)]
struct AgentTarget {
    pane_id: String,
    label: String,
}

struct AgentPicker {
    agents: Vec<AgentTarget>,
    selected: usize,
    scroll: usize,
    rect: Rect,
}

struct CommentHover {
    index: usize,
    position: Position,
    until: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterAction {
    Comment,
    CopySelection,
    CopyComments,
    Send,
    ClearFileComments,
}

fn footer_entries(
    selected: bool,
    has_comments: bool,
) -> Vec<(FooterAction, &'static str)> {
    if selected {
        vec![
            (FooterAction::Comment, " c Comment "),
            (FooterAction::CopySelection, COPY_SELECTION_LABEL),
        ]
    } else if has_comments {
        vec![
            (FooterAction::CopyComments, " y Copy "),
            (FooterAction::Send, " s Send Agent "),
        ]
    } else {
        Vec::new()
    }
}

fn is_copy_shortcut(key: &KeyEvent) -> bool {
    let modifier = if cfg!(target_os = "macos") {
        KeyModifiers::SUPER
    } else {
        KeyModifiers::CONTROL
    };
    key.code == KeyCode::Char('c')
        && key.modifiers == modifier
}

fn previous_grapheme_boundary(text: &str, caret: usize) -> usize {
    text[..caret]
        .grapheme_indices(true)
        .next_back()
        .map_or(caret, |(index, _)| index)
}

fn next_grapheme_boundary(text: &str, caret: usize) -> usize {
    text[caret..]
        .grapheme_indices(true)
        .next()
        .map_or(caret, |(_, grapheme)| caret + grapheme.len())
}

struct SelectedPart<'a> {
    row: &'a SelectableRow,
    from: usize,
    to: usize,
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
    visual_fragments: Vec<VisualFragment>,
    visual_width: u16,
    sources: Vec<Option<SelectableRow>>,
    selection: Option<TextSelection>,
    first_change: Option<usize>,
    center_first_change_pending: bool,
    center_source_row_pending: Option<usize>,
    highlighted_source_row: Option<(usize, Instant)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ViewAnchor {
    raw: String,
    kind: SourceKind,
    unit_start: usize,
    screen_row: usize,
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
            visual_fragments: Vec::new(),
            visual_width: 0,
            sources: Vec::new(),
            selection: None,
            first_change: None,
            center_first_change_pending: false,
            center_source_row_pending: None,
            highlighted_source_row: None,
        }
    }

    fn with_sources(mut self, sources: Vec<Option<SelectableRow>>) -> Self {
        debug_assert_eq!(self.lines.len(), sources.len());
        self.sources = sources;
        self
    }

    fn ensure_visual_lines(&mut self, width: u16) {
        let width = width.max(1);
        if self.folds.is_empty() || self.visual_width == width {
            return;
        }
        (self.visual_lines, self.visual_rows, self.visual_fragments) =
            reflow_diff_lines(&self.lines, &self.sources, width);
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

    fn view_anchor(&mut self, width: u16, height: u16) -> Option<ViewAnchor> {
        self.ensure_visual_lines(width);
        let end = self
            .scroll
            .saturating_add(usize::from(height))
            .min(self.visual_len());
        for visual_row in self.scroll..end {
            let Some(fragment) = self.fragment_at(visual_row) else {
                continue;
            };
            let Some(source) = self.sources.get(fragment.source_row).and_then(Option::as_ref)
            else {
                continue;
            };
            return Some(ViewAnchor {
                raw: source.raw.clone(),
                kind: source.kind,
                unit_start: fragment.unit_start,
                screen_row: visual_row - self.scroll,
            });
        }
        None
    }

    fn restore_view_anchor(&mut self, anchor: &ViewAnchor, width: u16) -> bool {
        self.ensure_visual_lines(width);
        let Some(source_row) = self.sources.iter().position(|source| {
            source
                .as_ref()
                .is_some_and(|source| source.kind == anchor.kind && source.raw == anchor.raw)
        }) else {
            return false;
        };
        let visual_row = if self.visual_width == 0 {
            source_row
        } else {
            let Some(visual_row) = self.visual_fragments.iter().position(|fragment| {
                fragment.source_row == source_row && fragment.unit_start == anchor.unit_start
            }) else {
                return false;
            };
            visual_row
        };
        self.scroll = visual_row.saturating_sub(anchor.screen_row);
        true
    }

    fn number_width(&self) -> usize {
        self.lines.len().max(1).to_string().len()
    }

    fn fragment_at(&self, visual_row: usize) -> Option<VisualFragment> {
        if self.visual_width == 0 {
            let row = self.sources.get(visual_row)?.as_ref()?;
            Some(VisualFragment {
                source_row: visual_row,
                unit_start: 0,
                unit_end: row.units.len(),
                content_x: 0,
            })
        } else {
            self.visual_fragments.get(visual_row).copied()
        }
    }

    fn hit_point(&self, visual_row: usize, column: usize) -> Option<HitPoint> {
        let fragment = self.fragment_at(visual_row)?;
        let source = self.sources.get(fragment.source_row)?.as_ref()?;
        let content_x = if self.numbered {
            self.number_width() + 1
        } else {
            fragment.content_x
        };
        let local = column.saturating_sub(content_x);
        let first_cell = source
            .units
            .get(fragment.unit_start)
            .map_or(0, |unit| unit.cell_start);
        let units = source.units.get(fragment.unit_start..fragment.unit_end)?;
        let unit = units.iter().find(|unit| {
            let start = unit.cell_start.saturating_sub(first_cell);
            let end = unit.cell_end.saturating_sub(first_cell);
            local < end.max(start + 1)
        });
        let (before, after) = if column < content_x {
            let byte = units.first().map_or(0, |unit| unit.raw_start);
            (byte, byte)
        } else if let Some(unit) = unit {
            (unit.raw_start, unit.raw_end)
        } else {
            let byte = units.last().map_or(source.raw.len(), |unit| unit.raw_end);
            (byte, byte)
        };
        Some(HitPoint {
            before: TextPoint {
                row: fragment.source_row,
                byte: before,
            },
            after: TextPoint {
                row: fragment.source_row,
                byte: after,
            },
        })
    }

    fn begin_selection(&mut self, hit: HitPoint) {
        self.selection = Some(TextSelection {
            anchor: hit.before,
            cursor: hit.before,
        });
    }

    fn drag_selection(&mut self, hit: HitPoint) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        let mut cursor = if hit.before >= selection.anchor {
            hit.after
        } else {
            hit.before
        };
        if cursor.row > selection.anchor.row {
            if let Some(blocked) = self.sources[selection.anchor.row + 1..=cursor.row]
                .iter()
                .position(Option::is_none)
            {
                let row = selection.anchor.row + blocked;
                let byte = self.sources[row]
                    .as_ref()
                    .map_or(0, |source| source.raw.len());
                cursor = TextPoint { row, byte };
            }
        } else if cursor.row < selection.anchor.row
            && let Some(blocked) = self.sources[cursor.row..selection.anchor.row]
                .iter()
                .rposition(Option::is_none)
        {
            cursor = TextPoint {
                row: cursor.row + blocked + 1,
                byte: 0,
            };
        }
        selection.cursor = cursor;
    }

    fn clear_selection(&mut self) {
        self.selection = None;
    }

    fn selected_range(&self) -> Option<(TextPoint, TextPoint)> {
        let range = self.selection?.normalized();
        (range.0 != range.1).then_some(range)
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selected_range()?;
        let mut out = String::new();
        for row_index in start.row..=end.row {
            let row = self.sources.get(row_index)?.as_ref()?;
            let from = if row_index == start.row {
                start.byte
            } else {
                0
            };
            let to = if row_index == end.row {
                end.byte
            } else {
                row.raw.len()
            };
            out.push_str(row.raw.get(from..to)?);
            if row_index != end.row {
                out.push('\n');
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn selection_export(&self) -> Option<SelectionExport> {
        let (start, end) = self.selected_range()?;
        let mut parts = Vec::new();
        let mut plain = String::new();
        let mut snippet = String::new();
        for row_index in start.row..=end.row {
            let row = self.sources.get(row_index)?.as_ref()?;
            let from = if row_index == start.row {
                start.byte
            } else {
                0
            };
            let to = if row_index == end.row {
                end.byte
            } else {
                row.raw.len()
            };
            let text = row.raw.get(from..to)?;
            plain.push_str(text);
            match row.kind {
                SourceKind::File { .. } => snippet.push_str(text),
                SourceKind::Diff { kind, .. } => {
                    snippet.push(match kind {
                        crate::diffview::DiffSourceKind::Context => ' ',
                        crate::diffview::DiffSourceKind::Deleted => '-',
                        crate::diffview::DiffSourceKind::Added => '+',
                    });
                    snippet.push_str(text);
                }
            }
            parts.push(SelectedPart { row, from, to });
            if row_index != end.row {
                plain.push('\n');
                snippet.push('\n');
            }
        }
        if plain.is_empty() {
            return None;
        }

        let preferred: Vec<&SelectedPart<'_>> = parts
            .iter()
            .filter(|part| {
                !matches!(
                    part.row.kind,
                    SourceKind::Diff {
                        kind: crate::diffview::DiffSourceKind::Deleted,
                        ..
                    }
                )
            })
            .collect();
        let removed = preferred.is_empty()
            && parts
                .iter()
                .any(|part| matches!(part.row.kind, SourceKind::Diff { .. }));
        let located: Vec<&SelectedPart<'_>> = if preferred.is_empty() {
            parts.iter().collect()
        } else {
            preferred
        };
        let first = *located.first()?;
        let last = *located.last()?;
        let line_of = |row: &SelectableRow| match row.kind {
            SourceKind::File { line } => Some(line),
            SourceKind::Diff {
                old_line,
                new_line,
                kind,
            } => match kind {
                crate::diffview::DiffSourceKind::Deleted => old_line,
                _ => new_line.or(old_line),
            },
        };
        let start_column = first.row.byte_column(first.from) + 1;
        let end_column = if std::ptr::eq(first, last) {
            first.row.byte_column(first.to).max(start_column)
        } else {
            last.row.byte_column(last.to).max(1)
        };

        Some(SelectionExport {
            snippet,
            start_line: line_of(first.row)?,
            start_column,
            end_line: line_of(last.row)?,
            end_column,
            removed,
            range: TextSelection {
                anchor: start,
                cursor: end,
            },
        })
    }

    fn selection_cells(&self, visual_row: usize) -> Option<(usize, usize)> {
        self.range_cells(visual_row, self.selection?)
    }

    fn range_cells(&self, visual_row: usize, range: TextSelection) -> Option<(usize, usize)> {
        let (start, end) = range.normalized();
        let fragment = self.fragment_at(visual_row)?;
        if fragment.source_row < start.row || fragment.source_row > end.row {
            return None;
        }
        let source = self.sources.get(fragment.source_row)?.as_ref()?;
        let from = if fragment.source_row == start.row {
            start.byte
        } else {
            0
        };
        let to = if fragment.source_row == end.row {
            end.byte
        } else {
            source.raw.len()
        };
        if from >= to {
            return None;
        }
        let units = source.units.get(fragment.unit_start..fragment.unit_end)?;
        let first_cell = units.first()?.cell_start;
        let mut selected = units
            .iter()
            .filter(|unit| unit.raw_end > from && unit.raw_start < to);
        let first = selected.next()?;
        let last = selected.next_back().unwrap_or(first);
        Some((
            fragment.content_x + first.cell_start.saturating_sub(first_cell),
            fragment.content_x + last.cell_end.saturating_sub(first_cell),
        ))
    }

    fn request_first_change_center(&mut self) {
        self.center_first_change_pending = self.first_change.is_some();
    }

    fn request_source_line(&mut self, line: usize, highlight: bool) {
        let row = line
            .saturating_sub(1)
            .min(self.lines.len().saturating_sub(1));
        self.center_source_row_pending = Some(row);
        self.highlighted_source_row =
            highlight.then(|| (row, Instant::now() + SOURCE_LINE_HIGHLIGHT));
    }

    fn request_diff_line(&mut self, line: usize, highlight: bool) -> bool {
        let Some(row) = self.sources.iter().position(|source| {
            source.as_ref().is_some_and(|source| {
                matches!(source.kind, SourceKind::Diff { new_line: Some(found), .. } if found == line)
            })
        }) else {
            return false;
        };
        self.center_source_row_pending = Some(row);
        self.highlighted_source_row =
            highlight.then(|| (row, Instant::now() + SOURCE_LINE_HIGHLIGHT));
        true
    }

    fn apply_source_line_center(&mut self, height: u16) {
        let Some(source_row) = self.center_source_row_pending.take() else {
            return;
        };
        self.center_on_source_row(source_row, height);
    }

    fn apply_first_change_center(&mut self, height: u16) {
        if !std::mem::take(&mut self.center_first_change_pending) {
            return;
        }
        let Some(source_row) = self.first_change else { return };
        self.center_on_source_row(source_row, height);
    }

    fn center_on_source_row(&mut self, source_row: usize, height: u16) {
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

    fn active_source_highlight(&self) -> Option<usize> {
        self.highlighted_source_row
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(row, _)| row)
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
    sources: &[Option<SelectableRow>],
    width: u16,
) -> (Vec<Line<'static>>, Vec<usize>, Vec<VisualFragment>) {
    let mut visual_lines = Vec::new();
    let mut visual_rows = Vec::new();
    let mut visual_fragments = Vec::new();
    for (source_row, line) in lines.iter().enumerate() {
        let wrapped = wrap_diff_line(line, width);
        visual_rows.extend(std::iter::repeat_n(source_row, wrapped.len()));
        if let Some(source) = sources.get(source_row).and_then(Option::as_ref) {
            let indent = line.spans.iter().take(2).map(Span::width).sum::<usize>();
            let content_width = usize::from(width).saturating_sub(indent).max(1);
            let mut start = 0usize;
            let mut used = 0usize;
            for (index, unit) in source.units.iter().enumerate() {
                let unit_width = unit.cell_end.saturating_sub(unit.cell_start);
                if index > start && unit_width > 0 && used + unit_width > content_width {
                    visual_fragments.push(VisualFragment {
                        source_row,
                        unit_start: start,
                        unit_end: index,
                        content_x: indent,
                    });
                    start = index;
                    used = 0;
                }
                used += unit_width;
            }
            visual_fragments.push(VisualFragment {
                source_row,
                unit_start: start,
                unit_end: source.units.len(),
                content_x: indent,
            });
            debug_assert_eq!(
                visual_fragments
                    .iter()
                    .rev()
                    .take_while(|row| row.source_row == source_row)
                    .count(),
                wrapped.len()
            );
        } else {
            visual_fragments.extend(std::iter::repeat_n(
                VisualFragment {
                    source_row,
                    unit_start: 0,
                    unit_end: 0,
                    content_x: 0,
                },
                wrapped.len(),
            ));
        }
        visual_lines.extend(wrapped);
    }
    (visual_lines, visual_rows, visual_fragments)
}

fn load(
    request: &Request,
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: &HashSet<crate::diffview::FoldId>,
) -> Doc {
    match request {
        Request::File(path) => load_file(path, diff_theme),
        Request::FileLine { path, line } => load_file_at(path, *line, diff_theme),
        Request::SearchFile { path, line, query, regex, case_sensitive, whole_word } => {
            load_search_file(path, *line, query, *regex, *case_sensitive, *whole_word, diff_theme)
        }
        Request::Diff { root, rel, kind, .. } => {
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

fn prepare_new_request(doc: &mut Doc, request: &Request, hide_unmodified: bool) {
    match request {
        Request::FileLine { line, .. } => doc.request_source_line(*line, true),
        Request::SearchFile { line, .. } => doc.request_source_line(*line, false),
        Request::Diff { line: Some(line), .. } => {
            if !doc.request_diff_line(*line, true) {
                doc.request_first_change_center();
            }
        }
        Request::Diff { .. } | Request::RefDiff { .. } if !hide_unmodified => {
            doc.request_first_change_center();
        }
        _ => {}
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
    load_file_inner(target, diff_theme, None, None)
}

fn load_file_at(target: &Path, line: usize, diff_theme: DiffTheme) -> Doc {
    load_file_inner(target, diff_theme, None, Some(line))
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
    load_file_inner(
        target,
        diff_theme,
        Some((query, regex, case_sensitive, whole_word)),
        Some(line),
    )
}

fn load_file_inner(
    target: &Path,
    diff_theme: DiffTheme,
    search: Option<(&str, bool, bool, bool)>,
    source_line: Option<usize>,
) -> Doc {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.display().to_string());
    let lower = name.to_lowercase();
    let is_markdown = lower.ends_with(".md") || lower.ends_with(".markdown");
    let (lines, numbered, sources) = match std::fs::read(target) {
        Err(e) => (
            vec![Line::raw(format!("(unreadable: {e})"))],
            true,
            vec![None],
        ),
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8192)];
            if head.contains(&0) {
                (
                    vec![Line::raw(format!("(binary file — {} bytes)", bytes.len()))],
                    false,
                    vec![None],
                )
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
                let glow_rendered = (search_re.is_none() && source_line.is_none())
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
                let mut sources: Vec<Option<SelectableRow>> = if numbered {
                    text.lines()
                        .take(MAX_LINES)
                        .enumerate()
                        .map(|(index, raw)| Some(SelectableRow::file(raw, index + 1)))
                        .collect()
                } else {
                    vec![None; lines.len()]
                };
                if let Some(search_re) = search_re.as_ref() {
                    apply_search_highlights(&mut lines, &text, search_re);
                }
                for line in &mut lines {
                    line.spans = crate::syntax::expand_tabs(std::mem::take(&mut line.spans));
                }
                if truncated || text.lines().count() > MAX_LINES {
                    lines.push(Line::raw("… (truncated)"));
                    sources.push(None);
                }
                if lines.is_empty() {
                    lines.push(Line::raw("(empty file)"));
                    sources.push(None);
                }
                sources.resize(lines.len(), None);
                (lines, numbered, sources)
            }
        }
    };
    Doc::new(
        name,
        target.display().to_string(),
        lines,
        numbered,
        Vec::new(),
    )
    .with_sources(sources)
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

    let (lines, folds, first_change, sources) = run_structured_diff(
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
    )
    .with_sources(sources);
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
    let (lines, folds, first_change, sources) = run_structured_diff(
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
    )
    .with_sources(sources);
    doc.first_change = first_change;
    doc
}

type StructuredDiffDoc = (
    Vec<Line<'static>>,
    Vec<Option<crate::diffview::FoldId>>,
    Option<usize>,
    Vec<Option<SelectableRow>>,
);

fn run_structured_diff(
    root: &Path,
    rel: &str,
    args: &[String],
    diff_theme: DiffTheme,
    hide_unmodified: bool,
    expanded_folds: &HashSet<crate::diffview::FoldId>,
) -> StructuredDiffDoc {
    match std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
    {
        Err(e) => (
            vec![Line::raw(format!("(git failed: {e})"))],
            Vec::new(),
            None,
            vec![None],
        ),
        Ok(out) => {
            // --no-index exits 1 when the files differ; that's not an error.
            let text = String::from_utf8_lossy(&out.stdout);
            if text.trim().is_empty() {
                let err = String::from_utf8_lossy(&out.stderr);
                if err.trim().is_empty() {
                    (
                        vec![Line::raw("(no changes)")],
                        Vec::new(),
                        None,
                        vec![None],
                    )
                } else {
                    (
                        vec![Line::raw(format!("({})", err.trim()))],
                        Vec::new(),
                        None,
                        vec![None],
                    )
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
                    rendered.sources.truncate(MAX_LINES);
                    rendered.first_change = rendered.first_change.filter(|row| *row < MAX_LINES);
                    rendered.lines.push(Line::raw("… (truncated)"));
                    rendered.folds.push(None);
                    rendered.sources.push(None);
                }
                (
                    rendered.lines,
                    rendered.folds,
                    rendered.first_change,
                    rendered
                        .sources
                        .into_iter()
                        .map(|source| source.map(SelectableRow::diff))
                        .collect(),
                )
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

fn request_path(request: &Request) -> Option<String> {
    fn shown(path: &Path) -> String {
        let relative = std::env::current_dir()
            .ok()
            .and_then(|cwd| path.strip_prefix(cwd).ok())
            .unwrap_or(path);
        relative.to_string_lossy().replace('\\', "/")
    }

    Some(match request {
        Request::File(path) | Request::FileLine { path, .. } | Request::SearchFile { path, .. } => {
            shown(path)
        }
        Request::Diff { rel, .. } | Request::RefDiff { rel, .. } => rel.replace('\\', "/"),
        Request::Show { .. } => return None,
    })
}

fn request_file_path(request: &Request) -> Option<PathBuf> {
    match request {
        Request::File(path) | Request::FileLine { path, .. } | Request::SearchFile { path, .. } => {
            Some(path.clone())
        }
        Request::Diff { root, rel, .. } | Request::RefDiff { root, rel, .. } => {
            Some(root.join(rel))
        }
        Request::Show { root, path, .. } => path.as_ref().map(|path| root.join(path)),
    }
}

fn same_preview_file(left: &Request, right: &Request) -> bool {
    request_file_path(left).is_some_and(|left| request_file_path(right) == Some(left))
}

fn remove_comments_for_file(comments: &mut Vec<SavedComment>, request: &Request) -> usize {
    let before = comments.len();
    comments.retain(|comment| !same_preview_file(&comment.pending.document, request));
    before - comments.len()
}

fn pending_comment(request: &Request, selection: SelectionExport) -> Option<PendingComment> {
    let path = request_path(request)?;
    Some(PendingComment {
        location: format!(
            "{path}:{}:{}-{}:{}",
            selection.start_line, selection.start_column, selection.end_line, selection.end_column
        ),
        snippet: selection.snippet,
        removed: selection.removed,
        document: request.clone(),
        range: selection.range,
    })
}

fn format_comments(comments: &[SavedComment]) -> String {
    let mut out = String::new();
    for (index, comment) in comments.iter().enumerate() {
        if index > 0 {
            out.push_str("\n\n");
        }
        out.push_str("## `");
        out.push_str(&comment.pending.location.replace('`', "\\`"));
        out.push('`');
        if comment.pending.removed {
            out.push_str(" (removed)");
        }
        out.push_str("\n\n");
        let longest_ticks = comment
            .pending
            .snippet
            .split(|ch| ch != '`')
            .map(str::len)
            .max()
            .unwrap_or(0);
        let fence = "`".repeat(longest_ticks.saturating_add(1).max(3));
        out.push_str(&fence);
        out.push('\n');
        out.push_str(&comment.pending.snippet);
        if !comment.pending.snippet.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&fence);
        out.push_str("\n\n");
        out.push_str(comment.body.trim());
    }
    out
}

fn export_comments_with(
    comments: &mut Vec<SavedComment>,
    export: impl FnOnce(&str) -> Result<(), String>,
) -> Result<usize, String> {
    let text = format_comments(comments);
    export(&text)?;
    let count = comments.len();
    comments.clear();
    Ok(count)
}

fn pipe_clipboard(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    child
        .stdin
        .take()
        .ok_or_else(|| "clipboard stdin unavailable".to_string())?
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    let status = child.wait().map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{program} failed"))
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        pipe_clipboard("pbcopy", &[], text)
    }
    #[cfg(windows)]
    {
        pipe_clipboard("clip.exe", &[], text)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let choices: [(&str, &[&str]); 3] = [
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        let mut last = None;
        for (program, args) in choices {
            match pipe_clipboard(program, args, text) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| "no clipboard command found".to_string()))
    }
}

fn api_result(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let raw = ipc::call_text(method, params).map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|error| format!("invalid Herdr response: {error}"))?;
    if let Some(error) = value.get("error") {
        return Err(error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| "Herdr response had no result".to_string())
}

fn agent_targets_from_value(
    result: serde_json::Value,
    owner: &str,
) -> Result<Vec<AgentTarget>, String> {
    #[derive(serde::Deserialize)]
    struct PaneList {
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: String,
        tab_id: String,
        agent: Option<String>,
        display_agent: Option<String>,
        title: Option<String>,
        terminal_title_stripped: Option<String>,
    }
    let panes: PaneList = serde_json::from_value(result).map_err(|error| error.to_string())?;
    let tab_id = panes
        .panes
        .iter()
        .find(|pane| pane.pane_id == owner)
        .map(|pane| pane.tab_id.clone())
        .ok_or_else(|| "Sidebar pane is no longer present".to_string())?;
    Ok(panes
        .panes
        .into_iter()
        .filter(|pane| pane.tab_id == tab_id && pane.pane_id != owner && pane.agent.is_some())
        .map(|pane| {
            let label = pane
                .title
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    pane.terminal_title_stripped
                        .filter(|value| !value.trim().is_empty())
                })
                .or(pane.display_agent)
                .or(pane.agent)
                .unwrap_or_else(|| pane.pane_id.clone());
            AgentTarget {
                pane_id: pane.pane_id,
                label,
            }
        })
        .collect())
}

fn agent_targets(owner: &str) -> Result<Vec<AgentTarget>, String> {
    let result = api_result("pane.list", serde_json::json!({}))?;
    agent_targets_from_value(result, owner)
}

fn bracketed_paste(text: &str) -> String {
    const START: &str = "\x1b[200~";
    const END: &str = "\x1b[201~";
    let mut safe = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let start = rest.find(START);
        let end = rest.find(END);
        let Some(index) = (match (start, end) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }) else {
            safe.push_str(rest);
            break;
        };
        safe.push_str(&rest[..index]);
        rest = &rest[index + START.len()..];
    }
    format!("{START}{safe}{END}")
}

fn send_to_agent(agent: &AgentTarget, text: &str) -> Result<(), String> {
    api_result(
        "pane.send_text",
        serde_json::json!({
            "pane_id": agent.pane_id,
            "text": bracketed_paste(text),
        }),
    )?;
    Ok(())
}

fn editor_rows(input: &str, caret: usize, width: usize) -> (Vec<Line<'static>>, usize, usize) {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    let mut cursor = None;
    for (byte, grapheme) in input.grapheme_indices(true) {
        if grapheme == "\n" {
            if byte == caret {
                cursor = Some((rows.len(), used));
            }
            rows.push(Line::raw(std::mem::take(&mut line)));
            used = 0;
            continue;
        }
        let mut grapheme_width = if grapheme == "\t" {
            (TAB_WIDTH - used % TAB_WIDTH).min(width)
        } else {
            grapheme.cell_width() as usize
        };
        if used > 0 && grapheme_width > 0 && used + grapheme_width > width {
            rows.push(Line::raw(std::mem::take(&mut line)));
            used = 0;
            if grapheme == "\t" {
                grapheme_width = TAB_WIDTH.min(width);
            }
        }
        if byte == caret {
            cursor = Some((rows.len(), used));
        }
        if grapheme == "\t" {
            line.push_str(&" ".repeat(grapheme_width));
        } else {
            line.push_str(grapheme);
        }
        used += grapheme_width;
    }
    if caret == input.len() {
        cursor = Some((rows.len(), used));
    }
    rows.push(Line::raw(line));
    let (cursor_row, cursor_column) = cursor.unwrap_or((rows.len().saturating_sub(1), 0));
    (rows, cursor_row, cursor_column)
}

fn draw_comment_editor(frame: &mut Frame, area: Rect, draft: &CommentDraft, notice: Option<&str>) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let width = area.width.saturating_sub(4).clamp(4, 72);
    let height = area.height.saturating_sub(2).clamp(4, 9);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let block = Block::bordered()
        .title(format!(" Comment · {} ", draft.pending.location))
        .border_style(Style::default().fg(Color::LightBlue));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }
    let input_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let footer = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    let (rows, cursor_row, cursor_column) =
        editor_rows(&draft.input, draft.caret, usize::from(input_area.width));
    let viewport = usize::from(input_area.height).max(1);
    let top = cursor_row.saturating_sub(viewport.saturating_sub(1));
    frame.render_widget(
        Paragraph::new(
            rows.into_iter()
                .skip(top)
                .take(viewport)
                .collect::<Vec<_>>(),
        ),
        input_area,
    );
    frame.render_widget(
        Paragraph::new(if let Some(notice) = notice {
            notice.to_string().red()
        } else if inner.width < 36 {
            " Enter save · ^J".dim()
        } else {
            " Enter save · ^J newline · Esc cancel".dim()
        }),
        footer,
    );
    if input_area.height > 0 && cursor_row < top + viewport {
        frame.set_cursor_position(Position::new(
            input_area.x
                + u16::try_from(cursor_column)
                    .unwrap_or(u16::MAX)
                    .min(input_area.width.saturating_sub(1)),
            input_area.y + u16::try_from(cursor_row - top).unwrap_or(0),
        ));
    }
}

fn draw_comment_tooltip(
    frame: &mut Frame,
    bounds: Rect,
    anchor: Position,
    comment: &SavedComment,
    border_color: Color,
) {
    if bounds.width < 16 || bounds.height < 4 {
        return;
    }
    let max_width = bounds.width.min(60);
    let desired = comment
        .body
        .lines()
        .map(|line| line.cell_width())
        .chain(std::iter::once(comment.pending.location.cell_width()))
        .max()
        .unwrap_or(12)
        .saturating_add(4);
    let width = desired.clamp(16, max_width);
    let mut lines = vec![Line::styled(
        format!(" {}", comment.pending.location),
        Style::default().dim(),
    )];
    lines.extend(
        crate::ui::wrap_footer_message(&comment.body, width.saturating_sub(2), 0)
            .into_iter()
            .map(Line::from),
    );
    lines.truncate(usize::from(bounds.height.saturating_sub(2)));
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(bounds.height);
    let right = bounds.x.saturating_add(bounds.width);
    let bottom = bounds.y.saturating_add(bounds.height);
    let x = anchor
        .x
        .saturating_add(1)
        .min(right.saturating_sub(width))
        .max(bounds.x);
    let below = anchor.y.saturating_add(1);
    let y = if below.saturating_add(height) <= bottom {
        below
    } else {
        anchor.y.saturating_sub(height).max(bounds.y)
    };
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color))
                .title(" Comment "),
        ),
        area,
    );
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
    dragging_selection: bool,
    selection_dragged: bool,
    comments: Vec<SavedComment>,
    draft: Option<CommentDraft>,
    agent_picker: Option<AgentPicker>,
    footer_actions: Vec<(Rect, FooterAction)>,
    footer_hover: Option<FooterAction>,
    comment_hover: Option<CommentHover>,
    notice: Option<(String, Instant)>,
    last_refresh: Instant,
}

impl InlinePreview {
    pub fn for_current_pane() -> Self {
        let owner = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty());
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
            dragging_selection: false,
            selection_dragged: false,
            comments: Vec::new(),
            draft: None,
            agent_picker: None,
            footer_actions: Vec::new(),
            footer_hover: None,
            comment_hover: None,
            notice: None,
            last_refresh: Instant::now(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.doc.is_some()
    }

    fn set_notice(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now() + NOTICE_DURATION));
    }

    fn start_comment(&mut self) {
        let Some(request) = self.current.as_ref() else {
            return;
        };
        let Some(selection) = self.doc.as_ref().and_then(Doc::selection_export) else {
            self.set_notice("Select text first");
            return;
        };
        let Some(pending) = pending_comment(request, selection) else {
            self.set_notice("Comments unavailable");
            return;
        };
        self.notice = None;
        self.draft = Some(CommentDraft {
            pending,
            input: String::new(),
            caret: 0,
        });
        self.agent_picker = None;
        let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    }

    fn cancel_comment(&mut self) {
        if self.draft.take().is_some() {
            let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
            self.notice = None;
        }
    }

    fn save_comment(&mut self) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        if draft.input.trim().is_empty() {
            self.draft = Some(draft);
            self.set_notice("Comment is empty");
            return;
        }
        self.comments.push(SavedComment {
            pending: draft.pending,
            body: draft.input,
        });
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        if let Some(doc) = self.doc.as_mut() {
            doc.clear_selection();
        }
        self.notice = None;
    }

    fn current_comment_count(&self) -> usize {
        let Some(current) = self.current.as_ref() else {
            return 0;
        };
        self.comments
            .iter()
            .filter(|comment| same_preview_file(&comment.pending.document, current))
            .count()
    }

    fn clear_current_file_comments(&mut self) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        let removed = remove_comments_for_file(&mut self.comments, current);
        self.comment_hover = None;
        if removed == 0 {
            self.set_notice("No comments in this file");
        } else {
            self.set_notice(format!(
                "Cleared {removed} comment{}",
                if removed == 1 { "" } else { "s" }
            ));
        }
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.doc.as_ref().and_then(Doc::selected_text) else {
            self.set_notice("No text selected");
            return;
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self.set_notice("Selection copied"),
            Err(_) => self.set_notice("Clipboard failed"),
        }
    }

    fn copy_comments(&mut self) {
        if self.comments.is_empty() {
            self.set_notice("No comments to copy");
            return;
        }
        match export_comments_with(&mut self.comments, copy_to_clipboard) {
            Ok(count) => {
                self.comment_hover = None;
                self.set_notice(format!(
                    "Copied {count} comment{}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            Err(_) => self.set_notice(format!("Copy failed; kept {}", self.comments.len())),
        }
    }

    fn choose_agent(&mut self) {
        if self.comments.is_empty() {
            self.set_notice("No comments to send");
            return;
        }
        let Some(owner) = self.owner.as_deref() else {
            self.set_notice("Sidebar unavailable");
            return;
        };
        match agent_targets(owner) {
            Err(_) => self.set_notice("Agent list failed"),
            Ok(agents) if agents.is_empty() => self.set_notice("No agents in this tab"),
            Ok(mut agents) if agents.len() == 1 => {
                self.send_comments(agents.remove(0));
            }
            Ok(agents) => {
                self.agent_picker = Some(AgentPicker {
                    agents,
                    selected: 0,
                    scroll: 0,
                    rect: Rect::default(),
                });
            }
        }
    }

    fn send_comments(&mut self, agent: AgentTarget) {
        match export_comments_with(&mut self.comments, |text| send_to_agent(&agent, text)) {
            Err(_) => {
                self.agent_picker = None;
                self.set_notice(format!("Send failed; kept {}", self.comments.len()));
            }
            Ok(_) => {
                self.agent_picker = None;
                self.close();
                let _ = ipc::call_text(
                    "pane.focus",
                    serde_json::json!({ "pane_id": agent.pane_id }),
                );
            }
        }
    }

    fn on_draft_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancel_comment();
            return;
        }
        if key.code == KeyCode::Enter
            && !key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            self.save_comment();
            return;
        }
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => draft.insert("\n"),
            KeyCode::Char('j')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                draft.insert("\n");
            }
            KeyCode::Backspace => draft.backspace(),
            KeyCode::Delete => draft.delete(),
            KeyCode::Left => draft.move_left(),
            KeyCode::Right => draft.move_right(),
            KeyCode::Up => draft.move_vertical(false),
            KeyCode::Down => draft.move_vertical(true),
            KeyCode::Home => draft.caret = draft.line_start(),
            KeyCode::End => draft.caret = draft.line_end(),
            KeyCode::Char('a')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                draft.caret = 0;
            }
            KeyCode::Char('e')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                draft.caret = draft.input.len();
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::SUPER)
                    && (!key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.modifiers.contains(KeyModifiers::ALT)) =>
            {
                draft.insert(&ch.to_string());
            }
            _ => {}
        }
    }

    fn on_agent_picker_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.agent_picker = None;
            return;
        }
        let Some(picker) = self.agent_picker.as_mut() else {
            return;
        };
        let max = picker.agents.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => picker.selected = (picker.selected + 1).min(max),
            KeyCode::Home | KeyCode::Char('g') => picker.selected = 0,
            KeyCode::End | KeyCode::Char('G') => picker.selected = max,
            KeyCode::Char(ch @ '1'..='9') => {
                let index = ch.to_digit(10).unwrap_or(1) as usize - 1;
                if let Some(agent) = picker.agents.get(index).cloned() {
                    self.send_comments(agent);
                }
            }
            KeyCode::Enter => {
                if let Some(agent) = picker.agents.get(picker.selected).cloned() {
                    self.send_comments(agent);
                }
            }
            _ => {}
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if let Some(draft) = self.draft.as_mut() {
            draft.insert(text);
        }
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
        self.dragging_selection = false;
        self.selection_dragged = false;
        self.observe_mouse();
    }

    /// Follow requests written synchronously by `open_in_pane`.
    pub fn sync(&mut self) {
        let Some(control) = &self.control else { return };
        let Some((width, request)) = read_inline_control(control) else {
            if self.current.is_some() {
                self.cancel_comment();
                self.agent_picker = None;
                self.current = None;
                self.doc = None;
                self.expanded_folds.clear();
            }
            return;
        };
        if self.current.as_ref() == Some(&request) {
            return;
        }
        self.cancel_comment();
        self.agent_picker = None;
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
        prepare_new_request(&mut doc, &request, self.hide_unmodified);
        self.doc = Some(doc);
        self.current = Some(request);
        self.dragging_selection = false;
        self.selection_dragged = false;
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
        if self.doc.is_none() {
            return;
        }
        self.area = area;
        frame.render_widget(Clear, area);
        let border = Block::default().borders(Borders::LEFT).border_style(Style::default().dim());
        let inner = border.inner(area);
        frame.render_widget(border, area);
        let comment_ranges = self
            .current
            .as_ref()
            .map(|current| {
                self.comments
                    .iter()
                    .filter(|comment| comment.pending.document == *current)
                    .map(|comment| comment.pending.range)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let highlight_styles = crate::syntax::preview_highlight_styles(self.diff_theme);
        let (_, body) = draw_doc(
            frame,
            self.doc.as_mut().expect("checked above"),
            self.theme,
            inner,
            &comment_ranges,
            highlight_styles,
        );
        self.body = body;

        let footer = Rect::new(body.x, body.y + body.height, body.width, 1);
        frame.render_widget(Clear, footer);
        let notice = self
            .notice
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(message, _)| message.clone());
        self.footer_actions.clear();
        if let Some(notice) = notice.as_deref() {
            frame.render_widget(Paragraph::new(notice.dim()), footer);
        } else {
            let selected = self
                .doc
                .as_ref()
                .is_some_and(|doc| doc.selected_range().is_some());
            let mut x = footer.x;
            let mut spans = Vec::new();
            if !selected {
                let label = " drag to select text";
                spans.push(Span::styled(label, Style::default().dim()));
                x = x.saturating_add(label.cell_width());
            }
            for (action, label) in footer_entries(selected, !self.comments.is_empty()) {
                let width = label.cell_width();
                if x.saturating_add(width) > footer.x.saturating_add(footer.width) {
                    break;
                }
                let style = if self.footer_hover == Some(action) {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default().dim()
                };
                spans.push(Span::styled(label, style));
                self.footer_actions
                    .push((Rect::new(x, footer.y, width, 1), action));
                x = x.saturating_add(width);
            }
            let current_comments = self.current_comment_count();
            if !selected && current_comments > 0 {
                let label = format!(" d Clear {current_comments} ");
                let width = label.cell_width();
                if x.saturating_add(width) <= footer.x.saturating_add(footer.width) {
                    let action = FooterAction::ClearFileComments;
                    let style = if self.footer_hover == Some(action) {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default().dim()
                    };
                    spans.push(Span::styled(label, style));
                    self.footer_actions
                        .push((Rect::new(x, footer.y, width, 1), action));
                }
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), footer);
        }

        if let Some(draft) = self.draft.as_ref() {
            draw_comment_editor(frame, area, draft, notice.as_deref());
        } else if let Some(picker) = self.agent_picker.as_mut() {
            let options = picker
                .agents
                .iter()
                .map(|agent| agent.label.as_str())
                .collect::<Vec<_>>();
            picker.rect = crate::ui::draw_option_picker(
                frame,
                area.inner(Margin::new(1, 1)),
                "Send comments to",
                &options,
                picker.selected,
                &mut picker.scroll,
            );
        } else if let Some(hover) = self
            .comment_hover
            .as_ref()
            .filter(|hover| Instant::now() < hover.until)
            && let Some(comment) = self.comments.get(hover.index)
            && self.current.as_ref() == Some(&comment.pending.document)
        {
            draw_comment_tooltip(
                frame,
                self.body,
                hover.position,
                comment,
                highlight_styles.comment_border,
            );
        }
    }

    pub fn owns_mouse(&self, mouse: &MouseEvent) -> bool {
        self.is_open()
            && mouse.column >= self.area.x
            && mouse.column < self.area.x + self.area.width
            && mouse.row >= self.area.y
            && mouse.row < self.area.y + self.area.height
    }

    fn comment_at(&self, mouse: &MouseEvent) -> Option<usize> {
        let current = self.current.as_ref()?;
        let doc = self.doc.as_ref()?;
        let visual_row = doc_row_at(self.body, doc.scroll, mouse)?;
        let column = usize::from(mouse.column.saturating_sub(self.body.x));
        let hit = doc.hit_point(visual_row, column)?;
        self.comments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, comment)| {
                comment.pending.document == *current && comment.pending.range.contains_hit(hit)
            })
            .map(|(index, _)| index)
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.draft.is_some() && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            self.on_draft_key(key);
            return;
        }
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.agent_picker.is_some() {
            self.on_agent_picker_key(key);
            return;
        }
        if is_copy_shortcut(&key) {
            self.copy_selection();
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.is_empty() {
            self.start_comment();
            return;
        }
        if key.code == KeyCode::Char('y') && key.modifiers.is_empty() {
            self.copy_comments();
            return;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.is_empty() {
            self.choose_agent();
            return;
        }
        if key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
            self.clear_current_file_comments();
            return;
        }
        if key.code == KeyCode::Esc
            && self
                .doc
                .as_ref()
                .is_some_and(|doc| doc.selected_range().is_some())
        {
            if let Some(doc) = self.doc.as_mut() {
                doc.clear_selection();
            }
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
        if self.draft.is_some() {
            self.comment_hover = None;
            return;
        }
        if self.agent_picker.is_some() {
            self.comment_hover = None;
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let (rect, scroll, selected, total) = self
                    .agent_picker
                    .as_ref()
                    .map(|picker| {
                        (
                            picker.rect,
                            picker.scroll,
                            picker.selected,
                            picker.agents.len(),
                        )
                    })
                    .unwrap_or_default();
                if let Some(index) =
                    crate::ui::option_picker_index(rect, scroll, mouse.column, mouse.row, total)
                {
                    if index == selected {
                        if let Some(agent) = self
                            .agent_picker
                            .as_ref()
                            .and_then(|picker| picker.agents.get(index))
                            .cloned()
                        {
                            self.send_comments(agent);
                        }
                    } else if let Some(picker) = self.agent_picker.as_mut() {
                        picker.selected = index;
                    }
                }
            }
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Moved) {
            self.comment_hover = self.comment_at(&mouse).map(|index| CommentHover {
                index,
                position: Position::new(mouse.column, mouse.row),
                until: Instant::now() + crate::ui::TITLE_ACTIONS_LINGER,
            });
        } else if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            self.comment_hover = None;
        }
        let hovered_action = self
            .footer_actions
            .iter()
            .find(|(rect, _)| rect.contains(Position::new(mouse.column, mouse.row)))
            .map(|(_, action)| *action);
        self.footer_hover = hovered_action;
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(action) = hovered_action
        {
            match action {
                FooterAction::Comment => self.start_comment(),
                FooterAction::CopySelection => self.copy_selection(),
                FooterAction::CopyComments => self.copy_comments(),
                FooterAction::Send => self.choose_agent(),
                FooterAction::ClearFileComments => self.clear_current_file_comments(),
            }
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some(visual_row) = doc_row_at(
                self.body,
                self.doc.as_ref().map_or(0, |doc| doc.scroll),
                &mouse,
            )
        {
            let column = usize::from(mouse.column.saturating_sub(self.body.x));
            if let Some(hit) = self
                .doc
                .as_ref()
                .and_then(|doc| doc.hit_point(visual_row, column))
            {
                if let Some(doc) = self.doc.as_mut() {
                    doc.begin_selection(hit);
                }
                self.dragging_selection = true;
                self.selection_dragged = false;
                return;
            }
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
                self.reload_current_preserving_view();
            }
            return;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.body.contains(Position::new(mouse.column, mouse.row))
        {
            if let Some(doc) = self.doc.as_mut() {
                doc.clear_selection();
            }
            self.dragging_selection = false;
            self.selection_dragged = false;
            return;
        }
        let Some(doc) = self.doc.as_mut() else { return };
        let max = doc.visual_len().saturating_sub(1);
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_selection => {
                self.selection_dragged = true;
                let inside_columns = mouse.column >= self.body.x
                    && mouse.column < self.body.x.saturating_add(self.body.width);
                let visual_row = if let Some(row) = doc_row_at(self.body, doc.scroll, &mouse) {
                    Some(row)
                } else if inside_columns && mouse.row < self.body.y {
                    doc.scroll = doc.scroll.saturating_sub(1);
                    Some(doc.scroll)
                } else if inside_columns
                    && mouse.row >= self.body.y.saturating_add(self.body.height)
                {
                    doc.scroll = (doc.scroll + 1).min(max);
                    Some(
                        doc.scroll
                            .saturating_add(usize::from(self.body.height).saturating_sub(1))
                            .min(max),
                    )
                } else {
                    None
                };
                if let Some(visual_row) = visual_row {
                    let column = usize::from(mouse.column.saturating_sub(self.body.x));
                    if let Some(hit) = doc.hit_point(visual_row, column) {
                        doc.drag_selection(hit);
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging_selection => {
                if self.selection_dragged
                    && let Some(visual_row) = doc_row_at(self.body, doc.scroll, &mouse)
                {
                    let column = usize::from(mouse.column.saturating_sub(self.body.x));
                    if let Some(hit) = doc.hit_point(visual_row, column) {
                        doc.drag_selection(hit);
                    }
                } else {
                    doc.clear_selection();
                }
                self.dragging_selection = false;
                self.selection_dragged = false;
            }
            MouseEventKind::ScrollUp => {
                self.dragging_selection = false;
                self.selection_dragged = false;
                doc.scroll = doc.scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                self.dragging_selection = false;
                self.selection_dragged = false;
                doc.scroll = (doc.scroll + 3).min(max);
            }
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.notice = None;
        }
        if self.draft.is_some()
            || self.agent_picker.is_some()
            || !self.comments.is_empty()
            || self
                .doc
                .as_ref()
                .is_some_and(|doc| doc.selected_range().is_some())
        {
            return;
        }
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

    fn reload_current_preserving_view(&mut self) {
        let Some(request) = self.current.as_ref() else { return };
        let keep = self.doc.as_ref().map(|doc| doc.scroll).unwrap_or(0);
        let anchor = self
            .doc
            .as_mut()
            .and_then(|doc| doc.view_anchor(self.body.width, self.body.height));
        let mut doc = load(
            request,
            self.diff_theme,
            self.hide_unmodified,
            &self.expanded_folds,
        );
        if !anchor
            .as_ref()
            .is_some_and(|anchor| doc.restore_view_anchor(anchor, self.body.width))
        {
            doc.scroll = keep;
        }
        self.doc = Some(doc);
        self.last_refresh = Instant::now();
    }

    pub fn close(&mut self) {
        if !self.is_open() {
            return;
        }
        self.cancel_comment();
        self.agent_picker = None;
        self.footer_actions.clear();
        self.footer_hover = None;
        self.comment_hover = None;
        if let Some(control) = &self.control {
            let _ = std::fs::remove_file(control);
        }
        self.current = None;
        self.doc = None;
        self.dragging_selection = false;
        self.selection_dragged = false;
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

    pub fn clear_mouse_hover(&mut self) {
        self.footer_hover = None;
        self.comment_hover = None;
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
            prepare_new_request(&mut doc, request, hide_unmodified);
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
            (page, body) = draw_doc(
                frame,
                &mut doc,
                theme,
                area,
                &[],
                crate::syntax::preview_highlight_styles(diff_theme),
            );
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
                            let anchor = doc.view_anchor(body.width, body.height);
                            let mut next = load(
                                request,
                                diff_theme,
                                hide_unmodified,
                                &expanded_folds,
                            );
                            if !anchor
                                .as_ref()
                                .is_some_and(|anchor| next.restore_view_anchor(anchor, body.width))
                            {
                                next.scroll = keep;
                            }
                            doc = next;
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
                    prepare_new_request(&mut doc, request, hide_unmodified);
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

fn highlight_cells(
    line: Line<'static>,
    selected: (usize, usize),
    highlight: Style,
) -> Line<'static> {
    let mut cells = 0usize;
    let mut spans: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        for grapheme in span.content.graphemes(true) {
            let width = grapheme.cell_width() as usize;
            let highlighted = cells < selected.1 && cells + width.max(1) > selected.0;
            let style = if highlighted {
                span.style.patch(highlight)
            } else {
                span.style
            };
            if let Some(last) = spans.last_mut()
                && last.style == style
            {
                last.content.to_mut().push_str(grapheme);
            } else {
                spans.push(Span::styled(grapheme.to_string(), style));
            }
            cells += width;
        }
    }
    Line::from(spans).style(line.style)
}

/// Header (✕ close + name + context), body, hint footer. Returns the page
/// stride for PageUp/Down and the body hit-test rectangle.
fn draw_doc(
    frame: &mut Frame,
    doc: &mut Doc,
    theme: IconTheme,
    area: Rect,
    comment_ranges: &[TextSelection],
    highlight_styles: PreviewHighlightStyles,
) -> (usize, Rect) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    doc.ensure_visual_lines(body.width);
    doc.apply_source_line_center(body.height);
    doc.apply_first_change_center(body.height);
    doc.scroll = doc
        .scroll
        .min(doc.visual_len().saturating_sub(usize::from(body.height).max(1)));
    let highlighted_source_row = doc.active_source_highlight();

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
            .enumerate()
            .skip(doc.scroll)
            .take(usize::from(body.height))
            .map(|(visual_row, line)| {
                let mut line = line.clone();
                for range in comment_ranges {
                    if let Some(cells) = doc.range_cells(visual_row, *range) {
                        line = highlight_cells(line, cells, highlight_styles.comment);
                    }
                }
                if let Some(cells) = doc.selection_cells(visual_row) {
                    line = highlight_cells(line, cells, highlight_styles.selection);
                }
                line
            })
            .collect()
    } else {
        let number_width = doc.lines.len().to_string().len();
        doc.lines
            .iter()
            .enumerate()
            .skip(doc.scroll)
            .take(usize::from(body.height))
            .map(|(n, line)| {
                let mut line = line.clone();
                for range in comment_ranges {
                    if let Some(cells) = doc.range_cells(n, *range) {
                        line = highlight_cells(line, cells, highlight_styles.comment);
                    }
                }
                if let Some(cells) = doc.selection_cells(n) {
                    line = highlight_cells(line, cells, highlight_styles.selection);
                }
                line = if doc.numbered {
                    let mut spans = vec![Span::styled(
                        format!("{:>number_width$} ", n + 1),
                        Style::default().dim(),
                    )];
                    spans.extend(line.spans);
                    Line::from(spans).style(line.style)
                } else {
                    line
                };
                if highlighted_source_row == Some(n) {
                    line.style = line.style.bg(Color::DarkGray);
                }
                if line.style.bg.is_some() {
                    let pad = usize::from(body.width).saturating_sub(line.width());
                    if pad > 0 {
                        line.spans.push(Span::raw(" ".repeat(pad)));
                    }
                }
                line
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
        let f = file_line_request(Path::new("C:/x/y.rs"), 42);
        assert_eq!(
            parse_request(&f),
            Some(Request::FileLine {
                path: PathBuf::from("C:/x/y.rs"),
                line: 42,
            })
        );
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
                kind: "staged".into(),
                line: None,
            })
        );
        let d = diff_line_request(Path::new("C:/repo"), "src/a.rs", "worktree", 42);
        assert_eq!(
            parse_request(&d),
            Some(Request::Diff {
                root: PathBuf::from("C:/repo"),
                rel: "src/a.rs".into(),
                kind: "worktree".into(),
                line: Some(42),
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
    fn file_line_preview_keeps_source_lines_and_centers_target() {
        let path = std::env::temp_dir().join(format!(
            "herdr-sidebar-file-line-{}.md",
            std::process::id()
        ));
        let text = (1..=100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).unwrap();

        let request = Request::FileLine {
            path: path.clone(),
            line: 42,
        };
        let mut doc = load(&request, crate::syntax::DEFAULT_DIFF_THEME, true, &HashSet::new());
        prepare_new_request(&mut doc, &request, true);
        doc.apply_source_line_center(21);

        assert!(doc.numbered, "line anchors must bypass glow reformatting");
        assert_eq!(doc.scroll, 31);
        assert_eq!(doc.active_source_highlight(), Some(41));
        assert_eq!(doc.lines[41].to_string(), "line 42");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn diff_line_anchor_uses_the_current_file_line() {
        let sources = vec![
            Some(SelectableRow::diff(crate::diffview::DiffSource {
                text: "old".into(),
                old_line: Some(41),
                new_line: None,
                kind: crate::diffview::DiffSourceKind::Deleted,
            })),
            Some(SelectableRow::diff(crate::diffview::DiffSource {
                text: "new".into(),
                old_line: None,
                new_line: Some(42),
                kind: crate::diffview::DiffSourceKind::Added,
            })),
        ];
        let mut doc = Doc::new(
            "a.rs".into(),
            "diff".into(),
            vec![Line::raw("old"), Line::raw("new")],
            false,
            vec![None, None],
        )
        .with_sources(sources);

        assert!(doc.request_diff_line(42, true));
        doc.apply_source_line_center(1);

        assert_eq!(doc.scroll, 1);
        assert_eq!(doc.active_source_highlight(), Some(1));
        assert!(!doc.request_diff_line(41, true), "deleted-side lines are not current-file anchors");
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

        let sources = vec![
            Some(SelectableRow::new(
                code.to_string(),
                SourceKind::Diff {
                    old_line: Some(12),
                    new_line: None,
                    kind: crate::diffview::DiffSourceKind::Deleted,
                },
            )),
            None,
        ];
        let (lines, rows, fragments) = reflow_diff_lines(&[long, short], &sources, 12);
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
        assert_eq!(
            fragments[..wrapped]
                .iter()
                .map(|row| row.source_row)
                .collect::<Vec<_>>(),
            vec![0; wrapped]
        );
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
        let source_count = lines.len();
        let mut doc = Doc::new("x.rs".into(), String::new(), lines, false, folds)
            .with_sources(vec![None; source_count]);
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
    fn character_selection_maps_tabs_wide_graphemes_and_raw_text() {
        let raw = "a\t中🙂e";
        let source = SelectableRow::file(raw, 1);
        let line = Line::raw("a   中🙂e");
        let mut doc = Doc::new("x.txt".into(), String::new(), vec![line], true, Vec::new())
            .with_sources(vec![Some(source)]);

        // One-cell line-number gutter, one separating space, then source cells.
        let chinese = doc.hit_point(0, 2 + 4).unwrap();
        let emoji = doc.hit_point(0, 2 + 7).unwrap();
        doc.begin_selection(chinese);
        doc.drag_selection(emoji);

        assert_eq!(doc.selected_text().as_deref(), Some("中🙂"));
        assert_eq!(doc.selection_cells(0), Some((4, 8)));
        let exported = doc.selection_export().unwrap();
        assert_eq!(exported.snippet, "中🙂");
        assert_eq!(doc.range_cells(0, exported.range), Some((4, 8)));
        assert!(exported.range.contains_hit(chinese));
        assert!(exported.range.contains_hit(emoji));
        assert!(!exported.range.contains_hit(doc.hit_point(0, 2 + 8).unwrap()));
        let comment_style = Style::default().bg(Color::Yellow);
        let highlighted = highlight_cells(doc.lines[0].clone(), (4, 8), comment_style);
        assert_eq!(
            highlighted
                .spans
                .iter()
                .filter(|span| span.style.bg == Some(Color::Yellow))
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "中🙂"
        );
        assert_eq!(
            (
                exported.start_line,
                exported.start_column,
                exported.end_line,
                exported.end_column
            ),
            (1, 3, 1, 4)
        );
    }

    #[test]
    fn reverse_selection_and_fold_boundaries_stay_in_selectable_source() {
        let sources = vec![
            Some(SelectableRow::file("alpha", 1)),
            None,
            Some(SelectableRow::file("omega", 3)),
        ];
        let mut doc = Doc::new(
            "x.txt".into(),
            String::new(),
            vec![Line::raw("alpha"), Line::raw("fold"), Line::raw("omega")],
            true,
            Vec::new(),
        )
        .with_sources(sources);
        doc.begin_selection(HitPoint {
            before: TextPoint { row: 2, byte: 5 },
            after: TextPoint { row: 2, byte: 5 },
        });
        doc.drag_selection(HitPoint {
            before: TextPoint { row: 0, byte: 0 },
            after: TextPoint { row: 0, byte: 1 },
        });

        assert_eq!(doc.selected_text().as_deref(), Some("omega"));
        assert_eq!(doc.selected_range().unwrap().0.row, 2);

        doc.begin_selection(HitPoint {
            before: TextPoint { row: 0, byte: 0 },
            after: TextPoint { row: 0, byte: 1 },
        });
        doc.drag_selection(HitPoint {
            before: TextPoint { row: 2, byte: 4 },
            after: TextPoint { row: 2, byte: 5 },
        });
        assert_eq!(doc.selected_text().as_deref(), Some("alpha"));
        assert_eq!(doc.selected_range().unwrap().1.row, 0);
    }

    #[test]
    fn expanding_a_fold_preserves_the_visible_diff_anchor() {
        use std::fmt::Write as _;

        fn doc(rendered: crate::diffview::RenderedDiff) -> Doc {
            Doc::new(
                "x.rs".into(),
                String::new(),
                rendered.lines,
                false,
                rendered.folds,
            )
            .with_sources(
                rendered
                    .sources
                    .into_iter()
                    .map(|source| source.map(SelectableRow::diff))
                    .collect(),
            )
        }

        let mut diff = String::from("@@ -1,20 +1,20 @@\n");
        for line in 1..=20 {
            if line == 10 {
                diff.push_str("-let old = 10;\n+let new = 10;\n");
            } else {
                writeln!(diff, " let line_{line} = {line};").unwrap();
            }
        }
        let collapsed = crate::diffview::render_expanded(
            "x.rs",
            &diff,
            crate::syntax::DEFAULT_DIFF_THEME,
            &HashSet::new(),
            true,
        );
        let fold = collapsed.folds.iter().flatten().copied().next().unwrap();
        let mut before = doc(collapsed);
        let anchor = before.view_anchor(80, 20).unwrap();
        assert_eq!(anchor.screen_row, 1, "the leading fold occupies row zero");

        let expanded = crate::diffview::render_expanded(
            "x.rs",
            &diff,
            crate::syntax::DEFAULT_DIFF_THEME,
            &HashSet::from([fold]),
            true,
        );
        let mut after = doc(expanded);
        assert!(after.restore_view_anchor(&anchor, 80));
        assert!(after.scroll > before.scroll, "new context must become reachable above");

        let restored_row = after.source_row_at(after.scroll + anchor.screen_row).unwrap();
        let restored = after.sources[restored_row].as_ref().unwrap();
        assert_eq!(restored.kind, anchor.kind);
        assert_eq!(restored.raw, anchor.raw);
    }

    #[test]
    fn wrapped_diff_selection_uses_source_bytes_not_visual_rows() {
        let diff = "@@ -1,1 +1,1 @@\n-abcdefghij\n+ABCDEFGHIJ\n";
        let rendered = crate::diffview::render_expanded(
            "x.txt",
            diff,
            crate::syntax::DEFAULT_DIFF_THEME,
            &HashSet::new(),
            false,
        );
        let sources = rendered
            .sources
            .into_iter()
            .map(|source| source.map(SelectableRow::diff))
            .collect();
        let mut doc = Doc::new(
            "x.txt".into(),
            String::new(),
            rendered.lines,
            false,
            rendered.folds,
        )
        .with_sources(sources);
        doc.ensure_visual_lines(10); // 5-cell gutter, 5 source cells per visual row.

        let start = doc.hit_point(0, 5 + 3).unwrap();
        let end = doc.hit_point(1, 5 + 4).unwrap();
        doc.begin_selection(start);
        doc.drag_selection(end);

        assert_eq!(doc.selected_text().as_deref(), Some("defghij"));
        let exported = doc.selection_export().unwrap();
        assert_eq!(exported.snippet, "-defghij");
        assert!(exported.removed);
    }

    #[test]
    fn mixed_diff_selection_keeps_markers_and_uses_the_new_location() {
        let diff = "@@ -4,1 +4,1 @@\n-old name\n+new name\n";
        let rendered = crate::diffview::render_expanded(
            "x.txt",
            diff,
            crate::syntax::DEFAULT_DIFF_THEME,
            &HashSet::new(),
            false,
        );
        let sources = rendered
            .sources
            .into_iter()
            .map(|source| source.map(SelectableRow::diff))
            .collect::<Vec<_>>();
        let source_kinds = sources
            .iter()
            .map(|source| source.as_ref().map(|source| source.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            source_kinds,
            [
                None,
                Some(SourceKind::Diff {
                    old_line: Some(4),
                    new_line: None,
                    kind: crate::diffview::DiffSourceKind::Deleted,
                }),
                Some(SourceKind::Diff {
                    old_line: None,
                    new_line: Some(4),
                    kind: crate::diffview::DiffSourceKind::Added,
                }),
            ]
        );
        let mut doc = Doc::new(
            "x.txt".into(),
            String::new(),
            rendered.lines,
            false,
            rendered.folds,
        )
        .with_sources(sources);
        doc.begin_selection(HitPoint {
            before: TextPoint { row: 1, byte: 0 },
            after: TextPoint { row: 1, byte: 1 },
        });
        doc.drag_selection(HitPoint {
            before: TextPoint { row: 2, byte: 7 },
            after: TextPoint { row: 2, byte: 8 },
        });

        let exported = doc.selection_export().unwrap();
        assert_eq!(exported.snippet, "-old name\n+new name");
        assert_eq!((exported.start_line, exported.end_line), (4, 4));
        assert!(!exported.removed);
    }

    #[test]
    fn comments_format_and_bracketed_paste_are_stable() {
        use ratatui::{Terminal, backend::TestBackend};

        let comments = vec![SavedComment {
            pending: PendingComment {
                location: "src/a.rs:2:3-2:5".into(),
                snippet: "x```y".into(),
                removed: true,
                document: Request::File("src/a.rs".into()),
                range: TextSelection {
                    anchor: TextPoint { row: 1, byte: 2 },
                    cursor: TextPoint { row: 1, byte: 5 },
                },
            },
            body: "Explain this".into(),
        }];
        let formatted = format_comments(&comments);
        assert!(formatted.contains("## `src/a.rs:2:3-2:5` (removed)"));
        assert!(formatted.contains("````\nx```y\n````"));
        assert!(formatted.ends_with("Explain this"));

        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        terminal
            .draw(|frame| {
                draw_comment_tooltip(
                    frame,
                    Rect::new(0, 0, 50, 10),
                    Position::new(8, 2),
                    &comments[0],
                    Color::Yellow,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = (0..10)
            .map(|y| (0..50).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Explain this"));
        assert!(rendered.contains("src/a.rs:2:3-2:5"));

        let pasted = bracketed_paste("a\x1b[200~b\x1b[201~c");
        assert_eq!(pasted, "\x1b[200~abc\x1b[201~");
    }

    #[test]
    fn copy_shortcut_matches_the_platform() {
        let (primary, other) = if cfg!(target_os = "macos") {
            (KeyModifiers::SUPER, KeyModifiers::CONTROL)
        } else {
            (KeyModifiers::CONTROL, KeyModifiers::SUPER)
        };
        assert!(is_copy_shortcut(&KeyEvent::new(KeyCode::Char('c'), primary)));
        assert!(!is_copy_shortcut(&KeyEvent::new(
            KeyCode::Char('c'),
            other,
        )));
        assert!(!is_copy_shortcut(&KeyEvent::new(
            KeyCode::Char('c'),
            primary | KeyModifiers::SHIFT,
        )));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(COPY_SELECTION_LABEL, " ⌘C Copy ");
            assert_eq!(COPY_SELECTION_LABEL.cell_width(), 9);
        }
    }

    #[test]
    fn saved_comment_footer_appends_copy_and_send_agent() {
        assert_eq!(
            footer_entries(false, true),
            vec![
                (FooterAction::CopyComments, " y Copy "),
                (FooterAction::Send, " s Send Agent "),
            ]
        );
        assert!(footer_entries(false, false).is_empty());
        assert_eq!(
            footer_entries(true, true),
            vec![
                (FooterAction::Comment, " c Comment "),
                (FooterAction::CopySelection, COPY_SELECTION_LABEL),
            ]
        );
    }

    #[test]
    fn clear_comments_removes_only_the_current_file() {
        fn comment(document: Request) -> SavedComment {
            SavedComment {
                pending: PendingComment {
                    location: "x:1:1-1:2".into(),
                    snippet: "x".into(),
                    removed: false,
                    document,
                    range: TextSelection {
                        anchor: TextPoint { row: 0, byte: 0 },
                        cursor: TextPoint { row: 0, byte: 1 },
                    },
                },
                body: "Comment".into(),
            }
        }

        let same = PathBuf::from("src/main.rs");
        let mut comments = vec![
            comment(Request::File(same.clone())),
            comment(Request::FileLine {
                path: same.clone(),
                line: 2,
            }),
            comment(Request::File(PathBuf::from("src/lib.rs"))),
        ];

        assert_eq!(
            remove_comments_for_file(
                &mut comments,
                &Request::SearchFile {
                    path: same,
                    line: 3,
                    query: "main".into(),
                    regex: false,
                    case_sensitive: false,
                    whole_word: false,
                },
            ),
            2
        );
        assert_eq!(comments.len(), 1);
        assert_eq!(
            request_file_path(&comments[0].pending.document),
            Some(PathBuf::from("src/lib.rs"))
        );
    }

    #[test]
    fn comment_exports_consume_only_after_success() {
        fn comment() -> SavedComment {
            SavedComment {
                pending: PendingComment {
                    location: "src/main.rs:2:3-2:5".into(),
                    snippet: "bad()".into(),
                    removed: false,
                    document: Request::File("src/main.rs".into()),
                    range: TextSelection {
                        anchor: TextPoint { row: 1, byte: 2 },
                        cursor: TextPoint { row: 1, byte: 5 },
                    },
                },
                body: "Please rename this.".into(),
            }
        }

        let mut failed = vec![comment()];
        let result = export_comments_with(&mut failed, |_| Err("offline".into()));
        assert_eq!(result, Err("offline".into()));
        assert_eq!(failed.len(), 1, "failed export must retain comments");

        let mut sent = String::new();
        let count = export_comments_with(&mut failed, |text| {
            sent.push_str(text);
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1);
        assert!(failed.is_empty());
        assert!(sent.contains("src/main.rs:2:3-2:5"));
        assert!(sent.contains("Please rename this."));
    }

    #[test]
    fn agent_targets_are_limited_to_the_sidebar_tab() {
        let panes = serde_json::json!({
            "panes": [
                {"pane_id":"w1:p1","tab_id":"w1:t1","agent":null},
                {"pane_id":"w1:p2","tab_id":"w1:t1","agent":"pi","title":"Reviewer"},
                {"pane_id":"w1:p3","tab_id":"w1:t1","agent":null,"title":"Shell"},
                {"pane_id":"w1:p4","tab_id":"w1:t2","agent":"claude","title":"Other tab"},
                {"pane_id":"w2:p1","tab_id":"w2:t1","agent":"codex","title":"Other workspace"}
            ]
        });
        let targets = agent_targets_from_value(panes, "w1:p1").unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pane_id, "w1:p2");
        assert_eq!(targets[0].label, "Reviewer");
    }

    #[test]
    fn agent_target_lookup_fails_when_the_sidebar_pane_is_gone() {
        let panes = serde_json::json!({
            "panes": [
                {"pane_id":"w1:p2","tab_id":"w1:t1","agent":"pi"}
            ]
        });
        assert!(agent_targets_from_value(panes, "w1:p1").is_err());
    }

    #[test]
    fn comment_editor_moves_and_deletes_on_grapheme_boundaries() {
        let mut draft = CommentDraft {
            pending: PendingComment {
                location: "x:1:1-1:1".into(),
                snippet: "x".into(),
                removed: false,
                document: Request::File("x".into()),
                range: TextSelection {
                    anchor: TextPoint { row: 0, byte: 0 },
                    cursor: TextPoint { row: 0, byte: 1 },
                },
            },
            input: "a中🙂".into(),
            caret: "a中🙂".len(),
        };
        draft.move_left();
        draft.backspace();
        assert_eq!(draft.input, "a🙂");
        assert_eq!(draft.caret, 1);
        draft.insert("\n二");
        draft.move_vertical(false);
        assert_eq!(draft.caret, 1);
    }

    #[test]
    fn comment_editor_expands_tabs_and_tracks_wide_text_cursor_cells() {
        let input = "a\t中🙂";
        let (rows, row, column) = editor_rows(input, 2, 8);
        assert_eq!(
            rows.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["a   中🙂"]
        );
        assert_eq!((row, column), (0, 4));
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
