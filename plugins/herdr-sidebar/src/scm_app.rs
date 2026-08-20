//! TUI state and rendering: the VS Code Source Control panel — commit message
//! box (with the ✨ suggest button), Commit button, collapsible Staged/Changes
//! sections, Git-Graph-style drawers (GRAPH, COMMITS, FILE HISTORY, BRANCHES,
//! REMOTES, STASHES, TAGS), theme-matched file icons, mouse support, and a
//! Ctrl+right-click context menu — kept interaction-consistent with the
//! Explorer view. No own border/title: herdr already frames the pane and
//! titles it with the pane label.
//!
//! In unified mode the panel shares one "Sidebar" pane with the Explorer and
//! Search views, switching in-process through the activity bar.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};

use herdr_sidebar::change_tree::{Row as ChangeTreeRow, Tree as ChangeTree};
use herdr_sidebar::git::{DiffStat, FileEntry, Git, RefFile, Status};
use herdr_sidebar::icons::{IconTheme, icon};
use herdr_sidebar::state::{self as sidebar, ScmFileView, View};
use herdr_sidebar::state::Exit;
use herdr_sidebar::syntax;
use herdr_sidebar::ui::{
    TitleAction, activity_button_style, activity_icons, branch_icon, draw_option_picker,
    draw_scrollbar, gear_icon, hits, hits_collapse_button, icon_button_style,
    hover_action_row, mouse_linger_active, option_picker_index, redraw_button, sibling_panes_of,
    sparkle_icon, title_actions_visible, truncate_to, within, wrap_footer_message, wrap_hints,
};
use herdr_sidebar::actions::{copy_to_clipboard, open_external, reveal};
use herdr_sidebar::suggest;
use herdr_sidebar::workspace_sync::{
    ExpandedScmRef, ScmAnchor, ScmDrawer, ScmFocus, ScmRef, ScmRepoState, ScmState,
};

// VS Code's dark-theme git decoration colors.
const BUTTON_BLUE: Color = Color::Rgb(0x00, 0x78, 0xd4);
const BUTTON_BLUE_FOCUS: Color = Color::Rgb(0x02, 0x8a, 0xf0);
const BADGE_BLUE: Color = Color::Rgb(0x00, 0x78, 0xd4);
const MODIFIED: Color = Color::Rgb(0xe2, 0xc0, 0x8d);
const UNTRACKED: Color = Color::Rgb(0x73, 0xc9, 0x91);
const ADDED: Color = Color::Rgb(0x81, 0xb8, 0x8b);
const RENAMED: Color = Color::Rgb(0x73, 0xc9, 0x91);
const DELETED: Color = Color::Rgb(0xc7, 0x4e, 0x39);
const CONFLICT: Color = Color::Rgb(0xe4, 0x67, 0x6b);
const HOVER_BG: Color = Color::Rgb(48, 52, 60);


/// How many log lines the history-ish drawers fetch.
const DRAWER_LIMIT: usize = 30;
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);

fn letter_color(letter: char) -> Color {
    match letter {
        'M' => MODIFIED,
        'U' => UNTRACKED,
        'A' => ADDED,
        'R' | 'C' => RENAMED,
        'D' => DELETED,
        '!' => CONFLICT,
        _ => Color::Reset,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Message,
    Commit,
    List,
}

/// The Git-Graph-style drawers below the Changes section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drawer {
    Graph,
    Commits,
    FileHistory,
    Branches,
    Worktrees,
    Remotes,
    Stashes,
    Tags,
}

impl Drawer {
    const ALL: [Drawer; 8] = [
        Drawer::Graph,
        Drawer::Commits,
        Drawer::FileHistory,
        Drawer::Branches,
        Drawer::Worktrees,
        Drawer::Remotes,
        Drawer::Stashes,
        Drawer::Tags,
    ];

    fn title(self) -> &'static str {
        match self {
            Drawer::Graph => "Graph",
            Drawer::Commits => "Commits",
            Drawer::FileHistory => "File History",
            Drawer::Branches => "Branches",
            Drawer::Worktrees => "Worktrees",
            Drawer::Remotes => "Remotes",
            Drawer::Stashes => "Stashes",
            Drawer::Tags => "Tags",
        }
    }

    fn index(self) -> usize {
        Drawer::ALL.iter().position(|d| *d == self).unwrap_or(0)
    }

    fn supports_file_tree(self) -> bool {
        matches!(self, Self::Commits | Self::Branches | Self::Stashes | Self::Tags)
    }
}

#[derive(Default)]
struct DrawerPanel {
    expanded: bool,
    lines: Vec<String>,
    /// What each line points at, parallel to `lines`.
    refs: Vec<DrawerRef>,
    /// FILE HISTORY carries the historical path/revision for direct modern diffs.
    files: Vec<Option<RefFile>>,
}

/// What a drawer line points at, for clicks and context menus.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
enum DrawerRef {
    #[default]
    None,
    /// A short commit hash (GRAPH / COMMITS / FILE HISTORY lines).
    Commit(String),
    /// `stash@{n}`.
    Stash(usize),
    Branch {
        name: String,
        current: bool,
    },
    Remote {
        name: String,
        url: String,
    },
    Tag(String),
    /// A worktree's checkout path.
    Worktree(String),
}

fn sync_drawer(drawer: Drawer) -> ScmDrawer {
    match drawer {
        Drawer::Graph => ScmDrawer::Graph,
        Drawer::Commits => ScmDrawer::Commits,
        Drawer::FileHistory => ScmDrawer::FileHistory,
        Drawer::Branches => ScmDrawer::Branches,
        Drawer::Worktrees => ScmDrawer::Worktrees,
        Drawer::Remotes => ScmDrawer::Remotes,
        Drawer::Stashes => ScmDrawer::Stashes,
        Drawer::Tags => ScmDrawer::Tags,
    }
}

fn app_drawer(drawer: ScmDrawer) -> Drawer {
    match drawer {
        ScmDrawer::Graph => Drawer::Graph,
        ScmDrawer::Commits => Drawer::Commits,
        ScmDrawer::FileHistory => Drawer::FileHistory,
        ScmDrawer::Branches => Drawer::Branches,
        ScmDrawer::Worktrees => Drawer::Worktrees,
        ScmDrawer::Remotes => Drawer::Remotes,
        ScmDrawer::Stashes => Drawer::Stashes,
        ScmDrawer::Tags => Drawer::Tags,
    }
}

fn sync_ref(target: &DrawerRef) -> ScmRef {
    match target {
        DrawerRef::None => ScmRef::None,
        DrawerRef::Commit(hash) => ScmRef::Commit(hash.clone()),
        DrawerRef::Stash(index) => ScmRef::Stash(*index),
        DrawerRef::Branch { name, .. } => ScmRef::Branch(name.clone()),
        DrawerRef::Remote { name, .. } => ScmRef::Remote(name.clone()),
        DrawerRef::Tag(name) => ScmRef::Tag(name.clone()),
        DrawerRef::Worktree(path) => ScmRef::Worktree(path.clone()),
    }
}

/// Parse the actionable reference out of one drawer line.
/// Display form of a `git worktree list` line: the folder NAME plus its
/// branch — the raw absolute path clipped uselessly in a narrow pane.
fn pretty_worktree_line(raw: &str) -> String {
    let path = raw.split_whitespace().next().unwrap_or("");
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if let (Some(start), Some(end)) = (raw.find('['), raw.rfind(']'))
        && start < end
    {
        return format!("{name}  ⎇ {}", &raw[start + 1..end]);
    }
    if raw.contains("(bare)") {
        return format!("{name}  (bare)");
    }
    if raw.contains("detached") {
        return format!("{name}  (detached)");
    }
    name.to_string()
}

/// Display form of a remote line: `name  owner/repo` for hosted URLs (the
/// interesting part), the folder name for local-path remotes.
fn pretty_remote_line(raw: &str) -> String {
    let mut it = raw.split_whitespace();
    match (it.next(), it.next()) {
        (Some(name), Some(url)) => format!("{name}  {}", pretty_remote_url(url)),
        _ => raw.to_string(),
    }
}

fn pretty_remote_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let hosted = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .or_else(|| trimmed.strip_prefix("git@"));
    if let Some(rest) = hosted {
        // git@host:owner/repo and host/owner/repo both → owner/repo.
        let rest = rest.replace(':', "/");
        return match rest.split_once('/') {
            Some((_host, path)) if !path.is_empty() => path.to_string(),
            _ => rest,
        };
    }
    // A local-path remote: its folder name is the recognizable bit.
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed).to_string()
}

fn parse_drawer_ref(kind: Drawer, line: &str) -> DrawerRef {
    match kind {
        Drawer::Graph | Drawer::Commits | Drawer::FileHistory => line
            .split_whitespace()
            .find(|tok| {
                tok.len() >= 7
                    && tok.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
            .map(|h| DrawerRef::Commit(h.to_string()))
            .unwrap_or(DrawerRef::None),
        Drawer::Branches => {
            let current = line.starts_with('*');
            let name = line.trim_start_matches('*').trim().to_string();
            if name.is_empty() || name.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Branch { name, current }
            }
        }
        Drawer::Remotes => {
            let mut it = line.split_whitespace();
            match it.next() {
                Some(name) => DrawerRef::Remote {
                    name: name.to_string(),
                    url: it.next().unwrap_or("").to_string(),
                },
                None => DrawerRef::None,
            }
        }
        Drawer::Worktrees => {
            let path = line.split_whitespace().next().unwrap_or("").to_string();
            if path.is_empty() || path.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Worktree(path)
            }
        }
        Drawer::Stashes => line
            .strip_prefix("stash@{")
            .and_then(|rest| rest.split('}').next())
            .and_then(|n| n.parse::<usize>().ok())
            .map(DrawerRef::Stash)
            .unwrap_or(DrawerRef::None),
        Drawer::Tags => {
            let name = line.trim().to_string();
            if name.is_empty() || name.starts_with('(') {
                DrawerRef::None
            } else {
                DrawerRef::Tag(name)
            }
        }
    }
}

/// One discovered repository and its per-repo view state — including its own
/// commit message, so the multi-repo view mirrors VS Code's per-repo inputs.
struct Repo {
    git: Git,
    name: String,
    status: Status,
    collapsed: bool,
    staged_collapsed: bool,
    changes_collapsed: bool,
    staged_tree: ChangeTree,
    changes_tree: ChangeTree,
    staged_rows: Vec<ChangeTreeRow>,
    changes_rows: Vec<ChangeTreeRow>,
    staged_dirs: BTreeSet<String>,
    changes_dirs: BTreeSet<String>,
    message: Vec<char>,
    cursor: usize,
}

impl Repo {
    fn new(git: Git) -> Self {
        Self {
            name: git.name(),
            git,
            status: Status::default(),
            collapsed: false,
            staged_collapsed: false,
            changes_collapsed: false,
            staged_tree: ChangeTree::default(),
            changes_tree: ChangeTree::default(),
            staged_rows: Vec::new(),
            changes_rows: Vec::new(),
            staged_dirs: BTreeSet::new(),
            changes_dirs: BTreeSet::new(),
            message: Vec::new(),
            cursor: 0,
        }
    }

    /// The repo row's branch decoration: `name*` when the tree is dirty.
    fn branch_decor(&self) -> String {
        let dirty = if self.status.staged.is_empty() && self.status.unstaged.is_empty() {
            ""
        } else {
            "*"
        };
        format!("{}{dirty}", self.status.branch)
    }

    fn rebuild_file_rows(&mut self, view: ScmFileView) {
        self.staged_tree = ChangeTree::new(
            self.status.staged.iter().enumerate().map(|(i, entry)| (i, entry.path.as_str())),
        );
        self.changes_tree = ChangeTree::new(
            self.status.unstaged.iter().enumerate().map(|(i, entry)| (i, entry.path.as_str())),
        );
        self.staged_rows = visible_change_rows(
            view,
            &self.staged_tree,
            &self.staged_dirs,
            self.status.staged.len(),
        );
        self.changes_rows = visible_change_rows(
            view,
            &self.changes_tree,
            &self.changes_dirs,
            self.status.unstaged.len(),
        );
    }
}

fn visible_change_rows(
    view: ScmFileView,
    tree: &ChangeTree,
    collapsed: &BTreeSet<String>,
    len: usize,
) -> Vec<ChangeTreeRow> {
    match view {
        ScmFileView::Tree => tree.rows(collapsed),
        ScmFileView::List => {
            (0..len).map(|index| ChangeTreeRow::File { index, depth: 0 }).collect()
        }
    }
}

struct ExpandedRef {
    kind: Drawer,
    target: DrawerRef,
    files: Vec<RefFile>,
    tree: ChangeTree,
    rows: Vec<ChangeTreeRow>,
    collapsed: BTreeSet<String>,
    error: Option<String>,
}

impl ExpandedRef {
    fn new(kind: Drawer, target: DrawerRef, files: Vec<RefFile>, error: Option<String>) -> Self {
        let tree = ChangeTree::new(
            files.iter().enumerate().map(|(i, file)| (i, file.entry.path.as_str())),
        );
        Self {
            kind,
            target,
            files,
            tree,
            rows: Vec::new(),
            collapsed: BTreeSet::new(),
            error,
        }
    }

    fn rebuild_rows(&mut self, view: ScmFileView) {
        self.rows = visible_change_rows(view, &self.tree, &self.collapsed, self.files.len());
    }
}

/// List rows; the first index on the repo-scoped variants is the repo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    /// Only rendered when more than one repository is visible.
    RepoHeader(usize),
    /// The repo's inline message box (3 screen lines) — multi-repo only.
    Message(usize),
    /// The repo's inline ✓ Commit button — multi-repo only.
    Commit(usize),
    StagedHeader(usize),
    ChangesHeader(usize),
    Staged(usize, usize),
    Unstaged(usize, usize),
    /// Full-width boundary between repository content and the drawer component;
    /// multi-repo mode also uses it around each repository block.
    RepoSeparator,
    DrawerHeader(Drawer),
    DrawerLine(Drawer, usize),
    HistoryTree(usize),
    HistoryNotice,
}

fn tree_nav_target(
    row: &ChangeTreeRow,
    rows: &[ChangeTreeRow],
    index: usize,
    expand: bool,
) -> Option<usize> {
    let depth = row.depth();
    if expand {
        return row.expanded().is_some_and(|expanded| expanded).then(|| index + 1).filter(
            |next| rows.get(*next).is_some_and(|candidate| candidate.depth() > depth),
        );
    }
    if row.expanded().is_some_and(|expanded| expanded) {
        return None;
    }
    rows[..index].iter().rposition(|candidate| candidate.depth() < depth)
}

fn same_row(left: Row, right: Row) -> bool {
    left == right
}

fn status_header_index(rows: &[Row], repo: usize, staged: bool) -> Option<usize> {
    rows.iter().position(|row| {
        matches!(
            (staged, row),
            (true, Row::StagedHeader(r)) | (false, Row::ChangesHeader(r)) if *r == repo
        )
    })
}

fn change_tree_chevron_hit(x: u16, row: Option<&ChangeTreeRow>) -> bool {
    change_tree_chevron_hit_at(x, row, 0)
}

fn change_tree_chevron_hit_at(x: u16, row: Option<&ChangeTreeRow>, base_indent: usize) -> bool {
    matches!(
        row,
        Some(ChangeTreeRow::Directory { depth, .. })
            if x == (1 + base_indent) as u16
                + (*depth as u16).saturating_mul(2)
    )
}

fn change_tail_width(action_slot: bool, buttons_visible: bool, status_slot: bool) -> usize {
    usize::from(buttons_visible) * (3 + usize::from(action_slot) * 3)
        + usize::from(status_slot) * 2
}

fn change_action_hit(x: u16, width: u16) -> bool {
    let start = width.saturating_sub(5);
    x >= start && x < start.saturating_add(3)
}

fn change_menu_hit(x: u16, width: u16, action_slot: bool) -> bool {
    let start = width.saturating_sub(change_tail_width(action_slot, true, true) as u16);
    x >= start && x < start.saturating_add(3)
}

fn section_action_start(title: &str, width: usize, count: usize) -> usize {
    let left_width = title.len() + 3;
    let badge_width = count.to_string().len() + 2;
    width
        .saturating_sub(badge_width + 3)
        .max(left_width + 1)
}

fn section_action_hit(x: u16, title: &str, width: u16, count: usize) -> bool {
    let glyph = section_action_start(title, width as usize, count) as u16;
    x >= glyph.saturating_sub(1) && x < glyph.saturating_add(2)
}

fn list_content_width(width: u16, total: usize, visible: usize) -> u16 {
    if width == 0 {
        0
    } else {
        width.saturating_sub(u16::from(total > visible)).max(1)
    }
}

fn directory_pathspecs(path: &str, entries: &[FileEntry]) -> Vec<String> {
    let mut paths = vec![path.to_string()];
    for entry in entries.iter().filter(|entry| {
        entry.path.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'))
    }) {
        let Some(orig) = entry.orig.as_ref() else { continue };
        let inside = orig.strip_prefix(path).is_some_and(|rest| rest.starts_with('/'));
        if !inside && !paths.contains(orig) {
            paths.push(orig.clone());
        }
    }
    paths
}

fn toggle_collapsed_path(collapsed: &mut BTreeSet<String>, path: &str, was_expanded: bool) {
    if was_expanded {
        collapsed.insert(path.to_string());
    } else {
        collapsed.retain(|candidate| {
            candidate != path
                && !path
                    .strip_prefix(candidate.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        });
    }
}

impl Row {
    /// The repository a row belongs to (drawers follow the active repo).
    fn repo(self) -> Option<usize> {
        match self {
            Row::RepoHeader(r)
            | Row::Message(r)
            | Row::Commit(r)
            | Row::StagedHeader(r)
            | Row::ChangesHeader(r)
            | Row::Staged(r, _)
            | Row::Unstaged(r, _) => Some(r),
            Row::RepoSeparator
            | Row::DrawerHeader(_)
            | Row::DrawerLine(..)
            | Row::HistoryTree(_)
            | Row::HistoryNotice => None,
        }
    }

    /// Keyboard navigation skips inline widgets and visual separators.
    fn selectable(self) -> bool {
        !matches!(
            self,
            Row::Message(_) | Row::Commit(_) | Row::RepoSeparator | Row::HistoryNotice
        )
    }
}

/// What a context menu is about.
#[derive(Clone)]
enum MenuTarget {
    File {
        repo: usize,
        entry: FileEntry,
        staged: bool,
    },
    Drawer {
        kind: Drawer,
        index: usize,
    },
    Directory {
        repo: usize,
        path: String,
        staged: bool,
    },
    HistoryFile {
        repo: usize,
        file: RefFile,
    },
}

/// Context-menu actions for file rows and drawer lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    OpenDiff,
    StageOrUnstage,
    Discard,
    CopyPath,
    CopyRelativePath,
    OpenExternal,
    Reveal,
    // Drawer-line actions (commits, branches, stashes, remotes, tags).
    ShowRef,
    Checkout,
    MergeInto,
    DeleteBranch,
    CherryPick,
    Revert,
    ResetHere,
    StashApply,
    StashPop,
    StashDrop,
    FetchRemote,
    CopyRef,
    DeleteTag,
    RemoveWorktree,
}

#[derive(Clone, Copy)]
enum MenuEntry {
    Action(MenuAction, &'static str),
    Separator,
}

/// A modal layered over the list; while open it owns keyboard and mouse input.
enum Overlay {
    Menu {
        x: u16,
        y: u16,
        target: MenuTarget,
        entries: Vec<MenuEntry>,
        selected: usize,
        rect: Rect,
    },
    ConfirmDiscard {
        repo: usize,
        entry: FileEntry,
    },
    /// A y/N prompt guarding a destructive git command (reset, delete, drop).
    ConfirmGit {
        repo: usize,
        prompt: String,
        args: Vec<String>,
    },
    /// The ⚙ settings modal: mouse-toggleable panel settings.
    Settings {
        selected: usize,
        rect: Rect,
    },
    ThemePicker {
        selected: usize,
        scroll: usize,
        rect: Rect,
    },
}

/// One row of the Settings modal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    UnifiedSidebar,
    IconTheme,
    DiffTheme,
    HideUnmodified,
    ScmView,
    AutoOpen,
    Hotkeys,
    Folder,
}

/// (setting, label, current value, enabled) — disabled rows render dimmed and
/// don't toggle.
type SettingRow = (Setting, &'static str, String, bool);

/// Where the list body was drawn last frame, for mouse hit-testing.
#[derive(Clone, Copy, Default)]
struct BodyGeom {
    left: u16,
    top: u16,
    height: u16,
    width: u16,
    offset: usize,
}

/// Clickable regions of the activity bar / header / message box, from the
/// last draw.
#[derive(Clone, Copy, Default)]
struct ClickZones {
    activity_row: u16,
    explorer: (u16, u16),
    source_control: (u16, u16),
    search: (u16, u16),
    /// The ⚙ button (activity bar in unified mode, header otherwise).
    gear: Rect,
    /// The permanent hard-redraw button immediately left of Settings.
    redraw: Rect,
    /// The main repository title row, excluding redraw and Settings.
    repo_header: Rect,
    message: Rect,
    sparkle: Rect,
    button: Rect,
    /// The Sync Changes row (zero-sized when hidden).
    sync: Rect,
}

/// Handle for identity/label control of our own pane over the socket API.
struct PaneCtl {
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty())?;
        Some(Self { pane_id })
    }

    /// Set or clear the pane label — cleared while collapsed so the sliver
    /// has no border title.
    fn set_label(&self, label: Option<&str>) {
        let mut params = serde_json::json!({ "pane_id": self.pane_id });
        if let Some(label) = label {
            params["label"] = serde_json::Value::String(label.to_string());
        }
        let _ = herdr_sidebar::ipc::call_text("pane.rename", params);
    }

    /// Resize our pane to `target` terminal columns over the socket API
    /// (`pane.resize` takes a split-RATIO delta; the plan converts columns).
    fn resize_to(&self, current: u16, target: u16) {
        let Ok(layout) = herdr_sidebar::ipc::call_text(
            "pane.layout",
            serde_json::json!({ "pane_id": self.pane_id }),
        ) else {
            return;
        };
        let Some(step) =
            herdr_sidebar::launch::resize_plan(&layout, &self.pane_id, current, target)
        else {
            return;
        };
        let _ = herdr_sidebar::ipc::call_text(
            "pane.resize",
            serde_json::json!({
                "pane_id": self.pane_id,
                "direction": step.direction,
                "amount": step.amount,
            }),
        );
    }

    /// Report identity tokens: always our own; in merged mode also the other
    /// view's (one Sidebar pane satisfies both plugins' launchers), otherwise
    /// clear the other view's token.
    fn report_tokens(&self, my: View, merged: bool) {
        herdr_sidebar::ipc::report_identity(&self.pane_id, my, merged);
    }
}

