//! Workspace-scoped visual state shared by unified Sidebar panes in sibling tabs.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ipc;
use crate::state::{self, View};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorerState {
    pub selected: Option<PathBuf>,
    pub top: Option<PathBuf>,
    pub expanded: BTreeSet<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScmFocus {
    #[default]
    List,
    Message,
    Commit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ScmDrawer {
    Graph,
    Commits,
    FileHistory,
    Branches,
    Worktrees,
    Remotes,
    Stashes,
    Tags,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScmRef {
    None,
    Commit(String),
    Stash(usize),
    Branch(String),
    Remote(String),
    Tag(String),
    Worktree(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScmAnchor {
    Repo(PathBuf),
    Message(PathBuf),
    Commit(PathBuf),
    StagedHeader(PathBuf),
    ChangesHeader(PathBuf),
    Staged { repo: PathBuf, path: String },
    Changes { repo: PathBuf, path: String },
    DrawerHeader(ScmDrawer),
    DrawerLine {
        drawer: ScmDrawer,
        target: ScmRef,
    },
    HistoryPath(String),
    HistoryNotice,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmRepoState {
    pub root: PathBuf,
    pub collapsed: bool,
    pub staged_collapsed: bool,
    pub changes_collapsed: bool,
    pub staged_dirs: BTreeSet<String>,
    pub changes_dirs: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpandedScmRef {
    pub drawer: ScmDrawer,
    pub target: ScmRef,
    pub collapsed: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScmState {
    pub active_repo: Option<PathBuf>,
    pub focus: ScmFocus,
    pub selected: Option<ScmAnchor>,
    pub top: Option<ScmAnchor>,
    #[serde(default)]
    pub top_offset: usize,
    pub repos: Vec<ScmRepoState>,
    pub drawers: BTreeSet<ScmDrawer>,
    pub expanded_ref: Option<ExpandedScmRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub root: PathBuf,
    pub active: View,
    pub width: u16,
    pub explorer: Option<ExplorerState>,
    pub scm: Option<ScmState>,
    writer: String,
    revision: u64,
}

impl WorkspaceState {
    fn new(root: PathBuf, active: View) -> Self {
        Self {
            root,
            active,
            width: 0,
            explorer: None,
            scm: None,
            writer: String::new(),
            revision: 0,
        }
    }

}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FocusState {
    sidebar_focused: bool,
    writer: String,
    revision: u64,
}

#[derive(Deserialize)]
struct PaneListMessage {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Clone, Deserialize)]
struct Pane {
    pane_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    tokens: serde_json::Value,
}

pub struct Session {
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    root: PathBuf,
    path: PathBuf,
    seen: Option<(String, u64)>,
    next_revision: u64,
    latest: Option<WorkspaceState>,
    unified: bool,
    pane_focused: bool,
}

impl Session {
    pub fn connect(root: &Path, unified: bool) -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID").ok().filter(|id| !id.is_empty())?;
        let json = ipc::call_text("pane.list", serde_json::json!({})).ok()?;
        let panes = parse_panes(&json)?;
        let pane = panes.iter().find(|pane| pane.pane_id == pane_id)?;
        let root = normalize_root(root);
        let path = visual_path(&pane.workspace_id, &root)?;
        let latest = read_json::<WorkspaceState>(&path).filter(|state| state.root == root);
        let next_revision = latest.as_ref().map(|state| state.revision).unwrap_or(0);
        Some(Self {
            pane_id,
            workspace_id: pane.workspace_id.clone(),
            tab_id: pane.tab_id.clone(),
            root,
            path,
            seen: None,
            next_revision,
            latest,
            unified,
            pane_focused: pane.focused,
        })
    }

    pub fn set_root(&mut self, root: &Path) {
        let root = normalize_root(root);
        if root == self.root {
            return;
        }
        self.root = root;
        if let Some(path) = visual_path(&self.workspace_id, &self.root) {
            self.path = path;
        }
        self.seen = None;
        self.latest = read_json::<WorkspaceState>(&self.path).filter(|state| state.root == self.root);
        self.next_revision = self.latest.as_ref().map(|state| state.revision).unwrap_or(0);
    }

    pub fn poll(&mut self) -> Option<WorkspaceState> {
        let state = read_json::<WorkspaceState>(&self.path)?;
        if state.root != self.root {
            return None;
        }
        let token = (state.writer.clone(), state.revision);
        if self.seen.as_ref() == Some(&token) {
            return None;
        }
        self.next_revision = self.next_revision.max(state.revision);
        self.seen = Some(token);
        self.latest = Some(state.clone());
        Some(state)
    }

    pub fn latest(&self) -> Option<&WorkspaceState> {
        self.latest.as_ref()
    }

    pub fn publish_explorer(&mut self, active: View, width: u16, explorer: ExplorerState) {
        self.publish(active, width, |state| state.explorer = Some(explorer));
    }

    pub fn publish_scm(&mut self, active: View, width: u16, scm: ScmState) {
        self.publish(active, width, |state| state.scm = Some(scm));
    }

    pub fn publish_active(&mut self, active: View, width: u16) {
        self.publish(active, width, |_| {});
    }

    fn publish(&mut self, active: View, width: u16, update: impl FnOnce(&mut WorkspaceState)) {
        if !self.unified {
            return;
        }
        let mut state = read_json::<WorkspaceState>(&self.path)
            .filter(|state| state.root == self.root)
            .unwrap_or_else(|| WorkspaceState::new(self.root.clone(), active));
        state.active = active;
        if width > 0 {
            state.width = width;
        }
        update(&mut state);
        self.next_revision = self.next_revision.max(state.revision).saturating_add(1);
        state.writer.clone_from(&self.pane_id);
        state.revision = self.next_revision;
        if write_json(&self.path, &state) {
            self.seen = Some((state.writer.clone(), state.revision));
            self.latest = Some(state);
        }
    }

    pub fn note_focus_gained(&mut self) {
        self.pane_focused = true;
        self.publish_focus(true);
    }

    pub fn note_interaction(&mut self) {
        if !self.pane_focused {
            self.note_focus_gained();
        }
    }

    pub fn note_focus_lost(&mut self) {
        self.pane_focused = false;
        let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) else { return };
        let Some(panes) = parse_panes(&json) else { return };
        let Some(focused) = panes.iter().find(|pane| pane.focused) else { return };
        if focused.workspace_id == self.workspace_id
            && focused.tab_id == self.tab_id
            && focused.pane_id != self.pane_id
        {
            self.publish_focus(false);
        }
    }

    pub fn clear_focus(&mut self) {
        self.pane_focused = false;
        self.publish_focus(false);
    }

    pub fn pane_focused(&self) -> bool {
        self.pane_focused
    }

    pub fn set_unified(&mut self, unified: bool) {
        if self.unified && !unified
            && let Some(path) = focus_path(&self.workspace_id)
        {
            let _ = fs::remove_file(path);
        }
        self.unified = unified;
    }

    fn publish_focus(&mut self, sidebar_focused: bool) {
        if !self.unified {
            return;
        }
        let Some(path) = focus_path(&self.workspace_id) else { return };
        let mut state = read_json::<FocusState>(&path).unwrap_or_default();
        state.sidebar_focused = sidebar_focused;
        state.writer.clone_from(&self.pane_id);
        state.revision = state.revision.saturating_add(1);
        let _ = write_json(&path, &state);
    }
}

/// Apply the last Sidebar/content focus role after a tab/workspace switch.
/// The caller is the ensure hook, so this runs after the destination tab is visible.
pub fn restore_focused_tab_sidebar() {
    if !state::load_state().merged {
        return;
    }
    let Ok(json) = ipc::call_text("pane.list", serde_json::json!({})) else { return };
    let Some(panes) = parse_panes(&json) else { return };
    let Some(focused) = panes.iter().find(|pane| pane.focused) else { return };
    let Some(path) = focus_path(&focused.workspace_id) else { return };
    let Some(focus) = read_json::<FocusState>(&path) else { return };
    if let Some(target) = focus_destination(&panes, focus.sidebar_focused)
        && target != focused.pane_id
    {
        let _ = ipc::call_text(
            "pane.focus",
            serde_json::json!({ "pane_id": target }),
        );
    }
}

fn focus_destination(panes: &[Pane], sidebar_focused: bool) -> Option<String> {
    let focused = panes.iter().find(|pane| pane.focused)?;
    let is_sidebar = |pane: &&Pane| {
        pane.tab_id == focused.tab_id
            && pane.agent.is_none()
            && token_string(&pane.tokens, "herdr-sidebar-explorer").is_some()
            && token_string(&pane.tokens, "herdr-sidebar-git").is_some()
    };
    if sidebar_focused {
        panes.iter().find(is_sidebar).map(|pane| pane.pane_id.clone())
    } else if is_sidebar(&focused) {
        panes
            .iter()
            .find(|pane| pane.tab_id == focused.tab_id && !is_sidebar(pane))
            .map(|pane| pane.pane_id.clone())
    } else {
        Some(focused.pane_id.clone())
    }
}

fn parse_panes(json: &str) -> Option<Vec<Pane>> {
    serde_json::from_str::<PaneListMessage>(json.trim_start_matches('\u{feff}'))
        .ok()
        .map(|message| message.result.panes)
}

fn token_string(tokens: &serde_json::Value, name: &str) -> Option<String> {
    let value = tokens.get(name)?;
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Object(map) => map.get("value")?.as_str().map(str::to_string),
        _ => None,
    }
}

fn normalize_root(root: &Path) -> PathBuf {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    #[cfg(windows)]
    {
        let text = root.to_string_lossy();
        return PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text));
    }
    #[cfg(not(windows))]
    root
}

fn visual_path(workspace: &str, root: &Path) -> Option<PathBuf> {
    let hash = root_hash(root);
    Some(sync_dir()?.join(format!("{}-{hash:016x}.json", safe_name(workspace))))
}

fn root_hash(root: &Path) -> u64 {
    root.to_string_lossy().bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        hash.wrapping_mul(0x100000001b3) ^ u64::from(byte)
    })
}