pub struct App {
    /// Every repository visible from the cwd (VS Code style: the containing
    /// repo plus child repos). Empty = "not a git repository".
    repos: Vec<Repo>,
    /// Why discovery came up empty, for the placeholder screen.
    discover_err: String,
    /// The repo the commit box / drawers / sync act on: the one the selection
    /// is in.
    active: usize,
    cwd: PathBuf,
    rows: Vec<Row>,
    /// Explicit selection — `None` until the user picks a row (nothing is
    /// highlighted by default; hover stays subtle).
    selected: Option<usize>,
    /// View scroll offset in ROWS, independent of the selection.
    scroll: usize,
    /// Bring the selection into view on the next draw (keyboard nav only).
    snap: bool,
    focus: Focus,
    theme: IconTheme,
    drawers: [DrawerPanel; 8],
    /// One expanded commit/branch/stash/tag file collection across all drawers.
    expanded_ref: Option<ExpandedRef>,
    /// The file the FILE HISTORY drawer follows: the last selected file row.
    history_target: Option<String>,
    /// One-shot footer notice: (text, is_error). Cleared on the next key press.
    flash: Option<(String, bool)>,
    /// Pending ✧ commit-message generation, polled from tick().
    suggesting: Option<Receiver<String>>,
    /// Pending Sync Changes run, polled from tick().
    syncing: Option<Receiver<Result<String, String>>>,
    overlay: Option<Overlay>,
    hovered: Option<usize>,
    body: BodyGeom,
    zones: ClickZones,
    redraw_requested: bool,
    /// The hover title-bar buttons' click zones from the last draw (empty
    /// while they are hidden).
    title_zones: Vec<(Rect, TitleAction)>,
    /// Multi-repository header actions generated by the same row layout as
    /// their rendered spans.
    repo_title_zones: Vec<(Rect, usize, TitleAction)>,
    /// When the mouse last moved/clicked/scrolled over this pane — the leave
    /// fallback for title actions and change tooltips.
    last_mouse: Option<std::time::Instant>,
    /// Last known mouse position, for the button hover highlight.
    mouse_pos: Option<(u16, u16)>,
    last_click: Option<(usize, std::time::Instant)>,
    page: usize,
    last_width: u16,
    last_height: u16,
    // Merged-sidebar state.
    sidebar_state: sidebar::State,
    other_exe: Option<PathBuf>,
    pane_ctl: Option<PaneCtl>,
    /// Last heartbeat stamp, throttling the token refresh.
    last_beat: std::time::Instant,
    /// A native folder picker running on a background thread; its result
    /// arrives here (None = cancelled).
    picking: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
}

const MY_VIEW: View = View::SourceControl;


impl App {
    pub fn new(cwd: PathBuf) -> Self {
        Self::new_with_pane(cwd, PaneCtl::from_env())
    }

    fn new_with_pane(cwd: PathBuf, pane_ctl: Option<PaneCtl>) -> Self {
        let repos: Vec<Repo> = Git::discover_all(&cwd).into_iter().map(Repo::new).collect();
        let discover_err = if repos.is_empty() {
            Git::discover(&cwd).err().unwrap_or_else(|| "no repositories found".to_string())
        } else {
            String::new()
        };
        let sidebar_state = sidebar::load_state();
        let theme = IconTheme::resolve(
            std::env::var("HERDR_SIDEBAR_ICONS")
                .or_else(|_| std::env::var("HERDR_AA_GIT_ICONS"))
                .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                .ok()
                .as_deref(),
            sidebar_state.icons,
        );
        // The other view ships in this same binary — always available.
        let other_exe = std::env::current_exe().ok();
        let mut app = Self {
            repos,
            discover_err,
            active: 0,
            cwd,
            rows: Vec::new(),
            selected: None,
            scroll: 0,
            snap: false,
            focus: Focus::List,
            theme,
            drawers: Default::default(),
            expanded_ref: None,
            history_target: None,
            flash: None,
            suggesting: None,
            syncing: None,
            overlay: None,
            hovered: None,
            body: BodyGeom::default(),
            zones: ClickZones::default(),
            redraw_requested: false,
            title_zones: Vec::new(),
            repo_title_zones: Vec::new(),
            last_mouse: None,
            mouse_pos: None,
            last_click: None,
            page: 20,
            last_width: 40,
            last_height: 24,
            sidebar_state,
            other_exe,
            pane_ctl,
            last_beat: std::time::Instant::now(),
            picking: None,
        };
        if app.pane_ctl.is_some() {
            app.apply_identity();
        }
        app.refresh();
        app
    }

    fn active_repo(&self) -> Option<&Repo> {
        self.repos.get(self.active)
    }

    fn active_repo_mut(&mut self) -> Option<&mut Repo> {
        let i = self.active;
        self.repos.get_mut(i)
    }

    fn status_tree_row(&self, repo: usize, row: usize, staged: bool) -> Option<&ChangeTreeRow> {
        let repo = self.repos.get(repo)?;
        if staged { repo.staged_rows.get(row) } else { repo.changes_rows.get(row) }
    }

    fn status_entry(&self, repo: usize, row: usize, staged: bool) -> Option<&FileEntry> {
        let index = match self.status_tree_row(repo, row, staged)? {
            ChangeTreeRow::File { index, .. } => *index,
            ChangeTreeRow::Directory { .. } => return None,
        };
        let repo = self.repos.get(repo)?;
        if staged { repo.status.staged.get(index) } else { repo.status.unstaged.get(index) }
    }

    fn hovered_change_tooltip(&self) -> Option<ChangeTooltip> {
        if !mouse_linger_active(self.last_mouse) {
            return None;
        }
        let row = *self.rows.get(self.hovered?)?;
        match row {
            Row::Staged(repo, row) => {
                let repo = self.repos.get(repo)?;
                let tree_row = repo.staged_rows.get(row)?;
                let entry = match tree_row {
                    ChangeTreeRow::File { index, .. } => repo.status.staged.get(*index),
                    ChangeTreeRow::Directory { .. } => None,
                };
                change_tree_tooltip(tree_row, entry, repo.status.staged.iter())
            }
            Row::Unstaged(repo, row) => {
                let repo = self.repos.get(repo)?;
                let tree_row = repo.changes_rows.get(row)?;
                let entry = match tree_row {
                    ChangeTreeRow::File { index, .. } => repo.status.unstaged.get(*index),
                    ChangeTreeRow::Directory { .. } => None,
                };
                change_tree_tooltip(tree_row, entry, repo.status.unstaged.iter())
            }
            Row::HistoryTree(row) => {
                let expanded = self.expanded_ref.as_ref()?;
                let tree_row = expanded.rows.get(row)?;
                let entry = match tree_row {
                    ChangeTreeRow::File { index, .. } => {
                        expanded.files.get(*index).map(|file| &file.entry)
                    }
                    ChangeTreeRow::Directory { .. } => None,
                };
                change_tree_tooltip(
                    tree_row,
                    entry,
                    expanded.files.iter().map(|file| &file.entry),
                )
            }
            _ => None,
        }
    }

    fn status_directory(&self, repo: usize, row: usize, staged: bool) -> Option<&str> {
        match self.status_tree_row(repo, row, staged)? {
            ChangeTreeRow::Directory { path, .. } => Some(path),
            ChangeTreeRow::File { .. } => None,
        }
    }

    fn status_row_path(&self, repo: usize, row: usize, staged: bool) -> Option<&str> {
        match self.status_tree_row(repo, row, staged)? {
            ChangeTreeRow::Directory { path, .. } => Some(path),
            ChangeTreeRow::File { index, .. } => {
                let repo = self.repos.get(repo)?;
                if staged {
                    repo.status.staged.get(*index).map(|entry| entry.path.as_str())
                } else {
                    repo.status.unstaged.get(*index).map(|entry| entry.path.as_str())
                }
            }
        }
    }

    /// More than one repo: VS Code-style per-repo inline inputs in the list.
    fn multi(&self) -> bool {
        self.repos.len() > 1
    }

    /// The merged sidebar is on and actually usable (other plugin present).
    fn merged(&self) -> bool {
        self.sidebar_state.merged && self.other_exe.is_some()
    }

    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn root(&self) -> PathBuf {
        self.cwd.clone()
    }

    pub fn workspace_state(&self) -> ScmState {
        let selected = self.selected.and_then(|index| self.anchor_at(index));
        let (top, top_offset) = self.top_anchor();
        ScmState {
            active_repo: self.active_repo().map(|repo| repo.git.root().to_path_buf()),
            focus: match self.focus {
                Focus::List => ScmFocus::List,
                Focus::Message => ScmFocus::Message,
                Focus::Commit => ScmFocus::Commit,
            },
            selected,
            top,
            top_offset,
            repos: self
                .repos
                .iter()
                .map(|repo| ScmRepoState {
                    root: repo.git.root().to_path_buf(),
                    collapsed: repo.collapsed,
                    staged_collapsed: repo.staged_collapsed,
                    changes_collapsed: repo.changes_collapsed,
                    staged_dirs: repo.staged_dirs.clone(),
                    changes_dirs: repo.changes_dirs.clone(),
                })
                .collect(),
            drawers: Drawer::ALL
                .iter()
                .copied()
                .filter(|drawer| self.drawers[drawer.index()].expanded)
                .map(sync_drawer)
                .collect(),
            expanded_ref: self.expanded_ref.as_ref().map(|expanded| ExpandedScmRef {
                drawer: sync_drawer(expanded.kind),
                target: sync_ref(&expanded.target),
                collapsed: expanded.collapsed.clone(),
            }),
        }
    }

    pub fn apply_workspace_state(&mut self, state: &ScmState) {
        let current = self.workspace_state();
        let selected_history = match state.selected.as_ref() {
            Some(ScmAnchor::Staged { path, .. } | ScmAnchor::Changes { path, .. }) => {
                Some(path.clone())
            }
            _ => None,
        };
        let history_changed = state.drawers.contains(&ScmDrawer::FileHistory)
            && selected_history.as_ref() != self.history_target.as_ref();
        let structure_changed = current.active_repo != state.active_repo
            || current.repos != state.repos
            || current.drawers != state.drawers
            || current.expanded_ref != state.expanded_ref;
        self.focus = match state.focus {
            ScmFocus::List => Focus::List,
            ScmFocus::Message => Focus::Message,
            ScmFocus::Commit => Focus::Commit,
        };
        if !structure_changed && !history_changed {
            self.selected = state.selected.as_ref().and_then(|anchor| self.find_anchor(anchor));
            self.scroll = self
                .find_top(state)
                .unwrap_or_else(|| self.selected.unwrap_or(0))
                .min(self.rows.len().saturating_sub(1));
            self.snap = false;
            self.hovered = None;
            self.mouse_pos = None;
            return;
        }
        if let Some(active) = state.active_repo.as_deref()
            && let Some(index) = self.repos.iter().position(|repo| repo.git.root() == active)
        {
            self.active = index;
        }
        for repo in &mut self.repos {
            if let Some(shared) = state.repos.iter().find(|shared| shared.root == repo.git.root()) {
                repo.collapsed = shared.collapsed;
                repo.staged_collapsed = shared.staged_collapsed;
                repo.changes_collapsed = shared.changes_collapsed;
                repo.staged_dirs.clone_from(&shared.staged_dirs);
                repo.changes_dirs.clone_from(&shared.changes_dirs);
                repo.rebuild_file_rows(self.sidebar_state.scm_file_view);
            }
        }
        if selected_history.is_some() {
            self.history_target = selected_history;
        }
        for drawer in Drawer::ALL {
            self.drawers[drawer.index()].expanded = state.drawers.contains(&sync_drawer(drawer));
        }
        self.expanded_ref = None;
        self.reload_expanded_drawers();
        self.rebuild();
        if let Some(shared) = &state.expanded_ref {
            let drawer = app_drawer(shared.drawer);
            if let Some(index) = self.drawers[drawer.index()]
                .refs
                .iter()
                .position(|target| sync_ref(target) == shared.target)
            {
                self.toggle_expanded_ref(drawer, index);
                if let Some(expanded) = self.expanded_ref.as_mut() {
                    expanded.collapsed.clone_from(&shared.collapsed);
                    expanded.rebuild_rows(self.sidebar_state.scm_file_view);
                    self.rebuild();
                }
            }
        }
        self.selected = state.selected.as_ref().and_then(|anchor| self.find_anchor(anchor));
        self.scroll = self
            .find_top(state)
            .unwrap_or_else(|| self.selected.unwrap_or(0))
            .min(self.rows.len().saturating_sub(1));
        self.snap = false;
        self.hovered = None;
        self.mouse_pos = None;
    }

    pub fn workspace_width(&self) -> u16 {
        self.last_width
    }

    pub fn take_redraw_request(&mut self) -> bool {
        std::mem::take(&mut self.redraw_requested)
    }

    pub fn workspace_sync_enabled(&self) -> bool {
        self.merged()
    }

    pub fn apply_workspace_width(&self, width: u16) {
        if width > 0 && width != self.last_width
            && let Some(ctl) = &self.pane_ctl
        {
            ctl.resize_to(self.last_width, width);
        }
    }

    fn anchor_at(&self, index: usize) -> Option<ScmAnchor> {
        match *self.rows.get(index)? {
            Row::RepoHeader(repo) => Some(ScmAnchor::Repo(self.repo_root(repo)?)),
            Row::Message(repo) => Some(ScmAnchor::Message(self.repo_root(repo)?)),
            Row::Commit(repo) => Some(ScmAnchor::Commit(self.repo_root(repo)?)),
            Row::StagedHeader(repo) => Some(ScmAnchor::StagedHeader(self.repo_root(repo)?)),
            Row::ChangesHeader(repo) => Some(ScmAnchor::ChangesHeader(self.repo_root(repo)?)),
            Row::Staged(repo, row) => Some(ScmAnchor::Staged {
                repo: self.repo_root(repo)?,
                path: self.status_row_path(repo, row, true)?.to_string(),
            }),
            Row::Unstaged(repo, row) => Some(ScmAnchor::Changes {
                repo: self.repo_root(repo)?,
                path: self.status_row_path(repo, row, false)?.to_string(),
            }),
            Row::DrawerHeader(drawer) => Some(ScmAnchor::DrawerHeader(sync_drawer(drawer))),
            Row::DrawerLine(drawer, line) => Some(ScmAnchor::DrawerLine {
                drawer: sync_drawer(drawer),
                target: sync_ref(self.drawers[drawer.index()].refs.get(line)?),
            }),
            Row::HistoryTree(row) => Some(ScmAnchor::HistoryPath(
                self.expanded_ref_path(row)?.to_string(),
            )),
            Row::HistoryNotice => Some(ScmAnchor::HistoryNotice),
            Row::RepoSeparator => None,
        }
    }

    fn find_anchor(&self, anchor: &ScmAnchor) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .find_map(|(index, _)| (self.anchor_at(index).as_ref() == Some(anchor)).then_some(index))
    }

    fn top_anchor(&self) -> (Option<ScmAnchor>, usize) {
        for index in self.scroll..self.rows.len() {
            if let Some(anchor) = self.anchor_at(index) {
                return (Some(anchor), index - self.scroll);
            }
        }
        (
            (0..self.scroll).rev().find_map(|index| self.anchor_at(index)),
            0,
        )
    }

    fn find_top(&self, state: &ScmState) -> Option<usize> {
        let index = self.find_anchor(state.top.as_ref()?)?;
        Some(index.saturating_sub(state.top_offset))
    }

    fn repo_root(&self, repo: usize) -> Option<PathBuf> {
        self.repos.get(repo).map(|repo| repo.git.root().to_path_buf())
    }

    fn expanded_ref_path(&self, row: usize) -> Option<&str> {
        let expanded = self.expanded_ref.as_ref()?;
        match expanded.rows.get(row)? {
            ChangeTreeRow::Directory { path, .. } => Some(path),
            ChangeTreeRow::File { index, .. } => {
                expanded.files.get(*index).map(|file| file.entry.path.as_str())
            }
        }
    }

    /// Push our label + metadata tokens to herdr for the current mode.
    fn apply_identity(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        let label = if self.merged() { sidebar::SIDEBAR_LABEL } else { MY_VIEW.label() };
        ctl.set_label(Some(label));
        ctl.report_tokens(MY_VIEW, self.merged());
    }

    /// Hide the sidebar: snooze this tab (so the quiet ensure hook doesn't
    /// immediately re-dock a fresh one) and close our own pane. The herdr
    /// prefix+b keybinding (→ the toggle action) brings it back.
    fn hide(&mut self) {
        let Some(ctl) = &self.pane_ctl else { return };
        if let Ok(json) =
            herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({}))
        {
            let tab = herdr_sidebar::launch::tab_of(&json, &ctl.pane_id);
            herdr_sidebar::snooze::set(&herdr_sidebar::snooze::dir(), &tab);
        }
        let _ = herdr_sidebar::ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": ctl.pane_id }),
        );
    }

    /// Re-read every repo's git status (this is the change auto-detection —
    /// tick() calls it every [`crate::REFRESH_EVERY`]); keeps the flash so
    /// periodic ticks don't eat notices.
    pub fn refresh(&mut self) {
        self.sidebar_state.scm_file_view = sidebar::load_state().scm_file_view;
        let mut error = None;
        for repo in &mut self.repos {
            match repo.git.status() {
                Ok(status) => {
                    repo.status = status;
                    repo.rebuild_file_rows(self.sidebar_state.scm_file_view);
                }
                Err(e) => error = Some(e),
            }
        }
        if let Some(e) = error {
            self.flash = Some((e, true));
        }
        self.reload_expanded_drawers();
        self.rebuild();
    }

    /// Re-stamp the identity tokens so launchers know this pane is alive.
    pub fn heartbeat(&mut self) {
        if self.last_beat.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_beat = std::time::Instant::now();
        if let Some(ctl) = &self.pane_ctl {
            ctl.report_tokens(MY_VIEW, self.merged());
        }
    }

    /// Periodic timer tick: retry repo discovery if we started outside one,
    /// pick up external changes, and collect finished ✧ suggestion / sync runs.
    pub fn tick(&mut self) {
        if self.repos.is_empty() {
            self.repos = Git::discover_all(&self.cwd).into_iter().map(Repo::new).collect();
            if !self.repos.is_empty() {
                self.discover_err.clear();
            }
        }
        if let Some(rx) = &self.suggesting {
            match rx.try_recv() {
                Ok(message) => {
                    if let Some(repo) = self.active_repo_mut() {
                        repo.message = message.chars().collect();
                        repo.cursor = repo.message.len();
                    }
                    self.focus = Focus::Message;
                    self.flash = Some(("✧ suggestion ready — edit or ⏎ to commit".into(), false));
                    self.suggesting = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.flash = Some(("✧ generation failed".into(), true));
                    self.suggesting = None;
                }
            }
        }
        if let Some(rx) = &self.syncing {
            match rx.try_recv() {
                Ok(Ok(summary)) => {
                    self.flash = Some((summary, false));
                    self.syncing = None;
                }
                Ok(Err(e)) => {
                    self.flash = Some((e, true));
                    self.syncing = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.flash = Some(("sync failed".into(), true));
                    self.syncing = None;
                }
            }
        }
        self.refresh();
    }

    /// The title bar's Collapse All only owns the repository area above the
    /// heavy divider. Graph and the remaining drawers are a separate component.
    fn collapse_all(&mut self) {
        for repo in &mut self.repos {
            repo.collapsed = true;
            repo.staged_collapsed = true;
            repo.changes_collapsed = true;
            repo.staged_tree.collapse_all(&mut repo.staged_dirs);
            repo.changes_tree.collapse_all(&mut repo.changes_dirs);
        }
        self.focus = Focus::List;
        self.scroll = 0;
        self.rebuild();
    }

    fn expand_all(&mut self) {
        for repo in &mut self.repos {
            repo.collapsed = false;
            repo.staged_collapsed = false;
            repo.changes_collapsed = false;
            repo.staged_dirs.clear();
            repo.changes_dirs.clear();
        }
        self.scroll = 0;
        self.rebuild();
    }

    fn fully_collapsed(&self) -> bool {
        self.repos.iter().all(|repo| repo.collapsed)
    }

    fn reload_expanded_drawers(&mut self) {
        let Some(git) = self.active_repo().map(|r| r.git.clone()) else { return };
        let git = &git;
        for kind in Drawer::ALL {
            if !self.drawers[kind.index()].expanded {
                continue;
            }
            let panel = &mut self.drawers[kind.index()];
            panel.files.clear();
            if kind == Drawer::FileHistory {
                match &self.history_target {
                    Some(path) => match git.file_history(path, DRAWER_LIMIT) {
                        Ok(entries) if entries.is_empty() => {
                            panel.lines = vec!["(none)".to_string()];
                            panel.refs = vec![DrawerRef::None];
                            panel.files = vec![None];
                        }
                        Ok(entries) => {
                            panel.lines = entries.iter().map(|entry| entry.line.clone()).collect();
                            panel.refs = entries
                                .iter()
                                .map(|entry| DrawerRef::Commit(entry.file.new_spec.clone()))
                                .collect();
                            panel.files = entries.into_iter().map(|entry| Some(entry.file)).collect();
                        }
                        Err(e) => {
                            panel.lines = vec![format!("({e})")];
                            panel.refs = vec![DrawerRef::None];
                            panel.files = vec![None];
                        }
                    },
                    None => {
                        panel.lines = vec!["(select a file above)".to_string()];
                        panel.refs = vec![DrawerRef::None];
                        panel.files = vec![None];
                    }
                }
                continue;
            }
            let lines = match kind {
                Drawer::Graph => git.graph(DRAWER_LIMIT),
                Drawer::Commits => git.commits(DRAWER_LIMIT),
                Drawer::Branches => git.branches(),
                Drawer::Worktrees => git.worktrees(),
                Drawer::Remotes => git.remotes(),
                Drawer::Stashes => git.stashes(),
                Drawer::Tags => git.tags(),
                Drawer::FileHistory => unreachable!(),
            };
            panel.lines = match lines {
                Ok(lines) if lines.is_empty() => vec!["(none)".to_string()],
                Ok(lines) => lines,
                Err(e) => vec![format!("({e})")],
            };
            panel.refs = panel.lines.iter().map(|line| parse_drawer_ref(kind, line)).collect();
            panel.files.resize(panel.lines.len(), None);
            match kind {
                Drawer::Worktrees => {
                    panel.lines = panel.lines.iter().map(|line| pretty_worktree_line(line)).collect();
                }
                Drawer::Remotes => {
                    panel.lines = panel.lines.iter().map(|line| pretty_remote_line(line)).collect();
                }
                _ => {}
            }
        }
    }

    fn rebuild(&mut self) {
        self.hovered = None;
        self.repo_title_zones.clear();
        self.rows.clear();
        for repo in &mut self.repos {
            repo.rebuild_file_rows(self.sidebar_state.scm_file_view);
        }
        if let Some(expanded) = &mut self.expanded_ref {
            expanded.rebuild_rows(self.sidebar_state.scm_file_view);
        }
        let multi = self.repos.len() > 1;
        if multi {
            self.rows.push(Row::RepoSeparator);
        }
        for (r, repo) in self.repos.iter().enumerate() {
            if multi {
                self.rows.push(Row::RepoHeader(r));
            }
            if repo.collapsed {
                if multi {
                    self.rows.push(Row::RepoSeparator);
                }
                continue;
            }
            if multi {
                // VS Code gives every repo its own message box and Commit
                // button, inline in the list.
                self.rows.push(Row::Message(r));
                self.rows.push(Row::Commit(r));
            }
            // Like VS Code, the Staged section only exists while something is staged.
            if !repo.status.staged.is_empty() {
                self.rows.push(Row::StagedHeader(r));
                if !repo.staged_collapsed {
                    for i in 0..repo.staged_rows.len() {
                        self.rows.push(Row::Staged(r, i));
                    }
                }
            }
            self.rows.push(Row::ChangesHeader(r));
            if !repo.changes_collapsed {
                for i in 0..repo.changes_rows.len() {
                    self.rows.push(Row::Unstaged(r, i));
                }
            }
            if multi {
                self.rows.push(Row::RepoSeparator);
            }
        }
        if !multi && !self.repos.is_empty() {
            self.rows.push(Row::RepoSeparator);
        }
        for kind in Drawer::ALL {
            self.rows.push(Row::DrawerHeader(kind));
            if self.drawers[kind.index()].expanded {
                for i in 0..self.drawers[kind.index()].lines.len() {
                    self.rows.push(Row::DrawerLine(kind, i));
                    let is_expanded = self.expanded_ref.as_ref().is_some_and(|expanded| {
                        expanded.kind == kind
                            && self.drawers[kind.index()].refs.get(i) == Some(&expanded.target)
                    });
                    if is_expanded {
                        let expanded = self.expanded_ref.as_ref().expect("checked above");
                        if expanded.error.is_some() || expanded.files.is_empty() {
                            self.rows.push(Row::HistoryNotice);
                        } else {
                            for row in 0..expanded.rows.len() {
                                self.rows.push(Row::HistoryTree(row));
                            }
                        }
                    }
                }
            }
        }
        if self.rows.is_empty() {
            self.selected = None;
            self.scroll = 0;
            return;
        }
        if let Some(sel) = self.selected {
            let index = sel.min(self.rows.len() - 1);
            self.selected = Some(self.nearest_selectable(index));
        }
        self.scroll = self.scroll.min(self.rows.len() - 1);
        self.follow_selection();
    }

    /// The closest keyboard-selectable row to `from` (widget rows — inline
    /// message boxes and commit buttons — are skipped).
    fn nearest_selectable(&self, from: usize) -> usize {
        if self.rows.get(from).is_some_and(|r| r.selectable()) {
            return from;
        }
        let after = (from..self.rows.len()).find(|&i| self.rows[i].selectable());
        let before = (0..from).rev().find(|&i| self.rows[i].selectable());
        after.or(before).unwrap_or(0)
    }

    /// Keep the active repo and the FILE HISTORY drawer following the
    /// selection: drawers, commit box, and sync all act on the selected
    /// row's repository.
    fn follow_selection(&mut self) {
        let selected = self.selected.and_then(|i| self.rows.get(i)).copied();
        if let Some(r) = selected.and_then(Row::repo)
            && r != self.active
            && r < self.repos.len()
        {
            self.set_active_repo(r);
            let keep = self.selected;
            self.rebuild();
            if let Some(i) = keep {
                self.selected = Some(i.min(self.rows.len().saturating_sub(1)));
            }
            return;
        }
        let path = match selected {
            Some(Row::Staged(r, i)) if r == self.active => self.status_entry(r, i, true).map(|e| e.path.clone()),
            Some(Row::Unstaged(r, i)) if r == self.active => {
                self.status_entry(r, i, false).map(|e| e.path.clone())
            }
            _ => return, // keep the last file while browsing elsewhere
        };
        if path.is_some() && path != self.history_target {
            self.history_target = path;
            if self.drawers[Drawer::FileHistory.index()].expanded {
                self.reload_expanded_drawers();
                // Line count may have changed; rebuild WITHOUT re-entering
                // follow_selection (path is unchanged now).
                let selected = self.selected;
                self.rebuild();
                if let Some(i) = selected {
                    self.selected = Some(i.min(self.rows.len() - 1));
                }
            }
        }
    }

    fn set_active_repo(&mut self, repo: usize) {
        if repo == self.active || repo >= self.repos.len() {
            return;
        }
        self.active = repo;
        self.history_target = None;
        self.expanded_ref = None;
        self.reload_expanded_drawers();
    }

    /// Handle one key press; `Some(exit)` ends the event loop.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.flash = None;
        if self.overlay.is_some() {
            self.overlay_key(key);
            return None;
        }
        match self.focus {
            Focus::Message => self.on_message_key(key),
            Focus::Commit => self.on_button_key(key),
            Focus::List => return self.on_list_key(key),
        }
        None
    }

    fn on_message_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.commit();
                return;
            }
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab => self.focus = Focus::Commit,
            KeyCode::BackTab => self.focus = Focus::List,
            KeyCode::Down => self.focus = Focus::Commit,
            _ => {}
        }
        let Some(repo) = self.active_repo_mut() else { return };
        match key.code {
            KeyCode::Backspace => {
                if repo.cursor > 0 {
                    repo.cursor -= 1;
                    repo.message.remove(repo.cursor);
                }
            }
            KeyCode::Delete => {
                if repo.cursor < repo.message.len() {
                    repo.message.remove(repo.cursor);
                }
            }
            KeyCode::Left => repo.cursor = repo.cursor.saturating_sub(1),
            KeyCode::Right => repo.cursor = (repo.cursor + 1).min(repo.message.len()),
            KeyCode::Home => repo.cursor = 0,
            KeyCode::End => repo.cursor = repo.message.len(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                repo.message.clear();
                repo.cursor = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                repo.message.insert(repo.cursor, c);
                repo.cursor += 1;
            }
            _ => {}
        }
    }

    fn on_button_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => self.commit(),
            KeyCode::Esc => self.focus = Focus::List,
            KeyCode::Tab | KeyCode::Down => self.focus = Focus::List,
            KeyCode::BackTab | KeyCode::Up => self.focus = Focus::Message,
            _ => {}
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) -> Option<Exit> {
        match key.code {
            KeyCode::Char('q') => return Some(Exit::Quit),
            // Esc never quits the sidebar — it closes the preview instead.
            KeyCode::Esc => self.close_preview(),
            KeyCode::Tab => self.focus = Focus::Message,
            KeyCode::BackTab => self.focus = Focus::Commit,
            KeyCode::Char('c') => self.focus = Focus::Message,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::Left | KeyCode::Char('h') => self.tree_nav(false),
            KeyCode::Right | KeyCode::Char('l') => self.tree_nav(true),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Char('a') => self.stage_all(),
            KeyCode::Char('u') => self.unstage_all(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('i') => self.set_theme(self.theme.toggled()),
            KeyCode::Char('A') => self.suggest_message(),
            KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('S') => self.sync_changes(),
            KeyCode::Char('o') => self.open_selected_diff(),
            KeyCode::Char('b') => self.hide(),
            KeyCode::Char('1') => return self.switch_to(View::Explorer),
            KeyCode::Char('2') => return self.switch_to(View::SourceControl),
            KeyCode::Char('3') => return self.switch_to(View::Search),
            _ => {}
        }
        None
    }

    /// `Some(exit)` ends the event loop, mirroring on_key.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        // Keep the last pane-local pointer position for row and title hover.
        // The timestamp is only a fallback for terminals lacking leave events.
        self.last_mouse = Some(std::time::Instant::now());
        self.mouse_pos = Some((mouse.column, mouse.row));
        if self.overlay.is_some() {
            self.overlay_mouse(mouse);
            return None;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                self.hovered = self.row_at(mouse.row);
            }
            MouseEventKind::ScrollUp => self.scroll_view(-3),
            MouseEventKind::ScrollDown => self.scroll_view(3),
            MouseEventKind::Down(MouseButton::Left) => {
                self.hovered = self.row_at(mouse.row);
                return self.left_click(mouse);
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Reaches us only as Ctrl+right-click (herdr's passthrough
                // modifier); plain right-click opens herdr's own pane menu.
                self.flash = None;
                self.open_context_menu(mouse.column, mouse.row);
            }
            _ => {}
        }
        None
    }

    pub fn on_focus_lost(&mut self) {
        self.hovered = None;
        self.mouse_pos = None;
        self.last_mouse = None;
        self.title_zones.clear();
        self.repo_title_zones.clear();
    }

    fn left_click(&mut self, mouse: MouseEvent) -> Option<Exit> {
        self.flash = None;
        if hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height) {
            self.hide();
            return None;
        }
        let (x, y) = (mouse.column, mouse.row);
        let z = self.zones;
        if self.merged() && y == z.activity_row {
            if within(x, z.explorer) {
                return self.switch_to(View::Explorer);
            }
            if within(x, z.source_control) {
                return self.switch_to(View::SourceControl);
            }
            if within(x, z.search) {
                return self.switch_to(View::Search);
            }
        }
        if hits(z.redraw, x, y) {
            self.redraw_requested = true;
            return None;
        }
        if hits(z.gear, x, y) {
            self.open_settings();
            return None;
        }
        if let Some(&(_, action)) =
            self.title_zones.iter().find(|(rect, _)| hits(*rect, x, y))
        {
            match action {
                TitleAction::Refresh => self.refresh(),
                TitleAction::CollapseAll => self.collapse_all(),
                TitleAction::ExpandAll => self.expand_all(),
                _ => {}
            }
            return None;
        }
        if !self.multi() && hits(z.repo_header, x, y) {
            self.focus = Focus::List;
            self.toggle_repo(0);
            return None;
        }
        if let Some((repo, action)) = self
            .repo_title_zones
            .iter()
            .find(|(rect, _, _)| hits(*rect, x, y))
            .map(|(_, repo, action)| (*repo, *action))
        {
            self.focus = Focus::List;
            if let Some((index, _)) = self.row_hit(y) {
                self.select(index);
            }
            match action {
                TitleAction::Sync => self.sync_repo(repo),
                TitleAction::Commit => self.commit_repo(repo),
                _ => {}
            }
            return None;
        }
        if hits(z.sparkle, x, y) {
            self.suggest_message();
            return None;
        }
        if hits(z.message, x, y) {
            self.focus = Focus::Message;
            return None;
        }
        if hits(z.button, x, y) {
            self.focus = Focus::Commit;
            self.commit();
            return None;
        }
        if hits(z.sync, x, y) {
            self.sync_changes();
            return None;
        }
        if let Some((index, line)) = self.row_hit(y) {
            let _ = line;
            let row_x = x.saturating_sub(self.body.left);
            let row_width = self.body.width;
            let now = std::time::Instant::now();
            let double = self
                .last_click
                .take()
                .is_some_and(|(previous, at)| previous == index && now.duration_since(at) < DOUBLE_CLICK);
            self.last_click = Some((index, now));
            let action_visible = self.hovered == Some(index);
            match self.rows[index] {
                // Clicking a changed file shows its diff, like VS Code —
                // except on the hover − / + zone, which unstages/stages it.
                Row::Staged(r, i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if action_visible && change_menu_hit(row_x, row_width, true) {
                        self.open_context_menu(x, y);
                        return None;
                    }
                    if let Some(path) = self.status_directory(r, i, true).map(str::to_string) {
                        if action_visible && change_action_hit(row_x, row_width) {
                            self.run_directory(r, path, true);
                        } else if change_tree_chevron_hit(row_x, self.status_tree_row(r, i, true))
                            || double
                        {
                            self.toggle_status_directory(r, i, true);
                        }
                    } else if let Some(entry) = self.status_entry(r, i, true).cloned() {
                        if action_visible && change_action_hit(row_x, row_width) {
                            if let Err(e) = self.repos[r].git.unstage(&entry) {
                                self.flash = Some((e, true));
                            }
                            self.refresh();
                        } else {
                            self.open_diff(r, &entry, true);
                        }
                    }
                }
                Row::Unstaged(r, i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if action_visible && change_menu_hit(row_x, row_width, true) {
                        self.open_context_menu(x, y);
                        return None;
                    }
                    if let Some(path) = self.status_directory(r, i, false).map(str::to_string) {
                        if action_visible && change_action_hit(row_x, row_width) {
                            self.run_directory(r, path, false);
                        } else if change_tree_chevron_hit(row_x, self.status_tree_row(r, i, false))
                            || double
                        {
                            self.toggle_status_directory(r, i, false);
                        }
                    } else if let Some(entry) = self.status_entry(r, i, false).cloned() {
                        if action_visible && change_action_hit(row_x, row_width) {
                            if let Err(e) = self.repos[r].git.stage(&entry) {
                                self.flash = Some((e, true));
                            }
                            self.refresh();
                        } else {
                            self.open_diff(r, &entry, false);
                        }
                    }
                }
                // The inline widgets: click focuses/acts without selecting.
                Row::Message(r) => {
                    self.set_active_repo(r);
                    self.rebuild();
                    // The box's middle line holds the input and the ✧ button.
                    if line == 1 && row_x >= row_width.saturating_sub(4) {
                        self.suggest_message();
                    } else {
                        self.focus = Focus::Message;
                    }
                    self.follow_selection();
                }
                Row::Commit(r) => {
                    // Only the button line commits — not its padding rows.
                    if line == 1 {
                        self.commit_repo(r);
                    }
                }
                Row::RepoHeader(_) => {
                    self.focus = Focus::List;
                    self.select(index);
                    self.activate();
                }
                // Header hover −/+ unstages/stages the whole section.
                Row::StagedHeader(r) => {
                    self.focus = Focus::List;
                    self.select(index);
                    let count = self.repos[r].status.staged.len();
                    if action_visible
                        && section_action_hit(row_x, "Staged Changes", row_width, count)
                    {
                        if let Some(repo) = self.repos.get(r)
                            && let Err(e) = repo.git.unstage_all()
                        {
                            self.flash = Some((e, true));
                        }
                        self.refresh();
                        self.select_status_header(r, false);
                    } else {
                        self.activate();
                    }
                }
                Row::ChangesHeader(r) => {
                    self.focus = Focus::List;
                    self.select(index);
                    let count = self.repos[r].status.unstaged.len();
                    if action_visible && section_action_hit(row_x, "Changes", row_width, count) {
                        if let Some(repo) = self.repos.get(r)
                            && let Err(e) = repo.git.stage_all()
                        {
                            self.flash = Some((e, true));
                        }
                        self.refresh();
                        self.select_status_header(r, true);
                    } else {
                        self.activate();
                    }
                }
                Row::DrawerHeader(_) => {
                    self.focus = Focus::List;
                    self.select(index);
                    self.activate();
                }
                Row::DrawerLine(kind, i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    self.open_drawer_ref(kind, i);
                }
                Row::HistoryTree(i) => {
                    self.focus = Focus::List;
                    self.select(index);
                    if action_visible
                        && matches!(
                            self.expanded_ref.as_ref().and_then(|expanded| expanded.rows.get(i)),
                            Some(ChangeTreeRow::File { .. })
                        )
                        && change_menu_hit(row_x, row_width, false)
                    {
                        self.open_context_menu(x, y);
                        return None;
                    }
                    match self.expanded_ref.as_ref().and_then(|expanded| expanded.rows.get(i)) {
                        Some(ChangeTreeRow::Directory { .. }) => {
                            if change_tree_chevron_hit_at(
                                row_x,
                                self.expanded_ref.as_ref().and_then(|e| e.rows.get(i)),
                                4,
                            ) || double
                            {
                                self.toggle_history_directory(i);
                            }
                        }
                        Some(ChangeTreeRow::File { index, .. }) => self.open_history_file(*index),
                        None => {}
                    }
                }
                Row::HistoryNotice => {}
                Row::RepoSeparator => {}
            }
        }
        None
    }

    /// Open the VS Code-style context menu for the row under the pointer.
    fn open_context_menu(&mut self, x: u16, y: u16) {
        let Some(index) = self.row_at(y) else { return };
        if self.rows[index] == Row::RepoSeparator {
            return;
        }
        self.select(index);
        let (repo, entry, staged) = match self.rows[index] {
            Row::Staged(r, i) => {
                if let Some(path) = self.status_directory(r, i, true).map(str::to_string) {
                    self.open_directory_menu(x, y, r, path, true);
                    return;
                }
                (r, self.status_entry(r, i, true), true)
            }
            Row::Unstaged(r, i) => {
                if let Some(path) = self.status_directory(r, i, false).map(str::to_string) {
                    self.open_directory_menu(x, y, r, path, false);
                    return;
                }
                (r, self.status_entry(r, i, false), false)
            }
            Row::DrawerLine(kind, i) => {
                self.open_drawer_menu(x, y, kind, i);
                return;
            }
            Row::HistoryTree(i) => {
                let Some(ChangeTreeRow::File { index, .. }) = self
                    .expanded_ref
                    .as_ref()
                    .and_then(|expanded| expanded.rows.get(i))
                else {
                    return;
                };
                let Some(file) = self
                    .expanded_ref
                    .as_ref()
                    .and_then(|expanded| expanded.files.get(*index))
                    .cloned()
                else {
                    return;
                };
                self.overlay = Some(Overlay::Menu {
                    x,
                    y,
                    target: MenuTarget::HistoryFile { repo: self.active, file },
                    entries: vec![
                        MenuEntry::Action(MenuAction::OpenDiff, "Open Diff"),
                        MenuEntry::Separator,
                        MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
                    ],
                    selected: 0,
                    rect: Rect::default(),
                });
                return;
            }
            _ => return, // section headers have no menu
        };
        let Some(entry) = entry.cloned() else { return };
        let mut entries = vec![MenuEntry::Action(MenuAction::OpenDiff, "Open Diff")];
        // A deleted file has nothing left on disk to hand to the shell.
        if entry.letter != 'D' {
            entries.push(MenuEntry::Action(MenuAction::OpenExternal, "Open with Default App"));
        }
        entries.push(MenuEntry::Action(
            MenuAction::StageOrUnstage,
            if staged { "Unstage Changes" } else { "Stage Changes" },
        ));
        if !staged {
            entries.push(MenuEntry::Action(MenuAction::Discard, "Discard Changes…"));
        }
        entries.extend([
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
            MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
        ]);
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target: MenuTarget::File { repo, entry, staged },
            entries,
            selected: 0,
            rect: Rect::default(),
        });
    }

    fn open_directory_menu(
        &mut self,
        x: u16,
        y: u16,
        repo: usize,
        path: String,
        staged: bool,
    ) {
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target: MenuTarget::Directory { repo, path, staged },
            entries: vec![
                MenuEntry::Action(
                    MenuAction::StageOrUnstage,
                    if staged { "Unstage Changes" } else { "Stage Changes" },
                ),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRelativePath, "Copy Relative Path"),
            ],
            selected: 0,
            rect: Rect::default(),
        });
    }

    /// The context menu for a commit / branch / stash / remote / tag.
    fn open_drawer_menu(&mut self, x: u16, y: u16, kind: Drawer, index: usize) {
        let Some(dref) = self.drawers[kind.index()].refs.get(index) else { return };
        let entries: Vec<MenuEntry> = match (kind, dref) {
            (Drawer::FileHistory, DrawerRef::Commit(_)) => vec![
                MenuEntry::Action(MenuAction::OpenDiff, "Open File Diff"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::Checkout, "Checkout (Detached)"),
                MenuEntry::Action(MenuAction::CherryPick, "Cherry-Pick"),
                MenuEntry::Action(MenuAction::Revert, "Revert"),
                MenuEntry::Action(MenuAction::ResetHere, "Reset Current Branch Here…"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRef, "Copy Hash"),
            ],
            (Drawer::Graph, DrawerRef::Commit(_)) => vec![
                MenuEntry::Action(MenuAction::ShowRef, "Show Changes"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::Checkout, "Checkout (Detached)"),
                MenuEntry::Action(MenuAction::CherryPick, "Cherry-Pick"),
                MenuEntry::Action(MenuAction::Revert, "Revert"),
                MenuEntry::Action(MenuAction::ResetHere, "Reset Current Branch Here…"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRef, "Copy Hash"),
            ],
            (Drawer::Commits, DrawerRef::Commit(_)) => vec![
                MenuEntry::Action(MenuAction::Checkout, "Checkout (Detached)"),
                MenuEntry::Action(MenuAction::CherryPick, "Cherry-Pick"),
                MenuEntry::Action(MenuAction::Revert, "Revert"),
                MenuEntry::Action(MenuAction::ResetHere, "Reset Current Branch Here…"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRef, "Copy Hash"),
            ],
            (_, DrawerRef::Branch { current: true, .. }) => vec![
                MenuEntry::Action(MenuAction::CopyRef, "Copy Branch Name"),
            ],
            (_, DrawerRef::Branch { current: false, .. }) => vec![
                MenuEntry::Action(MenuAction::Checkout, "Checkout Branch"),
                MenuEntry::Action(MenuAction::MergeInto, "Merge into Current Branch"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::DeleteBranch, "Delete Branch…"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRef, "Copy Branch Name"),
            ],
            (_, DrawerRef::Stash(_)) => vec![
                MenuEntry::Action(MenuAction::StashApply, "Apply Stash"),
                MenuEntry::Action(MenuAction::StashPop, "Pop Stash"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::StashDrop, "Drop Stash…"),
            ],
            (_, DrawerRef::Remote { .. }) => vec![
                MenuEntry::Action(MenuAction::FetchRemote, "Fetch"),
                MenuEntry::Action(MenuAction::CopyRef, "Copy URL"),
            ],
            (_, DrawerRef::Tag(_)) => vec![
                MenuEntry::Action(MenuAction::Checkout, "Checkout Tag"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::DeleteTag, "Delete Tag…"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::CopyRef, "Copy Tag Name"),
            ],
            (_, DrawerRef::Worktree(_)) => vec![
                MenuEntry::Action(MenuAction::Reveal, "Reveal in File Explorer"),
                MenuEntry::Action(MenuAction::CopyRef, "Copy Path"),
                MenuEntry::Separator,
                MenuEntry::Action(MenuAction::RemoveWorktree, "Remove Worktree…"),
            ],
            (_, DrawerRef::None) | (_, DrawerRef::Commit(_)) => return,
        };
        self.overlay = Some(Overlay::Menu {
            x,
            y,
            target: MenuTarget::Drawer { kind, index },
            entries,
            selected: 0,
            rect: Rect::default(),
        });
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            BackToSettings,
            ChooseTheme(usize),
            DiscardConfirmed(usize, FileEntry),
            GitConfirmed(usize, Vec<String>),
        }
        let row_count = self.settings_rows().len();
        let theme_count = syntax::diff_themes().len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::Menu { entries, selected, .. }) => match key.code {
                KeyCode::Esc => Cmd::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = step_menu(entries, *selected, -1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = step_menu(entries, *selected, 1);
                    Cmd::Nothing
                }
                KeyCode::Enter => Cmd::Activate,
                _ => Cmd::Nothing,
            },
            Some(Overlay::ThemePicker { selected, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Cmd::BackToSettings,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(theme_count.saturating_sub(1));
                    Cmd::Nothing
                }
                KeyCode::PageUp => {
                    *selected = selected.saturating_sub(10);
                    Cmd::Nothing
                }
                KeyCode::PageDown => {
                    *selected = (*selected + 10).min(theme_count.saturating_sub(1));
                    Cmd::Nothing
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    *selected = 0;
                    Cmd::Nothing
                }
                KeyCode::End | KeyCode::Char('G') => {
                    *selected = theme_count.saturating_sub(1);
                    Cmd::Nothing
                }
                KeyCode::Enter | KeyCode::Char(' ') => Cmd::ChooseTheme(*selected),
                _ => Cmd::Nothing,
            },
            Some(Overlay::Settings { selected, .. }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => Cmd::Close,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                    Cmd::Nothing
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(row_count.saturating_sub(1));
                    Cmd::Nothing
                }
                KeyCode::Enter | KeyCode::Char(' ') => Cmd::ToggleSetting(*selected),
                _ => Cmd::Nothing,
            },
            Some(Overlay::ConfirmDiscard { repo, entry }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Cmd::DiscardConfirmed(*repo, entry.clone())
                }
                _ => Cmd::Close,
            },
            Some(Overlay::ConfirmGit { repo, args, .. }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Cmd::GitConfirmed(*repo, args.clone())
                }
                _ => Cmd::Close,
            },
            None => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::BackToSettings => self.open_settings_at(Setting::DiffTheme),
            Cmd::ChooseTheme(index) => self.choose_diff_theme(index),
            Cmd::GitConfirmed(repo, args) => {
                self.overlay = None;
                let strs: Vec<&str> = args.iter().map(String::as_str).collect();
                self.run_git(repo, &strs);
            }
            Cmd::DiscardConfirmed(repo, entry) => {
                self.overlay = None;
                let result = match self.repos.get(repo) {
                    Some(r) => r.git.discard(&entry),
                    None => Err("repository is gone".to_string()),
                };
                match result {
                    Ok(()) => self.flash = Some((format!("discarded {}", entry.path), false)),
                    Err(e) => self.flash = Some((e, true)),
                }
                self.refresh();
            }
        }
    }

    fn overlay_mouse(&mut self, mouse: MouseEvent) {
        enum Cmd {
            Nothing,
            Close,
            Activate,
            ToggleSetting(usize),
            BackToSettings,
            ChooseTheme(usize),
            Reopen(u16, u16),
        }
        let row_count = self.settings_rows().len();
        let theme_count = syntax::diff_themes().len();
        let cmd = match self.overlay.as_mut() {
            Some(Overlay::ThemePicker { selected, scroll, rect }) => match mouse.kind {
                MouseEventKind::Moved => {
                    if let Some(index) =
                        option_picker_index(*rect, *scroll, mouse.column, mouse.row, theme_count)
                    {
                        *selected = index;
                    }
                    Cmd::Nothing
                }
                MouseEventKind::ScrollUp => {
                    *selected = selected.saturating_sub(3);
                    Cmd::Nothing
                }
                MouseEventKind::ScrollDown => {
                    *selected = (*selected + 3).min(theme_count.saturating_sub(1));
                    Cmd::Nothing
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    match option_picker_index(
                        *rect,
                        *scroll,
                        mouse.column,
                        mouse.row,
                        theme_count,
                    ) {
                        Some(index) => Cmd::ChooseTheme(index),
                        None if hits(*rect, mouse.column, mouse.row) => Cmd::Nothing,
                        None => Cmd::BackToSettings,
                    }
                }
                _ => Cmd::Nothing,
            },
            Some(Overlay::Settings { selected, rect }) => {
                // Rows start just inside the top border (the title renders ON
                // the border, not on its own line).
                let row_at = |row: u16, col: u16| -> Option<usize> {
                    (col > rect.x
                        && col < rect.x + rect.width.saturating_sub(1)
                        && row > rect.y
                        && row < rect.y + 1 + row_count as u16)
                        .then(|| usize::from(row - rect.y - 1))
                };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(i) = row_at(mouse.row, mouse.column) {
                            *selected = i;
                        }
                        Cmd::Nothing
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        match row_at(mouse.row, mouse.column) {
                            Some(i) => {
                                *selected = i;
                                Cmd::ToggleSetting(i)
                            }
                            None if hits(*rect, mouse.column, mouse.row) => Cmd::Nothing,
                            None => Cmd::Close,
                        }
                    }
                    _ => Cmd::Nothing,
                }
            }
            Some(Overlay::Menu { entries, selected, rect, .. }) => {
                let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
                let item_at = |row: u16, col: u16| -> Option<usize> {
                    (col >= inner.x
                        && col < inner.x + inner.width
                        && row >= inner.y
                        && row < inner.y + inner.height)
                        .then(|| usize::from(row - inner.y))
                        .filter(|i| {
                            *i < entries.len() && matches!(entries[*i], MenuEntry::Action(..))
                        })
                };
                match mouse.kind {
                    MouseEventKind::Moved => {
                        if let Some(i) = item_at(mouse.row, mouse.column) {
                            *selected = i;
                        }
                        Cmd::Nothing
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(i) = item_at(mouse.row, mouse.column) {
                            *selected = i;
                            Cmd::Activate
                        } else {
                            Cmd::Close
                        }
                    }
                    MouseEventKind::Down(MouseButton::Right) => Cmd::Reopen(mouse.column, mouse.row),
                    _ => Cmd::Nothing,
                }
            }
            // The discard confirm is keyboard-driven (y/N); clicks do nothing.
            _ => Cmd::Nothing,
        };
        match cmd {
            Cmd::Nothing => {}
            Cmd::Close => self.overlay = None,
            Cmd::Activate => self.activate_menu_entry(),
            Cmd::ToggleSetting(index) => self.toggle_setting(index),
            Cmd::BackToSettings => self.open_settings_at(Setting::DiffTheme),
            Cmd::ChooseTheme(index) => self.choose_diff_theme(index),
            Cmd::Reopen(x, y) => {
                self.overlay = None;
                self.open_context_menu(x, y);
            }
        }
    }

    // ---- Settings modal ----

    fn open_settings(&mut self) {
        self.overlay = Some(Overlay::Settings { selected: 0, rect: Rect::default() });
    }

    fn open_settings_at(&mut self, setting: Setting) {
        let selected = self
            .settings_rows()
            .iter()
            .position(|row| row.0 == setting)
            .unwrap_or(0);
        self.overlay = Some(Overlay::Settings { selected, rect: Rect::default() });
    }

    fn open_theme_picker(&mut self) {
        let selected = syntax::diff_theme_index(self.sidebar_state.diff_theme);
        self.overlay = Some(Overlay::ThemePicker {
            selected,
            scroll: selected.saturating_sub(5),
            rect: Rect::default(),
        });
    }

    /// The modal's rows for the current state.
    fn settings_rows(&self) -> Vec<SettingRow> {
        vec![
            (
                Setting::UnifiedSidebar,
                "Unified sidebar",
                if self.merged() { "on" } else { "off" }.to_string(),
                self.other_exe.is_some(),
            ),
            (
                Setting::IconTheme,
                "Icon theme",
                match self.theme {
                    IconTheme::Material => "material",
                    IconTheme::Emoji => "emoji",
                }
                .to_string(),
                true,
            ),
            (
                Setting::DiffTheme,
                "Diff theme",
                truncate_to(self.sidebar_state.diff_theme.as_name().to_string(), 15),
                true,
            ),
            (
                Setting::HideUnmodified,
                "Hide unmodified lines",
                if self.sidebar_state.hide_unmodified { "on" } else { "off" }.to_string(),
                true,
            ),
            (
                Setting::ScmView,
                "SCM file view",
                self.sidebar_state.scm_file_view.label().to_string(),
                true,
            ),
            (
                Setting::Hotkeys,
                "Footer hotkeys",
                if self.show_hotkeys() { "shown" } else { "hidden" }.to_string(),
                true,
            ),
            (
                Setting::AutoOpen,
                "Auto-open sidebar",
                if self.sidebar_state.auto_open { "on" } else { "off" }.to_string(),
                true,
            ),
            (
                Setting::Folder,
                "Change folder…",
                self.cwd
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.cwd.display().to_string()),
                true,
            ),
        ]
    }

    fn toggle_setting(&mut self, index: usize) {
        let rows = self.settings_rows();
        let Some(row) = rows.get(index) else { return };
        let (setting, enabled) = (row.0, row.3);
        if !enabled {
            return;
        }
        match setting {
            Setting::UnifiedSidebar => {
                // The pane layout changes underneath the modal; close it.
                self.overlay = None;
                let on = !self.merged();
                self.set_unified(on);
            }
            Setting::IconTheme => self.set_theme(self.theme.toggled()),
            Setting::DiffTheme => self.open_theme_picker(),
            Setting::HideUnmodified => {
                self.sidebar_state.hide_unmodified = !self.sidebar_state.hide_unmodified;
                sidebar::save_state(self.sidebar_state);
            }
            Setting::ScmView => {
                self.sidebar_state.scm_file_view = self.sidebar_state.scm_file_view.toggled();
                sidebar::save_state(self.sidebar_state);
                self.rebuild();
            }
            Setting::Hotkeys => {
                self.sidebar_state.show_hotkeys = !self.sidebar_state.show_hotkeys;
                sidebar::save_state(self.sidebar_state);
            }
            Setting::AutoOpen => {
                self.sidebar_state.auto_open = !self.sidebar_state.auto_open;
                sidebar::save_state(self.sidebar_state);
            }
            Setting::Folder => {
                self.overlay = None;
                self.change_folder_dialog();
            }
        }
    }

    fn choose_diff_theme(&mut self, index: usize) {
        let Some(theme) = syntax::diff_themes().get(index).copied() else { return };
        self.sidebar_state.diff_theme = theme;
        sidebar::save_state(self.sidebar_state);
        self.open_settings_at(Setting::DiffTheme);
    }

    /// The NATIVE folder picker on a background thread (the pane's liveness
    /// heartbeat must keep beating while the dialog is open).
    #[cfg(any(windows, target_os = "macos"))]
    fn change_folder_dialog(&mut self) {
        if self.picking.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let start = self.cwd.clone();
        std::thread::spawn(move || {
            let picked = rfd::FileDialog::new()
                .set_title("Open Folder")
                .set_directory(&start)
                .pick_folder();
            let _ = tx.send(picked);
        });
        self.picking = Some(rx);
        self.flash = Some(("folder picker open… (check your other windows)".into(), false));
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    fn change_folder_dialog(&mut self) {
        self.flash = Some(("no native picker here — use c in the Files view".into(), true));
    }

    /// Collect a finished folder pick, if any (called from the tick loop).
    pub fn poll_picker(&mut self) {
        let Some(rx) = &self.picking else { return };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.picking = None;
                if std::env::set_current_dir(&path).is_ok() {
                    let root = std::env::current_dir().unwrap_or(path);
                    *self = App::new(root);
                } else {
                    self.flash = Some((format!("cannot open {}", path.display()), true));
                }
            }
            Ok(None) => {
                self.picking = None;
                self.flash = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(_) => self.picking = None,
        }
    }

    /// Render the centered Settings popup and remember its rect for clicks.
    fn draw_settings(&mut self, frame: &mut Frame) {
        let rows = self.settings_rows();
        // The hotkey reference lives here now; the footer chips are opt-in.
        let hint_lines = wrap_hints(&self.hints(), 28, 0);
        let Some(Overlay::Settings { selected, rect }) = self.overlay.as_mut() else {
            return;
        };
        let area = Rect::new(0, 0, self.last_width, self.last_height);
        let width = 30.min(area.width);
        let height =
            (rows.len() as u16 + 5 + hint_lines.len() as u16).min(area.height);
        let popup = Rect::new(
            (area.width.saturating_sub(width)) / 2,
            (area.height.saturating_sub(height)) / 3,
            width,
            height,
        );
        *rect = popup;

        let inner_w = usize::from(width.saturating_sub(2));
        let mut lines: Vec<Line> = Vec::new();
        for (i, (_, label, value, enabled)) in rows.iter().enumerate() {
            let pad = inner_w.saturating_sub(label.chars().count() + value.chars().count() + 2);
            let text = format!(" {label}{}{value} ", " ".repeat(pad.max(1)));
            let style = if !enabled {
                Style::default().dim()
            } else if i == *selected {
                Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::styled(text, style));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(" Hotkeys", Style::default().bold())));
        lines.extend(hint_lines);
        lines.push(Line::from(" click/⏎ toggle · esc close".dim()));

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .title(" Settings ")
                    .border_style(Style::default().dim()),
            ),
            popup,
        );
    }

    fn draw_theme_picker(&mut self, frame: &mut Frame) {
        let options = syntax::diff_themes().iter().map(|theme| theme.as_name()).collect::<Vec<_>>();
        let Some(Overlay::ThemePicker { selected, scroll, rect }) = self.overlay.as_mut() else {
            return;
        };
        let area = Rect::new(0, 0, self.last_width, self.last_height);
        *rect = draw_option_picker(frame, area, "Diff theme", &options, *selected, scroll);
    }

    fn activate_menu_entry(&mut self) {
        let Some(Overlay::Menu { target, entries, selected, .. }) = self.overlay.take() else {
            return;
        };
        let MenuEntry::Action(action, _) = entries[selected] else { return };
        match target {
            MenuTarget::File { repo, entry, staged } => {
                self.file_menu_action(action, repo, entry, staged)
            }
            MenuTarget::Drawer { kind, index } => self.drawer_menu_action(action, kind, index),
            MenuTarget::Directory { repo, path, staged } => match action {
                MenuAction::StageOrUnstage => self.run_directory(repo, path, staged),
                MenuAction::CopyRelativePath => self.copy_relative_path(&path),
                _ => {}
            },
            MenuTarget::HistoryFile { repo, file } => match action {
                MenuAction::OpenDiff => self.open_ref_diff(repo, &file),
                MenuAction::CopyRelativePath => self.copy_relative_path(&file.entry.path),
                _ => {}
            },
        }
    }

    fn copy_relative_path(&mut self, path: &str) {
        let text = path.replace('/', std::path::MAIN_SEPARATOR_STR);
        self.flash = Some(match copy_to_clipboard(&text) {
            Ok(()) => (format!("copied: {text}"), false),
            Err(err) => (format!("copy failed: {err}"), true),
        });
    }

    fn file_menu_action(&mut self, action: MenuAction, repo: usize, entry: FileEntry, staged: bool) {
        let repo_root = self.repos.get(repo).map(|r| r.git.root().to_path_buf());
        match action {
            MenuAction::StageOrUnstage => {
                let result = match self.repos.get(repo) {
                    Some(r) if staged => r.git.unstage(&entry),
                    Some(r) => r.git.stage(&entry),
                    None => Err("repository is gone".to_string()),
                };
                if let Err(e) = result {
                    self.flash = Some((e, true));
                }
                self.refresh();
            }
            MenuAction::OpenDiff => self.open_diff(repo, &entry, staged),
            MenuAction::Discard => self.overlay = Some(Overlay::ConfirmDiscard { repo, entry }),
            MenuAction::CopyPath | MenuAction::CopyRelativePath => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let text = if action == MenuAction::CopyPath {
                    repo_root.unwrap_or_else(|| self.cwd.clone()).join(&rel).display().to_string()
                } else {
                    rel
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(()) => (format!("copied: {text}"), false),
                    Err(err) => (format!("copy failed: {err}"), true),
                });
            }
            MenuAction::Reveal => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let path = repo_root.unwrap_or_else(|| self.cwd.clone()).join(rel);
                reveal(&path);
            }
            MenuAction::OpenExternal => {
                let rel = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
                let path = repo_root.unwrap_or_else(|| self.cwd.clone()).join(&rel);
                self.flash = Some(match open_external(&path) {
                    Ok(()) => (format!("opened: {rel}"), false),
                    Err(err) => (format!("open failed: {err}"), true),
                });
            }
            _ => {}
        }
    }

    fn drawer_menu_action(&mut self, action: MenuAction, kind: Drawer, index: usize) {
        let Some(dref) = self.drawers[kind.index()].refs.get(index).cloned() else { return };
        let repo = self.active;
        let spec = match &dref {
            DrawerRef::Commit(h) => h.clone(),
            DrawerRef::Stash(n) => format!("stash@{{{n}}}"),
            DrawerRef::Branch { name, .. } => name.clone(),
            DrawerRef::Remote { name, .. } => name.clone(),
            DrawerRef::Tag(t) => t.clone(),
            DrawerRef::Worktree(p) => p.clone(),
            DrawerRef::None => return,
        };
        match action {
            MenuAction::OpenDiff if kind == Drawer::FileHistory => {
                if let Some(file) = self.drawers[kind.index()].files.get(index).and_then(Clone::clone)
                {
                    self.open_ref_diff(repo, &file);
                }
            }
            MenuAction::ShowRef => self.open_drawer_ref(kind, index),
            MenuAction::Reveal => reveal(std::path::Path::new(&spec)),
            MenuAction::RemoveWorktree => self.confirm_git(
                repo,
                format!("Remove worktree '{spec}'? (y/N)"),
                vec!["worktree".into(), "remove".into(), spec],
            ),
            MenuAction::CopyRef => {
                let text = match &dref {
                    DrawerRef::Remote { url, .. } if !url.is_empty() => url.clone(),
                    _ => spec,
                };
                self.flash = Some(match copy_to_clipboard(&text) {
                    Ok(()) => (format!("copied: {text}"), false),
                    Err(err) => (format!("copy failed: {err}"), true),
                });
            }
            MenuAction::Checkout => self.run_git(repo, &["checkout", &spec]),
            MenuAction::MergeInto => self.run_git(repo, &["merge", "--no-edit", &spec]),
            MenuAction::CherryPick => self.run_git(repo, &["cherry-pick", &spec]),
            MenuAction::Revert => self.run_git(repo, &["revert", "--no-edit", &spec]),
            MenuAction::StashApply => self.run_git(repo, &["stash", "apply", &spec]),
            MenuAction::StashPop => self.run_git(repo, &["stash", "pop", &spec]),
            MenuAction::FetchRemote => self.run_git(repo, &["fetch", &spec]),
            MenuAction::ResetHere => self.confirm_git(
                repo,
                format!("Reset current branch to {spec} (mixed)? (y/N)"),
                vec!["reset".into(), "--mixed".into(), spec],
            ),
            MenuAction::DeleteBranch => self.confirm_git(
                repo,
                format!("Delete branch '{spec}'? (y/N)"),
                vec!["branch".into(), "-D".into(), spec],
            ),
            MenuAction::StashDrop => self.confirm_git(
                repo,
                format!("Drop {spec}? (y/N)"),
                vec!["stash".into(), "drop".into(), spec],
            ),
            MenuAction::DeleteTag => self.confirm_git(
                repo,
                format!("Delete tag '{spec}'? (y/N)"),
                vec!["tag".into(), "-d".into(), spec],
            ),
            _ => {}
        }
    }

    /// Run a git op for `repo`, flash the outcome, refresh everything (merge
    /// conflicts and the like surface as the flashed git error).
    fn run_git(&mut self, repo: usize, args: &[&str]) {
        let result = match self.repos.get(repo) {
            Some(r) => r.git.raw(args),
            None => Err("repository is gone".to_string()),
        };
        match result {
            Ok(_) => self.flash = Some((format!("git {} ✓", args.join(" ")), false)),
            Err(e) => self.flash = Some((e, true)),
        }
        self.refresh();
    }

    fn confirm_git(&mut self, repo: usize, prompt: String, args: Vec<String>) {
        self.overlay = Some(Overlay::ConfirmGit { repo, prompt, args });
    }

    /// Graph keeps its raw `git show`; modern history drawers expand a file
    /// tree, and File History opens the selected historical file directly.
    fn open_drawer_ref(&mut self, kind: Drawer, index: usize) {
        if kind == Drawer::FileHistory {
            if let Some(file) = self.drawers[kind.index()].files.get(index).and_then(Clone::clone) {
                self.open_ref_diff(self.active, &file);
            }
            return;
        }
        if matches!(kind, Drawer::Commits | Drawer::Branches | Drawer::Stashes | Drawer::Tags) {
            self.toggle_expanded_ref(kind, index);
            return;
        }
        if kind != Drawer::Graph {
            return;
        }
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.flash = Some(("preview needs a herdr pane".into(), true));
            return;
        };
        let Some(repo) = self.repos.get(self.active) else { return };
        let spec = match self.drawers[kind.index()].refs.get(index) {
            Some(DrawerRef::Commit(h)) => h.clone(),
            _ => return,
        };
        let payload = herdr_sidebar::viewer::show_request(repo.git.root(), &spec, None);
        if let Err(e) =
            herdr_sidebar::viewer::open_in_pane(&pane_id, repo.git.root(), &payload)
        {
            self.flash = Some((e, true));
        }
    }

    fn toggle_expanded_ref(&mut self, kind: Drawer, index: usize) {
        let Some(target) = self.drawers[kind.index()].refs.get(index).cloned() else { return };
        if self
            .expanded_ref
            .as_ref()
            .is_some_and(|expanded| expanded.kind == kind && expanded.target == target)
        {
            self.expanded_ref = None;
            self.rebuild();
            self.select_drawer_line(kind, index);
            return;
        }
        let Some(repo) = self.active_repo() else { return };
        let result = match &target {
            DrawerRef::Commit(spec) | DrawerRef::Tag(spec) => repo.git.ref_files(spec),
            DrawerRef::Branch { name, .. } => repo.git.ref_files(name),
            DrawerRef::Stash(index) => repo.git.stash_files(&format!("stash@{{{index}}}")),
            _ => return,
        };
        let (files, error) = match result {
            Ok(files) => (files, None),
            Err(_) if kind == Drawer::Tags => {
                (Vec::new(), Some("tag does not point to a commit".to_string()))
            }
            Err(error) => (Vec::new(), Some(error)),
        };
        let mut expanded = ExpandedRef::new(kind, target, files, error);
        expanded.rebuild_rows(self.sidebar_state.scm_file_view);
        self.expanded_ref = Some(expanded);
        self.rebuild();
        self.select_drawer_line(kind, index);
    }

    fn select_drawer_line(&mut self, kind: Drawer, index: usize) {
        if let Some(row) = self
            .rows
            .iter()
            .position(|row| matches!(row, Row::DrawerLine(k, i) if *k == kind && *i == index))
        {
            self.select(row);
        }
    }

    fn open_history_file(&mut self, index: usize) {
        let Some(file) = self
            .expanded_ref
            .as_ref()
            .and_then(|expanded| expanded.files.get(index))
            .cloned()
        else {
            return;
        };
        self.open_ref_diff(self.active, &file);
    }

    fn open_ref_diff(&mut self, repo: usize, file: &RefFile) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            self.flash = Some(("diff preview needs a herdr pane".into(), true));
            return;
        };
        let Some(repo) = self.repos.get(repo) else { return };
        let payload = herdr_sidebar::viewer::ref_diff_request(
            repo.git.root(),
            &file.old_spec,
            &file.new_spec,
            &file.entry.path,
            file.entry.orig.as_deref(),
        );
        if let Err(e) = herdr_sidebar::viewer::open_in_pane(&pane_id, repo.git.root(), &payload) {
            self.flash = Some((e, true));
        }
    }

    /// Show a file's diff in the preview pane beside the sidebar. Staged
    /// rows show the staged diff; untracked files render as one addition.
    fn open_diff(&mut self, repo: usize, entry: &FileEntry, staged: bool) {
        let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) else {
            self.flash = Some(("diff preview needs a herdr pane".into(), true));
            return;
        };
        let Some(repo) = self.repos.get(repo) else { return };
        let kind = if staged {
            "staged"
        } else if entry.letter == 'U' {
            "untracked"
        } else {
            "worktree"
        };
        let payload =
            herdr_sidebar::viewer::diff_request(repo.git.root(), &entry.path, kind);
        if let Err(e) =
            herdr_sidebar::viewer::open_in_pane(&pane_id, repo.git.root(), &payload)
        {
            self.flash = Some((e, true));
        }
    }

    /// `o`: open the diff for the currently selected file row.
    fn open_selected_diff(&mut self) {
        let Some(&row) = self.selected.and_then(|i| self.rows.get(i)) else {
            return;
        };
        match row {
            Row::Staged(r, i) => {
                if let Some(entry) = self.status_entry(r, i, true).cloned() {
                    self.open_diff(r, &entry, true);
                }
            }
            Row::Unstaged(r, i) => {
                if let Some(entry) = self.status_entry(r, i, false).cloned() {
                    self.open_diff(r, &entry, false);
                }
            }
            Row::HistoryTree(i) => {
                if let Some(ChangeTreeRow::File { index, .. }) = self
                    .expanded_ref
                    .as_ref()
                    .and_then(|expanded| expanded.rows.get(i))
                {
                    self.open_history_file(*index);
                }
            }
            _ => {}
        }
    }

    // ---- Unified-sidebar operations ----

    /// Toggle the unified sidebar. On: adopt this pane as the Sidebar and
    /// close the other panel's standalone pane in this tab. Off: split the
    /// other view back out into its own pane. Deliberately silent — the
    /// layout change is its own feedback.
    fn set_unified(&mut self, on: bool) {
        if on == self.merged() || self.other_exe.is_none() {
            return;
        }
        self.sidebar_state =
            sidebar::State { merged: on, active: MY_VIEW, ..self.sidebar_state };
        sidebar::save_state(self.sidebar_state);
        self.apply_identity();
        if on {
            // Mirror the detach growth: absorbing the sibling leaves the
            // survivor at roughly double width — shrink back to one panel.
            let width = self.last_width;
            self.close_other_standalone_pane();
            if let Some(ctl) = &self.pane_ctl {
                ctl.resize_to(width.saturating_mul(2).saturating_add(1), width);
            }
        } else {
            self.spawn_other_pane();
        }
    }

    /// Hand the pane to the other view (the supervisor swaps processes).
    fn switch_to(&mut self, view: View) -> Option<Exit> {
        if !self.merged() || view == MY_VIEW {
            return None;
        }
        self.sidebar_state.active = view;
        sidebar::save_state(self.sidebar_state);
        Some(Exit::Switch)
    }

    /// Close the other panel's standalone pane in our tab, if one is open.
    fn close_other_standalone_pane(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        let Ok(json) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) else {
            return;
        };
        for id in sibling_panes_of(&json, &ctl.pane_id, MY_VIEW.other()) {
            let _ = herdr_sidebar::ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
        }
    }

    /// Open the other view in a fresh pane beside this one (detach).
    fn spawn_other_pane(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        // Grow to double width FIRST, then split 50/50 — each separated panel
        // keeps the width the unified sidebar had, instead of halving.
        ctl.resize_to(self.last_width, self.last_width.saturating_mul(2).saturating_add(1));
        #[cfg(not(windows))]
        {
            let _ = herdr_sidebar::ipc::call_text(
                "plugin.pane.open",
                serde_json::json!({
                    "plugin_id": "herdr-sidebar",
                    "entrypoint": "explorer",
                    "placement": "split",
                    "target_pane_id": ctl.pane_id,
                    "direction": "right",
                    "focus": false,
                    "cwd": self.cwd.display().to_string(),
                    "env": sidebar::spawn_env(),
                }),
            );
        }
        #[cfg(windows)]
        {
            let Some(exe) = &self.other_exe else { return };
            let response = herdr_sidebar::ipc::call_text(
                "pane.split",
                serde_json::json!({
                    "target_pane_id": ctl.pane_id,
                    "direction": "right",
                    "ratio": 0.5,
                    "focus": false,
                    "cwd": self.cwd.display().to_string(),
                    "env": sidebar::spawn_env(),
                }),
            );
            let Some(new_pane) =
                response.ok().and_then(|r| herdr_sidebar::launch::split_pane_id(&r))
            else {
                return;
            };
            let flag = MY_VIEW.other().view_flag();
            let command = format!("& \"{}\" --view {flag}", exe.display());
            let _ = herdr_sidebar::ipc::call_text(
                "pane.send_input",
                serde_json::json!({ "pane_id": new_pane, "text": command, "keys": ["Enter"] }),
            );
            let _ = herdr_sidebar::ipc::call_text(
                "pane.rename",
                serde_json::json!({ "pane_id": new_pane, "label": MY_VIEW.other().label() }),
            );
        }
    }

    // ---- Git operations ----

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.selected = Some(index.min(self.rows.len() - 1));
            self.snap = true;
            self.follow_selection();
        }
    }

    /// Wheel: move the VIEW only — the selection stays where it is.
    fn scroll_view(&mut self, delta: isize) {
        let max = self.rows.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        // First keyboard step on a selection-less list picks the first stop.
        let Some(sel) = self.selected else {
            let first = self.nearest_selectable(0);
            self.select(first);
            return;
        };
        let len = self.rows.len() as isize;
        let current = sel as isize;
        let step = if delta >= 0 { 1 } else { -1 };
        let mut next = (current + delta).clamp(0, len - 1);
        // Widget rows aren't keyboard stops: keep going in the same direction,
        // falling back to the nearest stop at the ends.
        while (0..len).contains(&next) && !self.rows[next as usize].selectable() {
            next += step;
        }
        let next = if (0..len).contains(&next) {
            next as usize
        } else {
            self.nearest_selectable((current + delta).clamp(0, len - 1) as usize)
        };
        self.select(next);
    }

    /// Enter/Space on the selected row: toggle a section/drawer, or move a
    /// file between the staged and unstaged lists.
    fn activate(&mut self) {
        let Some(&row) = self.selected.and_then(|i| self.rows.get(i)) else {
            return;
        };
        match row {
            // Widget rows aren't keyboard-selectable; nothing to activate.
            Row::Message(_) | Row::Commit(_) | Row::RepoSeparator => {}
            Row::DrawerLine(kind, i) => self.open_drawer_ref(kind, i),
            Row::HistoryTree(i) => match self.expanded_ref.as_ref().and_then(|expanded| expanded.rows.get(i)) {
                Some(ChangeTreeRow::Directory { .. }) => self.toggle_history_directory(i),
                Some(ChangeTreeRow::File { index, .. }) => self.open_history_file(*index),
                None => {}
            },
            Row::HistoryNotice => {}
            Row::RepoHeader(r) => self.toggle_repo(r),
            Row::StagedHeader(r) => {
                self.repos[r].staged_collapsed = !self.repos[r].staged_collapsed;
                self.rebuild();
            }
            Row::ChangesHeader(r) => {
                self.repos[r].changes_collapsed = !self.repos[r].changes_collapsed;
                self.rebuild();
            }
            Row::DrawerHeader(kind) => {
                self.drawers[kind.index()].expanded = !self.drawers[kind.index()].expanded;
                self.reload_expanded_drawers();
                self.rebuild();
            }
            Row::Staged(r, i) => {
                if self.status_directory(r, i, true).is_some() {
                    self.toggle_status_directory(r, i, true);
                } else {
                    self.run_op(|git, e| git.unstage(e), r, i, true);
                }
            }
            Row::Unstaged(r, i) => {
                if self.status_directory(r, i, false).is_some() {
                    self.toggle_status_directory(r, i, false);
                } else {
                    self.run_op(|git, e| git.stage(e), r, i, false);
                }
            }
        }
    }

    fn toggle_repo(&mut self, index: usize) {
        let Some(repo) = self.repos.get_mut(index) else { return };
        repo.collapsed = !repo.collapsed;
        self.rebuild();
    }

    fn run_op(
        &mut self,
        op: impl Fn(&Git, &FileEntry) -> Result<(), String>,
        repo: usize,
        index: usize,
        staged: bool,
    ) {
        let Some(entry) = self.status_entry(repo, index, staged).cloned() else { return };
        let Some(repo) = self.repos.get(repo) else { return };
        if let Err(e) = op(&repo.git, &entry) {
            self.flash = Some((e, true));
        }
        self.refresh();
    }

    fn run_directory(&mut self, repo_index: usize, path: String, staged: bool) {
        let Some(repo) = self.repos.get(repo_index) else { return };
        let entries = if staged { &repo.status.staged } else { &repo.status.unstaged };
        let pathspecs = directory_pathspecs(&path, entries);
        let pathspecs = pathspecs.iter().map(String::as_str).collect::<Vec<_>>();
        let result = if staged {
            repo.git.unstage_paths(&pathspecs)
        } else {
            repo.git.stage_paths(&pathspecs)
        };
        if let Err(e) = result {
            self.flash = Some((e, true));
        }
        self.refresh();
        self.select_nearest_status_row(repo_index, staged, &path);
    }

    fn select_nearest_status_row(&mut self, repo: usize, staged: bool, preferred: &str) {
        let header = self.rows.iter().position(|row| {
            matches!(
                (staged, row),
                (true, Row::StagedHeader(r)) | (false, Row::ChangesHeader(r)) if *r == repo
            )
        }).or_else(|| {
            staged
                .then(|| {
                    self.rows
                        .iter()
                        .position(|row| matches!(row, Row::ChangesHeader(r) if *r == repo))
                })
                .flatten()
        });
        let preferred = preferred.to_lowercase();
        let mut candidates = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match *row {
                Row::Staged(r, i) if staged && r == repo => self
                    .status_row_path(r, i, true)
                    .map(|path| (index, path.to_string())),
                Row::Unstaged(r, i) if !staged && r == repo => self
                    .status_row_path(r, i, false)
                    .map(|path| (index, path.to_string())),
                _ => None,
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|candidate| candidate.1.to_lowercase());
        let selected = candidates
            .iter()
            .find(|(_, path)| path.to_lowercase().as_str() >= preferred.as_str())
            .or_else(|| candidates.last())
            .map(|(index, _)| *index)
            .or(header);
        if let Some(index) = selected {
            self.select(index);
        }
    }

    fn select_status_header(&mut self, repo: usize, staged: bool) {
        let selected = status_header_index(&self.rows, repo, staged)
            .or_else(|| status_header_index(&self.rows, repo, false));
        if let Some(index) = selected {
            self.select(index);
        }
    }

    fn toggle_status_directory(&mut self, repo: usize, row: usize, staged: bool) {
        let was_expanded = self
            .status_tree_row(repo, row, staged)
            .and_then(ChangeTreeRow::expanded)
            .unwrap_or(false);
        let Some(path) = self.status_directory(repo, row, staged).map(str::to_string) else {
            return;
        };
        let Some(repo) = self.repos.get_mut(repo) else { return };
        let collapsed = if staged { &mut repo.staged_dirs } else { &mut repo.changes_dirs };
        toggle_collapsed_path(collapsed, &path, was_expanded);
        self.rebuild();
    }

    fn toggle_history_directory(&mut self, row: usize) {
        let Some(expanded) = &mut self.expanded_ref else { return };
        let Some(ChangeTreeRow::Directory { path, expanded: was_expanded, .. }) = expanded.rows.get(row) else {
            return;
        };
        let path = path.clone();
        let was_expanded = *was_expanded;
        toggle_collapsed_path(&mut expanded.collapsed, &path, was_expanded);
        self.rebuild();
    }

    fn tree_nav(&mut self, expand: bool) {
        let Some(selected) = self.selected else { return };
        let Some(row) = self.rows.get(selected).copied() else { return };
        match row {
            Row::Staged(repo, index) => self.status_tree_nav(selected, repo, index, true, expand),
            Row::Unstaged(repo, index) => {
                self.status_tree_nav(selected, repo, index, false, expand)
            }
            Row::HistoryTree(index) => self.history_tree_nav(selected, index, expand),
            _ => {}
        }
    }

    fn status_tree_nav(
        &mut self,
        selected: usize,
        repo: usize,
        index: usize,
        staged: bool,
        expand: bool,
    ) {
        let Some(row) = self.status_tree_row(repo, index, staged).cloned() else { return };
        let rows = if staged { &self.repos[repo].staged_rows } else { &self.repos[repo].changes_rows };
        if let Some(target) = tree_nav_target(&row, rows, index, expand) {
            let wanted =
                if staged { Row::Staged(repo, target) } else { Row::Unstaged(repo, target) };
            if let Some(global) = self.rows.iter().position(|row| same_row(*row, wanted)) {
                self.select(global);
            } else {
                self.select(selected);
            }
        } else if matches!(row, ChangeTreeRow::Directory { .. }) {
            let is_expanded = row.expanded().unwrap_or(false);
            if is_expanded != expand {
                self.toggle_status_directory(repo, index, staged);
            }
        }
    }

    fn history_tree_nav(&mut self, selected: usize, index: usize, expand: bool) {
        let Some(expanded) = &self.expanded_ref else { return };
        let Some(row) = expanded.rows.get(index).cloned() else { return };
        let target = tree_nav_target(&row, &expanded.rows, index, expand);
        if let Some(target) = target {
            if let Some(global) = self
                .rows
                .iter()
                .position(|row| matches!(row, Row::HistoryTree(i) if *i == target))
            {
                self.select(global);
            }
        } else if matches!(row, ChangeTreeRow::Directory { .. })
            && row.expanded().unwrap_or(false) != expand
        {
            self.toggle_history_directory(index);
        } else {
            self.select(selected);
        }
    }

    fn stage_all(&mut self) {
        let active = self.active;
        let Some(repo) = self.active_repo() else { return };
        if let Err(e) = repo.git.stage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
        self.select_status_header(active, true);
    }

    fn unstage_all(&mut self) {
        let active = self.active;
        let Some(repo) = self.active_repo() else { return };
        if let Err(e) = repo.git.unstage_all() {
            self.flash = Some((e, true));
        }
        self.refresh();
        self.select_status_header(active, false);
    }

    /// Kick off ✧ commit-message generation in the background.
    fn suggest_message(&mut self) {
        if self.suggesting.is_some() {
            return;
        }
        let Some(repo) = self.active_repo() else { return };
        match repo.git.diff_for_message() {
            Ok((diff, files)) if diff.trim().is_empty() && files.is_empty() => {
                self.flash = Some(("no changes to describe".into(), true));
            }
            Ok((diff, files)) => {
                self.suggesting = Some(suggest::spawn(diff, files));
                self.flash = Some(("✧ generating commit message…".into(), false));
            }
            Err(e) => self.flash = Some((e, true)),
        }
    }

    /// VS Code's Sync Changes (pull --rebase, then push) on a background
    /// thread; tick() collects the outcome.
    fn sync_changes(&mut self) {
        self.sync_repo(self.active);
    }

    fn sync_repo(&mut self, index: usize) {
        if self.syncing.is_some() {
            return;
        }
        let Some(repo) = self.repos.get(index) else { return };
        if !repo.status.has_upstream {
            self.flash = Some(("no upstream to sync with".into(), true));
            return;
        }
        let git = repo.git.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(git.sync());
        });
        self.syncing = Some(rx);
    }

    fn commit(&mut self) {
        self.commit_repo(self.active);
    }

    fn commit_repo(&mut self, index: usize) {
        let Some(repo) = self.repos.get_mut(index) else { return };
        let message: String = repo.message.iter().collect();
        if message.trim().is_empty() {
            self.active = index;
            self.flash = Some(("Commit message is empty.".to_string(), true));
            self.focus = Focus::Message;
            return;
        }
        if repo.status.staged.is_empty() {
            self.flash = Some(("No staged changes to commit.".to_string(), true));
            return;
        }
        match repo.git.commit(message.trim()) {
            Ok(summary) => {
                self.flash = Some((summary, false));
                repo.message.clear();
                repo.cursor = 0;
                self.focus = Focus::List;
            }
            Err(e) => self.flash = Some((e, true)),
        }
        self.refresh();
    }

    /// Screen lines a row occupies; the inline message boxes grow with
    /// their (wrapped) message, up to [`MESSAGE_MAX_ROWS`] content rows.
    fn row_height(&self, row: Row) -> u16 {
        match row {
            Row::Message(r) => 2 + self.message_rows_inline(r) as u16,
            _ => 1,
        }
    }

    /// Content rows repo `r`'s inline message box shows right now.
    fn message_rows_inline(&self, r: usize) -> usize {
        let Some(repo) = self.repos.get(r) else { return 1 };
        let field = usize::from(inline_field_width(self.last_width));
        wrap_message(&repo.message, repo.cursor, field).0.len().min(MESSAGE_MAX_ROWS)
    }

    /// Content rows the single-repo message box shows at `width`.
    fn single_message_rows(&self, width: u16) -> usize {
        let sparkle_w = Span::raw(sparkle_icon(self.theme)).width() + 1;
        let field = usize::from(width).saturating_sub(2 + sparkle_w).max(1);
        match self.active_repo() {
            Some(r) => wrap_message(&r.message, r.cursor, field).0.len().min(MESSAGE_MAX_ROWS),
            None => 1,
        }
    }

    /// The visible row at a pane-local mouse row plus the line within it
    /// (rows vary in height: message boxes and buttons span several lines).
    fn row_hit(&self, mouse_row: u16) -> Option<(usize, u16)> {
        if mouse_row < self.body.top || mouse_row >= self.body.top + self.body.height {
            return None;
        }
        let mut y = self.body.top;
        for index in self.body.offset..self.rows.len() {
            let h = self.row_height(self.rows[index]);
            if mouse_row < y + h {
                return Some((index, mouse_row - y));
            }
            y += h;
        }
        None
    }

    /// The visible row index at a pane-local mouse row, if it lands on one.
    fn row_at(&self, mouse_row: u16) -> Option<usize> {
        self.row_hit(mouse_row).map(|(index, _)| index)
    }

    /// The screen row where `index`'s first line is drawn, if visible.
    fn row_y(&self, index: usize) -> Option<u16> {
        let mut y = self.body.top;
        for i in self.body.offset..self.rows.len() {
            if i == index {
                return (y < self.body.top + self.body.height).then_some(y);
            }
            y += self.row_height(self.rows[i]);
        }
        None
    }

    // ---- Rendering ----

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.draw_in(frame, area);
    }

    pub fn draw_in(&mut self, frame: &mut Frame, area: Rect) {
        self.last_width = area.width;
        self.last_height = area.height;

        if self.repos.is_empty() {
            let text = format!(
                "Not a git repository.\n\n{}\n\nOpen this pane inside a repo,\nor press q to quit.",
                self.discover_err,
            );
            frame.render_widget(Paragraph::new(text).dim().wrap(Wrap { trim: false }), area);
            return;
        }

        // With several repos, VS Code puts a message box + Commit button
        // INSIDE each repo's section (rendered as list rows); the single-repo
        // view keeps them fixed at the top. The Sync Changes row only appears
        // when there is something to sync (or a sync is running).
        let multi = self.multi();
        let show_repo_controls = !multi && !self.repos[0].collapsed;
        let message_height = if show_repo_controls {
            2 + self.single_message_rows(area.width) as u16
        } else {
            0
        };
        let button_height = u16::from(show_repo_controls);
        let sync_height = u16::from(show_repo_controls && self.sync_label().is_some());
        let footer_lines = self.footer_lines(area.width);
        // A breathing row above and below the icons keeps the activity bar
        // from crowding the pane border.
        let activity_height = if self.merged() { 3 } else { 0 };
        let [activity, header, message, button, sync, list, footer] = Layout::vertical([
            Constraint::Length(activity_height),
            Constraint::Length(1),
            Constraint::Length(message_height),
            Constraint::Length(button_height),
            Constraint::Length(sync_height),
            Constraint::Min(0),
            Constraint::Length((footer_lines.len() as u16).max(1)),
        ])
        .areas(area);
        self.page = list.height.saturating_sub(1).max(1) as usize;

        if self.merged() {
            self.draw_activity_bar(frame, activity);
        }
        self.draw_header(frame, header);
        if show_repo_controls {
            self.draw_message(frame, message);
            self.draw_button(frame, button);
            self.draw_sync(frame, sync);
        } else {
            self.zones.message = Rect::default();
            self.zones.sparkle = Rect::default();
            self.zones.button = Rect::default();
            self.zones.sync = Rect::default();
        }
        self.draw_list(frame, list);
        frame.render_widget(Paragraph::new(footer_lines), footer);
        // Collapse button at the bottom-right of the last footer line,
        // mirroring the explorer (and herdr's own sidebar).
        let last_line = Rect::new(
            footer.x,
            footer.y + footer.height.saturating_sub(1),
            footer.width,
            1,
        );
        let [_, footer_button] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(3)]).areas(last_line);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "«",
                Style::default().bold().fg(Color::LightBlue),
            ))
            .centered(),
            footer_button,
        );

        match self.overlay {
            Some(Overlay::Menu { .. }) => self.draw_menu(frame),
            Some(Overlay::Settings { .. }) => self.draw_settings(frame),
            Some(Overlay::ThemePicker { .. }) => self.draw_theme_picker(frame),
            _ => {}
        }
    }

    /// The VS Code activity bar: view-switcher icons plus a detach button.
    /// The area is three rows tall — icons on the middle one, one blank
    /// spacer row each side.
    fn draw_activity_bar(&mut self, frame: &mut Frame, area: Rect) {
        // Three rows in the plain pane background; only the ACTIVE icon's
        // highlight chip extends into the outer rows by a half block — a tall
        // button with built-in breathing room, no strip container.
        let outer_top = area.y;
        let outer_bottom = area.y + 2;
        let area = Rect::new(area.x, area.y + 1, area.width, 1);
        let (exp_icon, git_icon, search_icon) = activity_icons(self.theme);
        // Both FA glyphs (folder, code-fork) render two cells wide in the
        // non-Mono Nerd Font; reserve the second cell in each chip so the
        // highlights are equal-sized with centered icons.
        let slack = if self.theme == IconTheme::Material { " " } else { "" };
        let exp_chip = format!(" {exp_icon}{slack} ");
        let git_chip = format!(" {git_icon}{slack} ");
        let search_chip = format!(" {search_icon}{slack} ");
        let exp_start = area.x + 1;
        let exp_end = exp_start + Span::raw(exp_chip.as_str()).width() as u16;
        let git_start = exp_end + 1;
        let git_end = git_start + Span::raw(git_chip.as_str()).width() as u16;
        let search_start = git_end + 1;
        let search_end = search_start + Span::raw(search_chip.as_str()).width() as u16;
        let hovered = |(start, end): (u16, u16)| {
            self.mouse_pos
                .is_some_and(|(x, y)| y == area.y && (start..end).contains(&x))
        };
        let spans = [
            Span::raw(" "),
            Span::styled(
                exp_chip,
                activity_button_style(false, hovered((exp_start, exp_end))),
            ),
            Span::raw(" "),
            Span::styled(
                git_chip,
                activity_button_style(true, hovered((git_start, git_end))),
            ),
            Span::raw(" "),
            Span::styled(
                search_chip,
                activity_button_style(false, hovered((search_start, search_end))),
            ),
        ];
        self.zones.activity_row = area.y;
        self.zones.explorer = (exp_start, exp_end);
        self.zones.source_control = (git_start, git_end);
        self.zones.search = (search_start, search_end);
        // Symmetric half-block caps: a 2-cell button with the icon in its
        // vertical center.
        let (chip_start, chip_end) = self.zones.source_control;
        let chip_w = chip_end.saturating_sub(chip_start);
        let cap = |glyph: &str| {
            Paragraph::new(glyph.repeat(usize::from(chip_w)))
                .style(Style::default().fg(Color::DarkGray))
        };
        frame.render_widget(cap("▄"), Rect::new(chip_start, outer_top, chip_w, 1));
        frame.render_widget(cap("▀"), Rect::new(chip_start, outer_bottom, chip_w, 1));
        let gear_chip = format!(" {} ", gear_icon(self.theme));
        let gear_w = Span::raw(gear_chip.as_str()).width() as u16;
        let gear_x = area.x + area.width.saturating_sub(gear_w);
        self.zones.gear = Rect::new(gear_x, area.y, gear_w, 1);
        let gear = Span::styled(
            gear_chip,
            icon_button_style(
                self.mouse_pos
                    .is_some_and(|(x, y)| hits(self.zones.gear, x, y)),
                true,
            ),
        );
        let redraw_x = gear_x.saturating_sub(3);
        let (redraw, redraw_rect) =
            redraw_button(self.theme, redraw_x, area.y, self.mouse_pos);
        self.zones.redraw = redraw_rect;

        let pad = usize::from(area.width)
            .saturating_sub(
                spans.iter().map(Span::width).sum::<usize>() + 3 + usize::from(gear_w),
            );
        let mut line = spans.to_vec();
        line.push(Span::raw(" ".repeat(pad)));
        line.push(redraw);
        line.push(gear);
        frame.render_widget(Paragraph::new(Line::from(line)), area);
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        // A single repo titles the panel with its NAME, like VS Code's repo
        // rows (the sections inside are already Changes/Staged Changes);
        // several repos fall back to a neutral "Source Control".
        let title = match self.active_repo() {
            Some(repo) if self.repos.len() == 1 => repo.name.clone(),
            _ => "Source Control".to_string(),
        };
        // With several repos visible, the header names the one the commit box
        // and sync act on; a single repo shows branch + ahead/behind arrows.
        let right_text = match self.active_repo() {
            Some(repo) if self.repos.len() > 1 => {
                format!("{} · {} ", repo.name, repo.status.branch)
            }
            Some(repo) => {
                let s = &repo.status;
                let counts = if s.ahead + s.behind > 0 {
                    format!(" {}↑ {}↓", s.ahead, s.behind)
                } else {
                    String::new()
                };
                format!("{}{} ", s.branch, counts)
            }
            None => String::new(),
        };
        // In unified mode the ⚙ lives in the activity bar; standalone puts it
        // at the header's right edge.
        let gear = (!self.merged()).then(|| format!("{} ", gear_icon(self.theme)));
        let gear_w = gear
            .as_ref()
            .map(|chip| Span::raw(chip.as_str()).width())
            .unwrap_or(0);
        let redraw_w = if gear.is_some() { 3 } else { 0 };
        // The main repository owns local Refresh + panel Collapse/Expand.
        self.title_zones.clear();
        let fold_action = if self.fully_collapsed() {
            TitleAction::ExpandAll
        } else {
            TitleAction::CollapseAll
        };
        let disclosure = if self.fully_collapsed() { " ▸ " } else { " ▾ " };
        let actions = [TitleAction::Refresh, fold_action];
        let header_area = Rect::new(
            area.x,
            area.y,
            area.width
                .saturating_sub(gear_w as u16 + redraw_w as u16),
            1,
        );
        self.zones.repo_header = header_area;
        let header_hovered = title_actions_visible(self.last_mouse, self.mouse_pos, area);
        let (mut spans, zones) = hover_action_row(
            self.theme,
            vec![Span::styled(disclosure, Style::default().bold())],
            Span::styled(title, Style::default().bold()),
            Span::styled(right_text, Style::default().dim()),
            header_hovered.then_some(actions.as_slice()),
            header_area,
            self.mouse_pos,
        );
        self.title_zones = zones;
        if redraw_w > 0 {
            let rx = area.x
                + area
                    .width
                    .saturating_sub(gear_w as u16 + redraw_w as u16);
            let (redraw, rect) = redraw_button(self.theme, rx, area.y, self.mouse_pos);
            self.zones.redraw = rect;
            spans.push(redraw);
        }
        if let Some(gear) = gear {
            let gx = area.x + area.width.saturating_sub(gear_w as u16);
            self.zones.gear = Rect::new(gx, area.y, gear_w as u16, 1);
            spans.push(Span::styled(
                gear,
                icon_button_style(
                    self.mouse_pos
                        .is_some_and(|(x, y)| hits(self.zones.gear, x, y)),
                    true,
                ),
            ));
        }
        let line = Line::from(spans).style(if header_hovered {
            Style::default().bg(HOVER_BG)
        } else {
            Style::default()
        });
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_message(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Message;
        let border = if focused {
            Style::default().fg(BUTTON_BLUE)
        } else {
            Style::default().dim()
        };
        let boxed = Block::bordered().border_style(border);
        let inner = boxed.inner(area);
        frame.render_widget(boxed, area);
        self.zones.message = area;

        // The suggest button lives at the right end of the input line — a
        // monochrome OUTLINE of the ✨ sparkles shape (MDI "creation" in the
        // material theme) in the normal foreground, never the colored emoji.
        let sparkle_glyph =
            if self.suggesting.is_some() { "…" } else { sparkle_icon(self.theme) };
        // The icon's width even while the "…" spinner shows, so the box
        // height computed in draw() always matches.
        let sparkle_w = Span::raw(sparkle_icon(self.theme)).width() as u16 + 1;
        let [text_area, sparkle_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(sparkle_w)]).areas(inner);
        frame.render_widget(Paragraph::new(sparkle_glyph), sparkle_area);
        self.zones.sparkle = sparkle_area;

        let (message, cursor, branch) = match self.active_repo() {
            Some(r) => (r.message.clone(), r.cursor, r.status.branch.clone()),
            None => (Vec::new(), 0, String::new()),
        };
        if message.is_empty() && !focused {
            let placeholder =
                message_placeholder(&branch, usize::from(text_area.width));
            frame.render_widget(Paragraph::new(placeholder).dim().italic(), text_area);
            return;
        }

        // Wrapped input: the box grows with the message (draw() sizes it) up
        // to MESSAGE_MAX_ROWS rows, then scrolls to keep the cursor visible.
        let field = text_area.width.max(1) as usize;
        let (rows, cursor_row, cursor_col) = wrap_message(&message, cursor, field);
        let (top, visible) = message_window(rows.len(), cursor_row, focused);
        let text: Vec<Line> = rows
            .iter()
            .skip(top)
            .take(visible)
            .map(|row| Line::from(row.clone()))
            .collect();
        frame.render_widget(Paragraph::new(text), text_area);
        if focused {
            frame.set_cursor_position(Position::new(
                text_area.x + cursor_col as u16,
                text_area.y + (cursor_row - top) as u16,
            ));
        }
    }

    fn draw_button(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Commit;
        let bg = if focused { BUTTON_BLUE_FOCUS } else { BUTTON_BLUE };
        let mut style = Style::default().bg(bg).fg(Color::White);
        if focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        frame.render_widget(Paragraph::new("✓ Commit").centered().style(style), area);
        self.zones.button = area;
    }

    /// The Sync Changes label, or `None` while there is nothing to sync
    /// (which hides the row entirely).
    fn sync_label(&self) -> Option<String> {
        if self.syncing.is_some() {
            return Some("⇅ Syncing…".to_string());
        }
        let status = &self.active_repo()?.status;
        if !status.has_upstream || status.ahead + status.behind == 0 {
            return None;
        }
        Some(format!("⇅ Sync Changes  {}↑ {}↓", status.ahead, status.behind))
    }

    /// A secondary button below Commit, VS Code's Sync Changes: pull + push
    /// with the outgoing↑ / incoming↓ counts.
    fn draw_sync(&mut self, frame: &mut Frame, area: Rect) {
        self.zones.sync = area;
        let Some(label) = self.sync_label() else { return };
        let style = if self.syncing.is_some() {
            Style::default().bg(Color::Rgb(0x2d, 0x2d, 0x33)).fg(Color::Gray)
        } else {
            Style::default().bg(Color::Rgb(0x3a, 0x3d, 0x41)).fg(Color::White)
        };
        frame.render_widget(Paragraph::new(label).centered().style(style), area);
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let theme = self.theme;
        let active = self.active;

        // Clamp the scroll and (keyboard nav only) walk it forward until the
        // selection fits — rows have variable heights.
        let h = (area.height as usize).max(1);
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(1));
        if self.snap {
            if let Some(sel) = self.selected {
                if sel < self.scroll {
                    self.scroll = sel;
                } else {
                    while self.scroll < sel {
                        let used: usize = (self.scroll..=sel)
                            .map(|i| self.row_height(self.rows[i]) as usize)
                            .sum();
                        if used <= h {
                            break;
                        }
                        self.scroll += 1;
                    }
                }
            }
            self.snap = false;
        }
        self.body = BodyGeom {
            left: area.x,
            top: area.y,
            height: area.height,
            width: area.width,
            offset: self.scroll,
        };
        // Git refreshes rebuild the row list, but a stationary pointer emits no new Moved event.
        self.hovered = self.mouse_pos.and_then(|(_, row)| self.row_at(row));
        let hovered = self.hovered;
        // Visible slice: everything from `scroll` until the viewport is
        // spent (plus one partially-clipped row).
        let mut end = self.scroll;
        let mut used = 0usize;
        while end < self.rows.len() && used < h {
            used += self.row_height(self.rows[end]) as usize;
            end += 1;
        }
        let visible = end - self.scroll;
        self.body.width = list_content_width(area.width, self.rows.len(), visible);
        let width = usize::from(self.body.width);

        let selected = self.selected;
        let list_focused = self.focus == Focus::List;
        let mut repo_title_zones = Vec::new();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
            .map(|(i, row)| {
                let row_hovered = hovered == Some(i);
                let item = match *row {
                    Row::RepoHeader(r) => {
                        let row_area = Rect::new(
                            area.x,
                            self.row_y(i).unwrap_or(area.y),
                            self.body.width,
                            1,
                        );
                        let (item, zones) = repo_header_item(
                            &self.repos[r],
                            r == active,
                            theme,
                            row_area,
                            row_hovered,
                            self.mouse_pos,
                        );
                        repo_title_zones.extend(
                            zones.into_iter().map(|(rect, action)| (rect, r, action)),
                        );
                        item
                    }
                    Row::Message(r) => message_box_item(
                        &self.repos[r],
                        r == active && self.focus == Focus::Message,
                        theme,
                        width,
                    ),
                    Row::Commit(r) => commit_button_item(
                        r == active,
                        r == active && self.focus == Focus::Commit,
                        width,
                    ),
                    Row::StagedHeader(r) => section_item(
                        "Staged Changes",
                        self.repos[r].staged_collapsed,
                        Some(self.repos[r].status.staged.len()),
                        width,
                        row_hovered.then_some('−'),
                    ),
                    Row::ChangesHeader(r) => section_item(
                        "Changes",
                        self.repos[r].changes_collapsed,
                        Some(self.repos[r].status.unstaged.len()),
                        width,
                        row_hovered.then_some('+'),
                    ),
                    Row::DrawerHeader(kind) => {
                        let mut item = section_item(
                            kind.title(),
                            !self.drawers[kind.index()].expanded,
                            None,
                            width,
                            None,
                        );
                        if kind == Drawer::FileHistory
                            && let Some(target) = &self.history_target
                        {
                            let name = target.rsplit('/').next().unwrap_or(target);
                            item = file_history_header(
                                !self.drawers[kind.index()].expanded,
                                name,
                            );
                        }
                        item
                    }
                    Row::DrawerLine(kind, i) => {
                        let expandable = kind.supports_file_tree()
                            && matches!(
                                self.drawers[kind.index()].refs.get(i),
                                Some(
                                    DrawerRef::Commit(_)
                                        | DrawerRef::Branch { .. }
                                        | DrawerRef::Stash(_)
                                        | DrawerRef::Tag(_)
                                )
                            );
                        let expanded = self.expanded_ref.as_ref().is_some_and(|expanded| {
                            expanded.kind == kind
                                && self.drawers[kind.index()].refs.get(i)
                                    == Some(&expanded.target)
                        });
                        drawer_line(
                            kind,
                            &self.drawers[kind.index()].lines[i],
                            expandable.then_some(expanded),
                            width,
                        )
                    }
                    Row::Staged(r, i) => change_tree_item(
                        &self.repos[r].staged_rows[i],
                        match self.repos[r].staged_rows[i] {
                            ChangeTreeRow::File { index, .. } => {
                                self.repos[r].status.staged.get(index)
                            }
                            ChangeTreeRow::Directory { .. } => None,
                        },
                        width,
                        theme,
                        ChangeTail::Status {
                            action: row_hovered.then_some('−'),
                            menu: row_hovered,
                        },
                        self.sidebar_state.scm_file_view == ScmFileView::Tree,
                        0,
                    ),
                    Row::Unstaged(r, i) => change_tree_item(
                        &self.repos[r].changes_rows[i],
                        match self.repos[r].changes_rows[i] {
                            ChangeTreeRow::File { index, .. } => {
                                self.repos[r].status.unstaged.get(index)
                            }
                            ChangeTreeRow::Directory { .. } => None,
                        },
                        width,
                        theme,
                        ChangeTail::Status {
                            action: row_hovered.then_some('+'),
                            menu: row_hovered,
                        },
                        self.sidebar_state.scm_file_view == ScmFileView::Tree,
                        0,
                    ),
                    Row::RepoSeparator => {
                        ListItem::new(repo_separator_line(width))
                    }
                    Row::HistoryTree(i) => self.expanded_ref.as_ref().map_or_else(
                        || ListItem::new(""),
                        |expanded| {
                            let entry = match expanded.rows[i] {
                                ChangeTreeRow::File { index, .. } => {
                                    expanded.files.get(index).map(|file| &file.entry)
                                }
                                ChangeTreeRow::Directory { .. } => None,
                            };
                            change_tree_item(
                                &expanded.rows[i],
                                entry,
                                width,
                                theme,
                                ChangeTail::History {
                                    menu: row_hovered
                                        && matches!(
                                            expanded.rows.get(i),
                                            Some(ChangeTreeRow::File { .. })
                                        ),
                                },
                                self.sidebar_state.scm_file_view == ScmFileView::Tree,
                                4,
                            )
                        },
                    ),
                    Row::HistoryNotice => ListItem::new(Line::from(Span::styled(
                        format!(
                            "   {}",
                            self.expanded_ref
                                .as_ref()
                                .and_then(|expanded| expanded.error.as_deref())
                                .unwrap_or("No changed files")
                        ),
                        Style::default().dim(),
                    ))),
                };
                if selected == Some(i) {
                    let style = if list_focused {
                        Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(Color::Rgb(0x2a, 0x2d, 0x2e))
                    };
                    item.style(style)
                } else if hovered == Some(i) && *row != Row::RepoSeparator {
                    item.style(Style::default().bg(HOVER_BG))
                } else {
                    item
                }
            })
            .collect();
        self.repo_title_zones = repo_title_zones;
        frame.render_widget(List::new(items), area);
        draw_scrollbar(frame, area, self.rows.len(), visible, self.scroll);
        if self.overlay.is_none()
            && let Some(index) = hovered
            && let Some(row_y) = self.row_y(index)
            && let Some(tooltip) = self.hovered_change_tooltip()
        {
            let anchor_x = self.mouse_pos.map_or(area.x, |(x, _)| x);
            draw_hover_tooltip(frame, area, row_y, anchor_x, tooltip);
        }

        // Terminal cursor inside the focused INLINE message box (multi-repo).
        if self.multi() && self.focus == Focus::Message {
            let target = self
                .rows
                .iter()
                .position(|row| matches!(row, Row::Message(r) if *r == self.active));
            if let Some(index) = target
                && let Some(y) = self.row_y(index)
                && y + 1 < area.y + area.height
                && let Some(repo) = self.active_repo()
            {
                let field = usize::from(inline_field_width(self.last_width));
                let (rows, cursor_row, cursor_col) =
                    wrap_message(&repo.message, repo.cursor, field);
                let (top, _) = message_window(rows.len(), cursor_row, true);
                let cy = y + 1 + (cursor_row - top) as u16;
                if cy + 1 < area.y + area.height {
                    frame.set_cursor_position(Position::new(
                        area.x + 1 + cursor_col as u16,
                        cy,
                    ));
                }
            }
        }
    }

    /// Footer content: a flash message or confirm prompt (WRAPPED — the
    /// one-line assumption used to clip them mid-question in narrow panes),
    /// or the hotkey hints.
    fn footer_lines(&self, width: u16) -> Vec<Line<'static>> {
        let message: Option<(String, Color)> = match (&self.overlay, &self.flash) {
            (Some(Overlay::ConfirmDiscard { entry, .. }), _) => Some((
                format!("Discard changes to '{}'? (y/N)", entry.path),
                DELETED,
            )),
            (Some(Overlay::ConfirmGit { prompt, .. }), _) => {
                Some((prompt.clone(), DELETED))
            }
            (_, Some((text, is_error))) => {
                let color = if *is_error { DELETED } else { UNTRACKED };
                let prefix = if *is_error { "" } else { "✓ " };
                Some((format!("{prefix}{text}"), color))
            }
            _ => None,
        };
        if let Some((msg, color)) = message {
            return wrap_footer_message(&msg, width, 4)
                .into_iter()
                .map(|l| Line::styled(l, Style::default().fg(color)))
                .collect();
        }
        if !self.show_hotkeys() {
            return Vec::new();
        }
        wrap_hints(&self.hints(), width, 3)
    }

    /// The hotkey hints, shown in Settings (and optionally the footer).
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints: Vec<(&'static str, &'static str)> = vec![
            ("⏎", "stage"),
            ("a", "all"),
            ("u", "none"),
            ("c", "msg"),
            ("A", "suggest"),
            ("o", "diff"),
            ("b", "hide"),
            ("S", "sync"),
            ("s", "settings"),
            ("r", "refresh"),
            ("q", "quit"),
        ];
        if self.merged() {
            hints.extend([("1", "files"), ("2", "git"), ("3", "search")]);
        }
        hints
    }

    /// Switch icon themes and REMEMBER it (see the explorer's twin).
    fn set_theme(&mut self, theme: IconTheme) {
        self.theme = theme;
        self.sidebar_state.icons = Some(theme);
        sidebar::save_state(self.sidebar_state);
    }

    /// The persisted "show hotkeys in the footer" setting.
    fn show_hotkeys(&self) -> bool {
        self.sidebar_state.show_hotkeys
    }

    /// Esc: close the preview pane beside us, if one is open.
    fn close_preview(&mut self) {
        if let Some(pane_id) = self.pane_ctl.as_ref().map(|c| c.pane_id.clone()) {
            herdr_sidebar::viewer::close_in_tab(&pane_id);
        }
    }

    /// Render the context-menu popup near its anchor, clamped inside the pane,
    /// and remember its rect for mouse hit-testing.
    fn draw_menu(&mut self, frame: &mut Frame) {
        let Some(Overlay::Menu { x, y, entries, selected, rect, .. }) = self.overlay.as_mut()
        else {
            return;
        };
        let area = Rect::new(0, 0, self.last_width, self.last_height);
        let label_width = entries
            .iter()
            .map(|e| match e {
                MenuEntry::Action(_, label) => label.chars().count(),
                MenuEntry::Separator => 0,
            })
            .max()
            .unwrap_or(0) as u16;
        let width = (label_width + 4).min(area.width);
        let height = (entries.len() as u16 + 2).min(area.height);
        let px = (*x).min(area.width.saturating_sub(width));
        let py = (*y + 1).min(area.height.saturating_sub(height));
        let popup = Rect::new(px, py, width, height);
        *rect = popup;

        let items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, entry)| match entry {
                MenuEntry::Separator => {
                    ListItem::new(Line::from("─".repeat(usize::from(width - 2)).dim()))
                }
                MenuEntry::Action(_, label) => {
                    let line = Line::raw(format!(" {label}"));
                    if i == *selected {
                        ListItem::new(line).style(
                            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ListItem::new(line)
                    }
                }
            })
            .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            List::new(items)
                .block(Block::bordered().border_style(Style::default().dim())),
            popup,
        );
    }
}