fn focus_path(workspace: &str) -> Option<PathBuf> {
    Some(sync_dir()?.join(format!("{}-focus.json", safe_name(workspace))))
}

fn sync_dir() -> Option<PathBuf> {
    Some(state::state_dir()?.join("workspace-sync"))
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') { ch } else { '_' })
        .collect()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, value: &impl Serialize) -> bool {
    let Some(parent) = path.parent() else { return false };
    if fs::create_dir_all(parent).is_err() {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec(value) else { return false };
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    if fs::write(&tmp, bytes).is_err() {
        return false;
    }
    if fs::rename(&tmp, path).is_ok() {
        true
    } else {
        let _ = fs::remove_file(path);
        let ok = fs::rename(&tmp, path).is_ok();
        let _ = fs::remove_file(tmp);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_state_round_trips_semantic_anchors() {
        let state = WorkspaceState {
            root: PathBuf::from("/repo"),
            active: View::SourceControl,
            width: 38,
            explorer: Some(ExplorerState {
                selected: Some(PathBuf::from("src/main.rs")),
                top: Some(PathBuf::from("src")),
                expanded: BTreeSet::from([PathBuf::from("src")]),
            }),
            scm: Some(ScmState {
                selected: Some(ScmAnchor::Changes {
                    repo: PathBuf::from("/repo"),
                    path: "src/main.rs".into(),
                }),
                ..ScmState::default()
            }),
            writer: "pane-1".into(),
            revision: 7,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<WorkspaceState>(&json).unwrap(), state);
    }

    #[test]
    fn old_scm_state_defaults_the_top_anchor_offset() {
        let mut json = serde_json::to_value(ScmState::default()).unwrap();
        json.as_object_mut().unwrap().remove("top_offset");
        let state = serde_json::from_value::<ScmState>(json).unwrap();
        assert_eq!(state.top_offset, 0);
    }

    #[test]
    fn visual_files_are_scoped_by_workspace_and_root() {
        let parent = PathBuf::from("/tmp/state");
        let root_a = PathBuf::from("/repo/a");
        let root_b = PathBuf::from("/repo/b");
        let name_a = format!("w_1-{:016x}.json", root_hash(&root_a));
        let name_b = format!("w_1-{:016x}.json", root_hash(&root_b));
        assert_ne!(parent.join(name_a), parent.join(name_b));
    }

    #[test]
    fn focus_destination_applies_sidebar_role_without_mapping_content_panes() {
        let panes = vec![
            Pane {
                pane_id: "sidebar".into(),
                workspace_id: "w".into(),
                tab_id: "t".into(),
                agent: None,
                focused: false,
                tokens: serde_json::json!({
                    "herdr-sidebar-explorer": "1",
                    "herdr-sidebar-git": "1"
                }),
            },
            Pane {
                pane_id: "content".into(),
                workspace_id: "w".into(),
                tab_id: "t".into(),
                agent: None,
                focused: true,
                tokens: serde_json::json!({}),
            },
        ];
        assert_eq!(focus_destination(&panes, true).as_deref(), Some("sidebar"));
        assert_eq!(focus_destination(&panes, false).as_deref(), Some("content"));

        let panes = vec![
            Pane { focused: true, ..panes[0].clone() },
            Pane { focused: false, ..panes[1].clone() },
        ];
        assert_eq!(focus_destination(&panes, false).as_deref(), Some("content"));

        let panes = vec![
            Pane {
                agent: Some("pi".into()),
                focused: true,
                ..panes[0].clone()
            },
            Pane { focused: false, ..panes[1].clone() },
        ];
        assert_eq!(focus_destination(&panes, true), None);
    }

}