/// Next selectable (non-separator) menu index in `direction`, staying put at
/// the ends.
fn step_menu(entries: &[MenuEntry], from: usize, direction: isize) -> usize {
    let mut index = from as isize;
    loop {
        index += direction;
        if index < 0 || index >= entries.len() as isize {
            return from;
        }
        if matches!(entries[index as usize], MenuEntry::Action(..)) {
            return index as usize;
        }
    }
}

/// A repository row, matching VS Code's multi-repo Source Control: disclosure
/// arrow, repo icon and name on the left. The branch tail yields to the fixed
/// sync/commit action tail while hovered.
fn repo_header_item(
    repo: &Repo,
    active: bool,
    theme: IconTheme,
    area: Rect,
    actions_visible: bool,
    mouse_pos: Option<(u16, u16)>,
) -> (ListItem<'static>, Vec<(Rect, TitleAction)>) {
    let arrow = if repo.collapsed { "▸" } else { "▾" };
    let repo_icon = icon(theme, "", true, false);
    let name_style = if active { Style::default().bold() } else { Style::default().dim().bold() };
    let prefix = Span::styled(format!(" {arrow} "), Style::default().bold());
    let icon_span = Span::raw(format!("{} ", repo_icon.glyph));
    let s = &repo.status;
    let counts = if s.ahead + s.behind > 0 {
        format!(" {}↑ {}↓", s.ahead, s.behind)
    } else {
        String::new()
    };
    let actions = [TitleAction::Sync, TitleAction::Commit];
    let (spans, zones) = hover_action_row(
        theme,
        vec![prefix, icon_span],
        Span::styled(repo.name.clone(), name_style),
        Span::styled(
            format!("{} {}{}", branch_icon(theme), repo.branch_decor(), counts),
            Style::default().dim(),
        ),
        actions_visible.then_some(actions.as_slice()),
        area,
        mouse_pos,
    );
    (ListItem::new(Line::from(spans)), zones)
}

fn repo_separator_line(width: usize) -> Line<'static> {
    Line::from(Span::styled(
        "━".repeat(width),
        Style::default().fg(Color::Gray),
    ))
}

/// Columns the inline message box's input field spans (between the left
/// border and the ✧ button).
fn inline_field_width(pane_width: u16) -> u16 {
    pane_width.saturating_sub(2 + 3)
}

fn inline_sparkle(theme: IconTheme) -> String {
    format!(" {} ", sparkle_icon(theme))
}

/// Width-aware commit placeholder: drop detail rather than clipping
/// mid-word when the pane is narrow.
fn message_placeholder(branch: &str, width: usize) -> String {
    for text in [
        format!("Message (⏎ to commit on \"{branch}\")"),
        "Message (⏎ to commit)".to_string(),
        "Message".to_string(),
    ] {
        if Span::raw(text.as_str()).width() <= width {
            return text;
        }
    }
    String::new()
}

/// Most content rows a message box shows before it scrolls instead.
const MESSAGE_MAX_ROWS: usize = 4;

/// Wrap `message` into `field`-wide rows plus the cursor's (row, col). The
/// cursor may sit one past the end, opening a fresh row when that lands on a
/// wrap boundary.
fn wrap_message(message: &[char], cursor: usize, field: usize) -> (Vec<String>, usize, usize) {
    let field = field.max(1);
    let mut rows: Vec<String> = message.chunks(field).map(|c| c.iter().collect()).collect();
    if rows.is_empty() {
        rows.push(String::new());
    }
    if !message.is_empty() && message.len().is_multiple_of(field) && cursor == message.len() {
        rows.push(String::new());
    }
    (rows, cursor / field, cursor % field)
}

/// The `(top, count)` window of wrapped rows to show: everything when it
/// fits, else the slice keeping the cursor visible (or the start, unfocused).
fn message_window(rows: usize, cursor_row: usize, focused: bool) -> (usize, usize) {
    let visible = rows.min(MESSAGE_MAX_ROWS);
    let top = if focused { (cursor_row + 1).saturating_sub(visible) } else { 0 };
    (top, visible)
}

/// A repo's inline message box, VS Code style: a bordered input that grows
/// with its wrapped message, with the ✧ suggest button at its right end.
fn message_box_item(
    repo: &Repo,
    focused: bool,
    theme: IconTheme,
    width: usize,
) -> ListItem<'static> {
    let border = if focused {
        Style::default().fg(BUTTON_BLUE)
    } else {
        Style::default().dim()
    };
    let horizontal = "─".repeat(width.saturating_sub(2));
    let field = usize::from(inline_field_width(width as u16));

    let mut lines = vec![Line::from(Span::styled(format!("┌{horizontal}┐"), border))];
    if repo.message.is_empty() && !focused {
        let placeholder = message_placeholder(&repo.status.branch, field);
        let pad = field.saturating_sub(Span::raw(placeholder.as_str()).width());
        lines.push(Line::from(vec![
            Span::styled("│", border),
            Span::styled(placeholder, Style::default().dim().italic()),
            Span::raw(" ".repeat(pad)),
            Span::raw(inline_sparkle(theme)),
            Span::styled("│", border),
        ]));
    } else {
        let (rows, cursor_row, _) = wrap_message(&repo.message, repo.cursor, field);
        let (top, visible) = message_window(rows.len(), cursor_row, focused);
        for (i, row) in rows.iter().skip(top).take(visible).enumerate() {
            let pad = field.saturating_sub(Span::raw(row.as_str()).width());
            // The ✧ button owns the 3-column tail of the FIRST line only.
            let tail = if i == 0 {
                Span::raw(inline_sparkle(theme))
            } else {
                Span::raw("   ".to_string())
            };
            lines.push(Line::from(vec![
                Span::styled("│", border),
                Span::raw(row.clone()),
                Span::raw(" ".repeat(pad)),
                tail,
                Span::styled("│", border),
            ]));
        }
    }
    lines.push(Line::from(Span::styled(format!("└{horizontal}┘"), border)));
    ListItem::new(lines)
}

/// A repo's inline ✓ Commit button; only the active repo's button is fully lit.
fn commit_button_item(active: bool, focused: bool, width: usize) -> ListItem<'static> {
    let (bg, fg) = match (active, focused) {
        (true, true) => (BUTTON_BLUE_FOCUS, Color::White),
        (true, false) => (BUTTON_BLUE, Color::White),
        (false, _) => (Color::Rgb(0x24, 0x45, 0x5c), Color::Rgb(0x9a, 0xb2, 0xc2)),
    };
    let mut style = Style::default().bg(bg).fg(fg);
    if focused {
        style = style.add_modifier(Modifier::BOLD);
    }
    let label = "✓ Commit";
    let label_width = Span::raw(label).width();
    let left_pad = width.saturating_sub(label_width) / 2;
    let right_pad = width.saturating_sub(left_pad + label_width);
    ListItem::new(Line::from(Span::styled(
        format!("{}{label}{}", " ".repeat(left_pad), " ".repeat(right_pad)),
        style,
    )))
}

/// A collapsible section header; `count` renders as a right-aligned badge
/// (the drawers have no badge, like Git Graph's).
fn section_item(
    title: &str,
    collapsed: bool,
    count: Option<usize>,
    width: usize,
    action: Option<char>,
) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    let left = Span::styled(format!(" {arrow} {title}"), Style::default().bold());
    let Some(count) = count else {
        return ListItem::new(Line::from(left));
    };
    let badge = Span::styled(
        format!(" {count} "),
        Style::default().bg(BADGE_BLUE).fg(Color::White),
    );
    // Hovering shows the section-wide stage/unstage glyph before the badge.
    let action_span = action.map(|a| Span::styled(format!("{a} "), Style::default().bold()));
    let pad = action_span.as_ref().map_or_else(
        || width.saturating_sub(left.width() + badge.width() + 1).max(1),
        |_| section_action_start(title, width, count).saturating_sub(left.width()),
    );
    let mut spans = vec![left, Span::raw(" ".repeat(pad))];
    if let Some(a) = action_span {
        spans.push(a);
    }
    spans.push(badge);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

/// The FILE HISTORY header with the followed file's name appended, dimmed.
fn file_history_header(collapsed: bool, file: &str) -> ListItem<'static> {
    let arrow = if collapsed { "▸" } else { "▾" };
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {arrow} File History"), Style::default().bold()),
        Span::styled(format!("  {file}"), Style::default().dim()),
    ]))
}

/// One content line inside an expanded drawer. Branch lines highlight the
/// current branch (git's `%(HEAD)` renders it as `* name`).
fn drawer_line(
    kind: Drawer,
    text: &str,
    expanded: Option<bool>,
    width: usize,
) -> ListItem<'static> {
    let style = match kind {
        Drawer::Branches if text.starts_with('*') => {
            Style::default().fg(UNTRACKED).bold()
        }
        _ => Style::default(),
    };
    let prefix = expanded.map_or_else(
        || "   ".to_string(),
        |expanded| format!("   {} ", if expanded { '▾' } else { '▸' }),
    );
    ListItem::new(Line::from(Span::styled(
        truncate_to(format!("{prefix}{text}"), width),
        style,
    )))
}

#[derive(Clone, Copy)]
enum ChangeTail {
    Status { action: Option<char>, menu: bool },
    History { menu: bool },
}

struct ChangeTooltip {
    path: String,
    stat: DiffStat,
}

fn diff_stat_spans(stat: DiffStat) -> Vec<Span<'static>> {
    let mut stats = Vec::new();
    if let Some(added) = stat.added.filter(|count| *count > 0) {
        stats.push(Span::styled(format!("+{added}"), Style::default().fg(ADDED)));
    }
    if let Some(deleted) = stat.deleted.filter(|count| *count > 0) {
        if !stats.is_empty() {
            stats.push(Span::raw(" "));
        }
        stats.push(Span::styled(format!("-{deleted}"), Style::default().fg(DELETED)));
    }
    stats
}

fn directory_diff_stat<'a>(
    path: &str,
    entries: impl Iterator<Item = &'a FileEntry>,
) -> DiffStat {
    let prefix = format!("{path}/");
    let mut added = 0usize;
    let mut deleted = 0usize;
    let mut has_added = false;
    let mut has_deleted = false;
    for entry in entries.filter(|entry| entry.path.starts_with(&prefix)) {
        if let Some(count) = entry.stat.added {
            has_added = true;
            added = added.saturating_add(count);
        }
        if let Some(count) = entry.stat.deleted {
            has_deleted = true;
            deleted = deleted.saturating_add(count);
        }
    }
    DiffStat {
        added: has_added.then_some(added),
        deleted: has_deleted.then_some(deleted),
    }
}

fn change_tree_tooltip<'a>(
    row: &ChangeTreeRow,
    entry: Option<&FileEntry>,
    entries: impl Iterator<Item = &'a FileEntry>,
) -> Option<ChangeTooltip> {
    match row {
        ChangeTreeRow::Directory { path, .. } => Some(ChangeTooltip {
            path: path.replace('/', std::path::MAIN_SEPARATOR_STR),
            stat: directory_diff_stat(path, entries),
        }),
        ChangeTreeRow::File { .. } => {
            let entry = entry?;
            let mut path = entry.path.replace('/', std::path::MAIN_SEPARATOR_STR);
            if let Some(orig) = &entry.orig {
                path.push_str(" ← ");
                path.push_str(&orig.replace('/', std::path::MAIN_SEPARATOR_STR));
            }
            Some(ChangeTooltip {
                path,
                stat: entry.stat,
            })
        }
    }
}

fn draw_hover_tooltip(
    frame: &mut Frame,
    bounds: Rect,
    row_y: u16,
    anchor_x: u16,
    tooltip: ChangeTooltip,
) {
    if bounds.width < 14 || bounds.height < 3 {
        return;
    }
    let stats = diff_stat_spans(tooltip.stat);
    let stats_width = stats.iter().map(Span::width).sum::<usize>();
    let width = (Span::raw(tooltip.path.as_str()).width().max(stats_width) + 4)
        .clamp(14, bounds.width as usize) as u16;
    let mut lines: Vec<Line<'static>> =
        wrap_footer_message(&tooltip.path, width.saturating_sub(2), 0)
        .into_iter()
        .map(Line::from)
        .collect();
    let stat_line = if stats.is_empty() {
        None
    } else {
        let mut line = vec![Span::raw(" ")];
        line.extend(stats);
        Some(Line::from(line))
    };
    let max_lines = bounds.height.saturating_sub(2) as usize;
    lines.truncate(max_lines.saturating_sub(usize::from(stat_line.is_some())));
    lines.extend(stat_line);
    let height = (lines.len() + 2) as u16;

    let right = bounds.x.saturating_add(bounds.width);
    let bottom = bounds.y.saturating_add(bounds.height);
    let x = anchor_x
        .saturating_sub(1)
        .clamp(bounds.x, right.saturating_sub(width));
    let below = row_y.saturating_add(1);
    let y = if below.saturating_add(height) <= bottom {
        below
    } else {
        row_y.saturating_sub(height).max(bounds.y)
    };
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(Color::Rgb(0x25, 0x25, 0x26)).fg(Color::White))
            .block(Block::bordered().border_style(Style::default().fg(Color::DarkGray))),
        area,
    );
}

fn change_tree_item(
    row: &ChangeTreeRow,
    entry: Option<&FileEntry>,
    width: usize,
    theme: IconTheme,
    tail: ChangeTail,
    tree_mode: bool,
    base_indent: usize,
) -> ListItem<'static> {
    let (action_slot, action, menu) = match tail {
        ChangeTail::Status { action, menu } => (true, action, menu),
        ChangeTail::History { menu } => (false, None, menu),
    };
    let buttons_visible = menu || action.is_some();
    let ChangeTreeRow::File { depth, .. } = row else {
        let ChangeTreeRow::Directory { name, depth, expanded, .. } = row else {
            unreachable!()
        };
        let dir_icon = icon(theme, name, true, *expanded);
        let icon_style = dir_icon.rgb.map_or_else(Style::default, |(r, g, b)| {
            Style::default().fg(Color::Rgb(r, g, b))
        });
        let prefix = format!(
            "{}{} ",
            " ".repeat(1 + base_indent + depth * 2),
            if *expanded { '▾' } else { '▸' }
        );
        // Live directory actions occupy the same columns as file actions while
        // hovered, including the empty status slot that keeps hit zones aligned.
        let status_slot = action_slot && buttons_visible;
        let tail_width = change_tail_width(action_slot, buttons_visible, status_slot);
        let gap = usize::from(tail_width > 0);
        let icon_text = format!("{} ", dir_icon.glyph);
        let name_width = width
            .saturating_sub(
                Span::raw(prefix.as_str()).width()
                    + Span::raw(icon_text.as_str()).width()
                    + tail_width
                    + gap,
            )
            .max(1);
        let mut spans = vec![
            Span::raw(prefix),
            Span::styled(icon_text, icon_style),
            Span::raw(truncate_to(name.clone(), name_width)),
        ];
        let used = spans.iter().map(Span::width).sum::<usize>();
        let pad = width.saturating_sub(used + tail_width);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        if buttons_visible {
            spans.push(if menu {
                Span::styled(" ⋯ ", Style::default().bold())
            } else {
                Span::raw("   ")
            });
            if action_slot {
                spans.push(match action {
                    Some(action) => Span::styled(format!(" {action} "), Style::default().bold()),
                    None => Span::raw("   "),
                });
            }
        }
        if status_slot {
            spans.push(Span::raw("  "));
        }
        return ListItem::new(Line::from(spans));
    };
    let Some(entry) = entry else { return ListItem::new("") };
    let (dir, name) = match entry.path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, entry.path.as_str()),
    };
    let color = letter_color(entry.letter);
    let file_icon = icon(theme, name, false, false);
    let icon_style = match file_icon.rgb {
        Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)),
        None => Style::default(),
    };
    let tail_width = change_tail_width(action_slot, buttons_visible, true);
    let wanted_indent = if tree_mode {
        3 + base_indent + depth * 2
    } else {
        3 + base_indent
    };
    let icon_text = format!("{} ", file_icon.glyph);
    let icon_width = Span::raw(icon_text.as_str()).width();
    let left_limit = width.saturating_sub(tail_width + 1);
    let indent = wanted_indent.min(left_limit.saturating_sub(icon_width));
    let name_width = left_limit.saturating_sub(indent + icon_width);
    let mut spans = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(icon_text, icon_style),
        Span::styled(truncate_to(name.to_string(), name_width), Style::default().fg(color)),
    ];
    if !tree_mode && let Some(dir) = dir {
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let used: usize = spans.iter().map(Span::width).sum();
        let avail = left_limit.saturating_sub(used);
        let text = truncate_to(format!(" {}", dir.replace('/', &sep)), avail);
        if !text.is_empty() {
            spans.push(Span::styled(text, Style::default().dim()));
        }
    }
    if let Some(orig) = &entry.orig {
        let used: usize = spans.iter().map(Span::width).sum();
        let avail = left_limit.saturating_sub(used);
        let text = truncate_to(format!(" ← {}", orig.replace('/', std::path::MAIN_SEPARATOR_STR)), avail);
        if !text.is_empty() {
            spans.push(Span::styled(text, Style::default().dim()));
        }
    }
    let left_width: usize = spans.iter().map(Span::width).sum();
    let letter = Span::styled(entry.letter.to_string(), Style::default().fg(color).bold());
    let pad = width.saturating_sub(left_width + tail_width);
    spans.push(Span::raw(" ".repeat(pad)));
    if buttons_visible {
        spans.push(if menu {
            Span::styled(" ⋯ ", Style::default().bold())
        } else {
            Span::raw("   ")
        });
        if action_slot {
            spans.push(match action {
                Some(action) => Span::styled(format!(" {action} "), Style::default().bold()),
                None => Span::raw("   "),
            });
        }
    }
    spans.push(letter);
    spans.push(Span::raw(" "));
    ListItem::new(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn drawer_lines_parse_into_actionable_refs() {
        assert_eq!(
            parse_drawer_ref(Drawer::Commits, "a1b2c3d Add telemetry module"),
            DrawerRef::Commit("a1b2c3d".into())
        );
        // Graph edge-only lines carry no commit; subject words never match
        // (uppercase or non-hex letters, or too short).
        assert_eq!(parse_drawer_ref(Drawer::Graph, "| \\"), DrawerRef::None);
        assert_eq!(
            parse_drawer_ref(Drawer::Graph, "* deadbee (HEAD -> main) Added decoded fallback"),
            DrawerRef::Commit("deadbee".into())
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Branches, "* main"),
            DrawerRef::Branch { name: "main".into(), current: true }
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Branches, "  origin/main"),
            DrawerRef::Branch { name: "origin/main".into(), current: false }
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Stashes, "stash@{2}: WIP on main: 1a2b3c4 x"),
            DrawerRef::Stash(2)
        );
        assert_eq!(
            parse_drawer_ref(Drawer::Remotes, "origin  https://github.com/a/b.git"),
            DrawerRef::Remote {
                name: "origin".into(),
                url: "https://github.com/a/b.git".into()
            }
        );
        assert_eq!(parse_drawer_ref(Drawer::Tags, "v0.1.0"), DrawerRef::Tag("v0.1.0".into()));
        assert_eq!(parse_drawer_ref(Drawer::Tags, "(none)"), DrawerRef::None);
        assert_eq!(
            parse_drawer_ref(Drawer::Worktrees, "C:/Users/x/proj  a1b2c3d [main]"),
            DrawerRef::Worktree("C:/Users/x/proj".into())
        );
        assert_eq!(parse_drawer_ref(Drawer::Worktrees, "(none)"), DrawerRef::None);
    }

    #[test]
    fn drawer_lines_prettify_for_display() {
        assert_eq!(
            pretty_worktree_line("C:/Users/x/Projects/herdr  7c12b6d [main]"),
            "herdr  ⎇ main"
        );
        assert_eq!(
            pretty_worktree_line("C:/x/wt-fix  1a2b3c4 (detached HEAD)"),
            "wt-fix  (detached)"
        );
        assert_eq!(pretty_worktree_line("(none)"), "(none)");
        assert_eq!(
            pretty_remote_line("origin  https://github.com/alexarthurs/herdr-sidebar.git"),
            "origin  alexarthurs/herdr-sidebar"
        );
        assert_eq!(
            pretty_remote_line("up  git@github.com:me/repo.git"),
            "up  me/repo"
        );
        assert_eq!(
            pretty_remote_line("origin  C:/Users/x/Projects/.demo-origin.git"),
            "origin  .demo-origin"
        );
        assert_eq!(pretty_remote_line("(none)"), "(none)");
    }

    #[test]
    fn menu_navigation_skips_separators_and_clamps() {
        let entries = [
            MenuEntry::Action(MenuAction::StageOrUnstage, "Stage Changes"),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::CopyPath, "Copy Path"),
        ];
        assert_eq!(step_menu(&entries, 0, -1), 0);
        assert_eq!(step_menu(&entries, 0, 1), 2, "skips the separator");
        assert_eq!(step_menu(&entries, 2, 1), 2);
    }

    #[test]
    fn drawer_titles_are_title_case_and_include_worktrees() {
        let titles: Vec<&str> = Drawer::ALL.iter().map(|d| d.title()).collect();
        assert_eq!(
            titles,
            [
                "Graph",
                "Commits",
                "File History",
                "Branches",
                "Worktrees",
                "Remotes",
                "Stashes",
                "Tags"
            ]
        );
    }

    #[test]
    fn modern_history_drawers_are_explicit() {
        assert!(Drawer::Commits.supports_file_tree());
        assert!(Drawer::Branches.supports_file_tree());
        assert!(Drawer::Stashes.supports_file_tree());
        assert!(Drawer::Tags.supports_file_tree());
        assert!(!Drawer::Graph.supports_file_tree());
        assert!(!Drawer::FileHistory.supports_file_tree());
    }

    #[test]
    fn repo_header_hover_reserves_actions_before_truncating_a_long_name() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-repo-header-{}",
            std::process::id()
        ));
        init_git_repo(&root);
        let mut repo = Repo::new(Git::discover(&root).unwrap());
        repo.name = "a-project-name-that-is-far-too-long".into();
        repo.status.branch = "main".into();

        let width = 28u16;
        let (item, zones) = repo_header_item(
            &repo,
            true,
            IconTheme::Material,
            Rect::new(0, 0, width, 1),
            true,
            None,
        );
        assert_eq!(item.width(), usize::from(width));
        assert_eq!(zones[0], (Rect::new(width - 6, 0, 3, 1), TitleAction::Sync));
        assert_eq!(zones[1], (Rect::new(width - 3, 0, 3, 1), TitleAction::Commit));
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| frame.render_widget(List::new(vec![item]), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for x in 0..width {
            rendered.push_str(buffer[(x, 0)].symbol());
        }
        assert!(rendered.contains('…'));
        assert!(rendered.ends_with(&format!(
            " {}  {} ",
            herdr_sidebar::ui::title_action_icon(IconTheme::Material, TitleAction::Sync),
            herdr_sidebar::ui::title_action_icon(IconTheme::Material, TitleAction::Commit),
        )));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn main_repo_header_actions_require_hover_on_that_header_row() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-main-header-{}",
            std::process::id()
        ));
        init_git_repo(&root);
        let mut app = App::new_with_pane(root.clone(), None);
        app.repos[0].name = "mairo_competition_Backend_with_a_long_suffix".into();
        app.last_mouse = Some(std::time::Instant::now());
        let area = Rect::new(0, 1, 28, 1);
        let backend = TestBackend::new(28, 3);
        let mut terminal = Terminal::new(backend).unwrap();

        app.mouse_pos = Some((4, 2));
        terminal.draw(|frame| app.draw_header(frame, area)).unwrap();
        assert!(app.title_zones.is_empty(), "another SCM row is not header hover");
        assert_eq!(terminal.backend().buffer()[(0, 1)].bg, Color::Reset);

        app.mouse_pos = Some((4, 1));
        terminal.draw(|frame| app.draw_header(frame, area)).unwrap();
        assert_eq!(app.title_zones.len(), 2);
        assert_eq!(app.title_zones[0].1, TitleAction::Refresh);
        assert_eq!(app.title_zones[1].1, TitleAction::CollapseAll);
        let rendered = (0..28)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(rendered.contains('…'), "long main repo name yields to actions");
        assert_eq!(terminal.backend().buffer()[(0, 1)].bg, HOVER_BG);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collapse_all_switches_to_expand_all_without_touching_drawers() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-expand-all-{}",
            std::process::id()
        ));
        let child = root.join("child-repository");
        let _ = std::fs::remove_dir_all(&root);
        init_git_repo(&root);
        init_git_repo(&child);

        let mut app = App::new_with_pane(root.clone(), None);
        assert!(app.multi());
        for repo in &mut app.repos {
            repo.staged_dirs.insert("src".into());
            repo.changes_dirs.insert("docs".into());
        }
        app.drawers[Drawer::Graph.index()].expanded = true;
        app.drawers[Drawer::Commits.index()].expanded = false;

        app.collapse_all();
        assert!(app.fully_collapsed());
        assert!(app.repos.iter().all(|repo| {
            repo.collapsed && repo.staged_collapsed && repo.changes_collapsed
        }));
        assert!(app.drawers[Drawer::Graph.index()].expanded);
        assert!(!app.drawers[Drawer::Commits.index()].expanded);

        app.expand_all();
        assert!(!app.fully_collapsed());
        assert!(app.repos.iter().all(|repo| {
            !repo.collapsed
                && !repo.staged_collapsed
                && !repo.changes_collapsed
                && repo.staged_dirs.is_empty()
                && repo.changes_dirs.is_empty()
        }));
        assert!(app.drawers[Drawer::Graph.index()].expanded);
        assert!(!app.drawers[Drawer::Commits.index()].expanded);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn single_repo_collapse_hides_controls_and_status_sections() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-single-collapse-{}",
            std::process::id()
        ));
        init_git_repo(&root);

        let mut app = App::new_with_pane(root.clone(), None);
        app.repos[0].status.has_upstream = true;
        app.repos[0].status.ahead = 1;
        app.drawers[Drawer::Graph.index()].expanded = true;
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.zones.message.height > 0);
        assert_eq!(app.zones.button.height, 1);
        assert_eq!(app.zones.sync.height, 1);

        app.collapse_all();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.fully_collapsed());
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.zones.message, Rect::default());
        assert_eq!(app.zones.button, Rect::default());
        assert_eq!(app.zones.sync, Rect::default());
        assert!(!app.rows.iter().any(|row| matches!(
            row,
            Row::StagedHeader(_) | Row::ChangesHeader(_) | Row::Staged(_, _) | Row::Unstaged(_, _)
        )));
        assert!(app.drawers[Drawer::Graph.index()].expanded);

        app.expand_all();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(!app.fully_collapsed());
        assert!(app.zones.message.height > 0);
        assert_eq!(app.zones.button.height, 1);
        assert_eq!(app.zones.sync.height, 1);
        assert!(app.rows.iter().any(|row| matches!(row, Row::ChangesHeader(_))));
        assert!(app.drawers[Drawer::Graph.index()].expanded);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn single_repo_title_click_toggles_that_repo() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-single-title-click-{}",
            std::process::id()
        ));
        init_git_repo(&root);
        let mut app = App::new_with_pane(root.clone(), None);
        app.last_width = 60;
        app.last_height = 24;
        app.zones.repo_header = Rect::new(0, 2, 50, 1);

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(click);
        assert!(app.repos[0].collapsed);
        app.on_mouse(click);
        assert!(!app.repos[0].collapsed);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn multi_repo_widgets_and_drawer_lines_stay_inside_content_width() {
        for theme in [IconTheme::Material, IconTheme::Emoji] {
            let tail = inline_sparkle(theme);
            assert_eq!(Span::raw(tail.as_str()).width(), 3);
            assert_eq!(usize::from(inline_field_width(42)) + 2 + Span::raw(tail).width(), 42);
        }

        let backend = TestBackend::new(42, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    List::new(vec![commit_button_item(true, false, 42)]),
                    frame.area(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(41, 0)].symbol(), " ");
        assert_eq!(buffer[(41, 0)].bg, BUTTON_BLUE);
        assert!((0..42).all(|x| buffer[(x, 0)].symbol() != "∨"));

        let item = drawer_line(
            Drawer::Commits,
            "d15aea93a feat(problem-bank): 支持题集权限关系和更多内容",
            Some(false),
            42,
        );
        assert!(item.width() <= 42);
    }

    #[test]
    fn change_action_has_a_stable_three_column_hit_zone() {
        assert!(!change_action_hit(34, 40));
        assert!(change_action_hit(35, 40));
        assert!(change_action_hit(37, 40));
        assert!(!change_action_hit(38, 40));
    }

    #[test]
    fn change_rows_reserve_action_width_only_while_hovered() {
        assert_eq!(change_tail_width(true, false, true), 2);
        assert_eq!(change_tail_width(true, true, true), 8);
        assert_eq!(change_tail_width(false, false, true), 2);
        assert_eq!(change_tail_width(false, true, true), 5);
        assert_eq!(change_tail_width(true, false, false), 0);
    }

    #[test]
    fn change_menu_precedes_the_existing_action_and_status_cells() {
        assert!(!change_menu_hit(31, 40, true));
        assert!(change_menu_hit(32, 40, true));
        assert!(change_menu_hit(34, 40, true));
        assert!(!change_menu_hit(35, 40, true));

        assert!(!change_menu_hit(34, 40, false));
        assert!(change_menu_hit(35, 40, false));
        assert!(change_menu_hit(37, 40, false));
        assert!(!change_menu_hit(38, 40, false));
    }

    #[test]
    fn section_action_hit_tracks_the_glyph_before_the_count_badge() {
        assert_eq!(section_action_start("Changes", 28, 73), 21);
        assert!(section_action_hit(20, "Changes", 28, 73));
        assert!(section_action_hit(21, "Changes", 28, 73));
        assert!(section_action_hit(22, "Changes", 28, 73));
        assert!(!section_action_hit(23, "Changes", 28, 73));
        assert!(!section_action_hit(24, "Changes", 28, 73));
    }

    #[test]
    fn bulk_status_moves_target_the_destination_header() {
        let mut staged = vec![Row::StagedHeader(0)];
        staged.extend((0..73).map(|index| Row::Staged(0, index)));
        staged.push(Row::ChangesHeader(0));
        assert_eq!(status_header_index(&staged, 0, true), Some(0));
        assert_eq!(status_header_index(&staged, 0, false), Some(74));

        assert_eq!(status_header_index(&[Row::ChangesHeader(0)], 0, false), Some(0));
    }

    #[test]
    fn repo_separator_is_full_width_and_not_selectable() {
        assert!(!Row::RepoSeparator.selectable());
        let line = repo_separator_line(28);
        assert_eq!(line.spans.iter().map(Span::width).sum::<usize>(), 28);

        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-single-repo-divider-{}",
            std::process::id()
        ));
        init_git_repo(&root);
        let app = App::new_with_pane(root.clone(), None);
        let graph = app
            .rows
            .iter()
            .position(|row| *row == Row::DrawerHeader(Drawer::Graph))
            .unwrap();
        assert!(matches!(app.rows[graph - 1], Row::RepoSeparator));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn overflowing_lists_reserve_the_scrollbar_column_before_action_layout() {
        assert_eq!(list_content_width(30, 3, 20), 30);
        assert_eq!(list_content_width(30, 73, 20), 29);
        assert_eq!(list_content_width(1, 73, 20), 1);
        assert_eq!(list_content_width(0, 73, 20), 0);

        let content_width = list_content_width(30, 73, 20);
        assert!(change_menu_hit(21, content_width, true));
        assert!(change_menu_hit(23, content_width, true));
        assert!(change_action_hit(24, content_width));
        assert!(change_action_hit(26, content_width));
        assert!(!change_action_hit(27, content_width));
        assert!(!change_action_hit(29, content_width));
    }

    #[test]
    fn directory_actions_use_one_prefix_and_preserve_external_rename_sources() {
        let entries = vec![
            FileEntry {
                path: "src/nested/one.rs".into(),
                orig: None,
                letter: 'M',
                stat: DiffStat::default(),
            },
            FileEntry {
                path: "src/two.rs".into(),
                orig: Some("legacy/two.rs".into()),
                letter: 'R',
                stat: DiffStat::default(),
            },
            FileEntry {
                path: "README.md".into(),
                orig: None,
                letter: 'M',
                stat: DiffStat::default(),
            },
        ];
        assert_eq!(directory_pathspecs("src", &entries), ["src", "legacy/two.rs"]);
    }

    #[test]
    fn every_change_row_offers_the_full_path_and_stats_tooltip() {
        let entry = FileEntry {
            path: "plugins/herdr-sidebar/src/a_very_long_filename.rs".into(),
            orig: None,
            letter: 'M',
            stat: herdr_sidebar::git::DiffStat {
                added: Some(417),
                deleted: Some(14),
            },
        };
        let row = ChangeTreeRow::File { index: 0, depth: 0 };
        let tooltip = change_tree_tooltip(&row, Some(&entry), std::iter::once(&entry)).unwrap();
        assert_eq!(tooltip.path, entry.path);
        assert_eq!(tooltip.stat, entry.stat);

        let other = FileEntry {
            path: "README.md".into(),
            orig: None,
            letter: 'M',
            stat: DiffStat { added: Some(3), deleted: None },
        };
        let directory = ChangeTreeRow::Directory {
            path: "plugins/herdr-sidebar".into(),
            name: "plugins/herdr-sidebar".into(),
            depth: 0,
            expanded: true,
        };
        let entries = [&entry, &other];
        let tooltip = change_tree_tooltip(&directory, None, entries.into_iter()).unwrap();
        assert_eq!(tooltip.path, "plugins/herdr-sidebar");
        assert_eq!(tooltip.stat, entry.stat);
    }

    #[test]
    fn expanding_a_compacted_path_clears_collapsed_ancestors() {
        let mut collapsed = BTreeSet::from(["src".to_string(), "other".to_string()]);
        toggle_collapsed_path(&mut collapsed, "src/components", false);
        assert_eq!(collapsed, BTreeSet::from(["other".to_string()]));
        toggle_collapsed_path(&mut collapsed, "src/components", true);
        assert!(collapsed.contains("src/components"));
    }

    #[test]
    fn workspace_state_restores_scm_tree_and_viewport_anchors() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-scm-sync-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "init"]);
        std::fs::write(root.join("src/main.rs"), "fn main() { println!(\"changed\"); }\n")
            .unwrap();
        std::fs::write(root.join("docs/note.md"), "new\n").unwrap();

        let mut source = App::new_with_pane(root.clone(), None);
        source.repos[0].staged_collapsed = true;
        source.repos[0].changes_dirs.insert("docs".into());
        source.repos[0].rebuild_file_rows(source.sidebar_state.scm_file_view);
        source.rebuild();
        source.selected = source.rows.iter().enumerate().find_map(|(index, _)| {
            matches!(
                source.anchor_at(index),
                Some(ScmAnchor::Changes { ref path, .. }) if path == "src/main.rs"
            )
            .then_some(index)
        });
        source.scroll = source
            .rows
            .iter()
            .position(|row| matches!(row, Row::ChangesHeader(0)))
            .unwrap();
        source.focus = Focus::Commit;
        let expected = source.workspace_state();

        let mut target = App::new_with_pane(root.clone(), None);
        target.apply_workspace_state(&expected);
        assert_eq!(target.workspace_state(), expected);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_state_keeps_the_multi_repo_separator_above_the_top_anchor() {
        let root = std::env::temp_dir().join(format!(
            "herdr-sidebar-scm-sync-separator-{}",
            std::process::id()
        ));
        let child = root.join("packages/child");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "root\n").unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "root"]);

        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("lib.rs"), "pub fn child() {}\n").unwrap();
        git(&child, &["init", "--quiet"]);
        git(&child, &["config", "user.email", "test@example.com"]);
        git(&child, &["config", "user.name", "Test"]);
        git(&child, &["add", "."]);
        git(&child, &["commit", "--quiet", "-m", "child"]);

        let mut source = App::new_with_pane(root.clone(), None);
        assert!(source.multi());
        assert!(matches!(source.rows.first(), Some(Row::RepoSeparator)));
        source.scroll = 0;
        let shared = source.workspace_state();
        assert_eq!(shared.top_offset, 1);
        assert!(matches!(shared.top, Some(ScmAnchor::Repo(_))));

        let mut target = App::new_with_pane(root.clone(), None);
        target.scroll = 4;
        target.apply_workspace_state(&shared);
        assert_eq!(target.scroll, 0);
        assert!(matches!(target.rows.first(), Some(Row::RepoSeparator)));
        assert_eq!(target.workspace_state(), shared);

        let _ = std::fs::remove_dir_all(root);
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn init_git_repo(root: &std::path::Path) {
        let _ = std::fs::remove_dir_all(root);
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("README.md"), "test\n").unwrap();
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", "init"]);
    }

}
