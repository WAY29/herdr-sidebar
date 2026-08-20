use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{actions, ensure, ipc, launch, state};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpenRequest {
    pub root: PathBuf,
    pub path: PathBuf,
    pub is_dir: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<LinkDiff>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinkDiff {
    pub root: PathBuf,
    pub rel: String,
    pub kind: String,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub pane_id: String,
    pub root: PathBuf,
}

pub struct Inbox {
    path: Option<PathBuf>,
    pending: Option<OpenRequest>,
}

impl Inbox {
    pub fn for_current_pane() -> Self {
        let path = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .and_then(|id| request_path(&id));
        Self {
            path,
            pending: None,
        }
    }

    pub fn poll(&mut self) -> bool {
        if self.pending.is_some() {
            return true;
        }
        let Some(path) = &self.path else {
            return false;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let _ = std::fs::remove_file(path);
        self.pending = serde_json::from_slice(&bytes)
            .ok()
            .filter(|request: &OpenRequest| safe_relative(&request.path));
        self.pending.is_some()
    }

    pub fn take(&mut self) -> Option<OpenRequest> {
        self.poll();
        self.pending.take()
    }

    pub fn peek(&mut self) -> Option<&OpenRequest> {
        self.poll();
        self.pending.as_ref()
    }

    pub fn put_back(&mut self, request: OpenRequest) {
        self.pending = Some(request);
    }
}

pub fn locate_target() -> Result<Target, String> {
    #[derive(Deserialize, Default)]
    struct Context {
        #[serde(default)]
        workspace_id: String,
        #[serde(default)]
        tab_id: String,
        #[serde(default)]
        focused_pane_cwd: Option<String>,
    }
    #[derive(Deserialize)]
    struct Message {
        result: ResultBody,
    }
    #[derive(Deserialize)]
    struct ResultBody {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        workspace_id: Option<String>,
        tab_id: Option<String>,
        cwd: Option<String>,
        foreground_cwd: Option<String>,
        #[serde(default)]
        focused: bool,
        #[serde(default)]
        agent: Option<serde_json::Value>,
        #[serde(default)]
        tokens: serde_json::Map<String, serde_json::Value>,
    }

    let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|json| serde_json::from_str::<Context>(&json).ok())
        .unwrap_or_default();
    let json = ipc::call_text("pane.list", serde_json::json!({}))
        .map_err(|error| format!("Could not inspect the current tab: {error}"))?;
    let message: Message = serde_json::from_str(json.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("Could not read the pane list: {error}"))?;
    let focused = message.result.panes.iter().find(|pane| pane.focused);
    let workspace_id = if context.workspace_id.is_empty() {
        focused.and_then(|pane| pane.workspace_id.as_deref())
    } else {
        Some(context.workspace_id.as_str())
    };
    let tab_id = if context.tab_id.is_empty() {
        focused.and_then(|pane| pane.tab_id.as_deref())
    } else {
        Some(context.tab_id.as_str())
    };
    let now = state::unix_now();
    let pane = message
        .result
        .panes
        .iter()
        .filter(|pane| pane.agent.is_none())
        .filter(|pane| pane.workspace_id.as_deref() == workspace_id)
        .filter(|pane| pane.tab_id.as_deref() == tab_id)
        .filter_map(|pane| {
            let stamp = pane
                .tokens
                .get("herdr-sidebar-explorer")
                .and_then(|value| value.as_str())
                .and_then(|value| value.parse::<u64>().ok())?;
            (now.saturating_sub(stamp) <= launch::HEARTBEAT_STALE_SECS).then_some((stamp, pane))
        })
        .max_by_key(|(stamp, _)| *stamp)
        .map(|(_, pane)| pane)
        .ok_or_else(|| "No live Explorer is open in the current tab.".to_string())?;
    let root = pane
        .foreground_cwd
        .as_deref()
        .or(pane.cwd.as_deref())
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            context
                .focused_pane_cwd
                .map(PathBuf::from)
                .filter(|path| path.is_dir())
        })
        .ok_or_else(|| "Could not determine the Explorer root folder.".to_string())?;
    Ok(Target {
        pane_id: pane
            .pane_id
            .clone()
            .ok_or_else(|| "Explorer pane has no id.".to_string())?,
        root: std::fs::canonicalize(&root).unwrap_or(root),
    })
}

pub fn send_request(
    target: &Target,
    path: &Path,
    is_dir: bool,
    line: Option<usize>,
) -> Result<(), String> {
    send_request_inner(target, path, is_dir, line, None)
}

fn send_request_inner(
    target: &Target,
    path: &Path,
    is_dir: bool,
    line: Option<usize>,
    diff: Option<LinkDiff>,
) -> Result<(), String> {
    let request = OpenRequest {
        root: target.root.clone(),
        path: path.to_path_buf(),
        is_dir,
        line,
        diff,
    };
    let path = request_path(&target.pane_id)
        .ok_or_else(|| "Could not resolve the Sidebar state directory.".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "Quick Open request path has no parent.".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create the Quick Open mailbox: {error}"))?;
    let bytes = serde_json::to_vec(&request)
        .map_err(|error| format!("Could not encode the Quick Open request: {error}"))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)
        .map_err(|error| format!("Could not write the Quick Open request: {error}"))?;
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp, &path)
            .map_err(|error| format!("Could not publish the Quick Open request: {error}"))?;
    }
    Ok(())
}

pub fn open_clicked_link() -> std::io::Result<()> {
    open_clicked_link_inner().map_err(std::io::Error::other)
}

fn open_clicked_link_inner() -> Result<(), String> {
    let raw = std::env::var("HERDR_PLUGIN_CLICKED_URL")
        .map_err(|_| "Link handler received no URL.".to_string())?;
    let link = parse_link(&raw)?;
    let path = std::fs::canonicalize(&link.path)
        .map_err(|error| format!("Could not open {}: {error}", link.path.display()))?;
    let target = locate_target().or_else(|_| {
        ensure::show().map_err(|error| error.to_string())?;
        locate_target()
    });

    if let Ok(target) = target {
        let root = std::fs::canonicalize(&target.root).unwrap_or_else(|_| target.root.clone());
        if let Ok(relative) = path.strip_prefix(&root)
            && !relative.as_os_str().is_empty()
        {
            let is_dir = path.is_dir();
            let diff = (!is_dir).then(|| link_diff(&path)).flatten();
            return send_request_inner(
                &target,
                relative,
                is_dir,
                if is_dir { None } else { link.line },
                diff,
            );
        }
    }

    actions::open_external(&path).map_err(|error| error.to_string())
}

fn link_diff(path: &Path) -> Option<LinkDiff> {
    let git = crate::git::Git::discover(path.parent()?).ok()?;
    let relative = path.strip_prefix(git.root()).ok()?;
    let rel = relative.to_string_lossy().replace('\\', "/");
    let status = git.status().ok()?;
    let kind = preferred_diff_kind(&status, &rel)?;
    Some(LinkDiff {
        root: git.root().to_path_buf(),
        rel,
        kind: kind.into(),
    })
}

fn preferred_diff_kind(status: &crate::git::Status, rel: &str) -> Option<&'static str> {
    if let Some(entry) = status.unstaged.iter().find(|entry| entry.path == rel) {
        Some(if entry.letter == 'U' { "untracked" } else { "worktree" })
    } else {
        status.staged.iter().any(|entry| entry.path == rel).then_some("staged")
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Link {
    path: PathBuf,
    line: Option<usize>,
}

fn parse_link(raw: &str) -> Result<Link, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Empty file link.".to_string());
    }
    let (resource, fragment) = raw
        .split_once('#')
        .map_or((raw, None), |(resource, fragment)| {
            (resource, Some(fragment))
        });
    let fragment_line = fragment.and_then(parse_line_hint);
    let (path, query_line) = if let Some(rest) = resource.strip_prefix("file://") {
        parse_file_url_path(rest)?
    } else {
        (PathBuf::from(resource), None)
    };
    if !path.is_absolute() {
        return Err(format!("File link is not absolute: {}", path.display()));
    }
    if path.exists() || fragment_line.is_some() || query_line.is_some() {
        return Ok(Link {
            path,
            line: fragment_line.or(query_line),
        });
    }
    let Some((path, line)) = path_line_suffix(&path) else {
        return Ok(Link {
            path,
            line: fragment_line.or(query_line),
        });
    };
    Ok(Link {
        path,
        line: Some(line),
    })
}

fn parse_file_url_path(rest: &str) -> Result<(PathBuf, Option<usize>), String> {
    let (resource, query) = rest
        .split_once('?')
        .map_or((rest, None), |(resource, query)| (resource, Some(query)));
    let slash = resource.find('/').unwrap_or(resource.len());
    let authority = &resource[..slash];
    let decoded = percent_decode(&resource[slash..])?;
    let query_line = query.and_then(|query| {
        query.split('&').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            if key.eq_ignore_ascii_case("line") {
                parse_line_hint(value)
            } else {
                None
            }
        })
    });

    #[cfg(windows)]
    let path = {
        let decoded = decoded.replace('/', "\\");
        if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
            let local = decoded
                .strip_prefix('\\')
                .filter(|path| path.as_bytes().get(1) == Some(&b':'))
                .unwrap_or(&decoded);
            PathBuf::from(local)
        } else {
            PathBuf::from(format!("\\\\{authority}{decoded}"))
        }
    };
    #[cfg(not(windows))]
    let path = {
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return Err(format!("Remote file URL is not supported: {authority}"));
        }
        PathBuf::from(decoded)
    };

    Ok((path, query_line))
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes
                .get(index + 1)
                .and_then(|byte| hex(*byte))
                .ok_or_else(|| "Invalid percent escape in file URL.".to_string())?;
            let low = bytes
                .get(index + 2)
                .and_then(|byte| hex(*byte))
                .ok_or_else(|| "Invalid percent escape in file URL.".to_string())?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    if decoded.contains(&0) {
        return Err("File URL contains a NUL byte.".to_string());
    }
    String::from_utf8(decoded).map_err(|_| "File URL path is not UTF-8.".to_string())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_line_hint(value: &str) -> Option<usize> {
    let value = value
        .strip_prefix("line=")
        .or_else(|| value.strip_prefix("Line="))
        .unwrap_or(value);
    let value = value
        .strip_prefix('L')
        .or_else(|| value.strip_prefix('l'))
        .unwrap_or(value);
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|line| *line > 0)
}

fn path_line_suffix(path: &Path) -> Option<(PathBuf, usize)> {
    let value = path.to_str()?;
    let (prefix, tail) = value.rsplit_once(':')?;
    if tail.parse::<usize>().is_ok()
        && let Some((path, line)) = prefix.rsplit_once(':')
        && let Some(line) = line.parse::<usize>().ok().filter(|line| *line > 0)
    {
        return Some((PathBuf::from(path), line));
    }
    let line = tail.parse::<usize>().ok().filter(|line| *line > 0)?;
    Some((PathBuf::from(prefix), line))
}

fn request_path(pane_id: &str) -> Option<PathBuf> {
    Some(
        state::state_dir()?
            .join("quick-open")
            .join(format!("{}.json", safe_component(pane_id))),
    )
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_are_per_pane() {
        assert_ne!(safe_component("pane:a"), safe_component("pane:b"));
        assert_eq!(safe_component("pane:a"), "pane_a");
    }

    #[test]
    fn old_mailbox_requests_default_to_no_line() {
        let request: OpenRequest =
            serde_json::from_str(r#"{"root":"/repo","path":"src/main.rs","is_dir":false}"#)
                .unwrap();
        assert_eq!(request.line, None);
        assert!(request.diff.is_none());
    }

    fn status_entry(path: &str, letter: char) -> crate::git::FileEntry {
        crate::git::FileEntry {
            path: path.into(),
            orig: None,
            letter,
            stat: crate::git::DiffStat::default(),
        }
    }

    #[test]
    fn hyperlink_diff_prefers_worktree_then_untracked_then_staged() {
        let mut status = crate::git::Status::default();
        status.staged.push(status_entry("both.rs", 'M'));
        status.unstaged.push(status_entry("both.rs", 'M'));
        status.unstaged.push(status_entry("new.rs", 'U'));
        status.staged.push(status_entry("staged.rs", 'A'));

        assert_eq!(preferred_diff_kind(&status, "both.rs"), Some("worktree"));
        assert_eq!(preferred_diff_kind(&status, "new.rs"), Some("untracked"));
        assert_eq!(preferred_diff_kind(&status, "staged.rs"), Some("staged"));
        assert_eq!(preferred_diff_kind(&status, "clean.rs"), None);
    }

    #[test]
    fn rejects_paths_that_escape_the_root() {
        assert!(safe_relative(Path::new("src/main.rs")));
        assert!(!safe_relative(Path::new("../secret")));
        assert!(!safe_relative(Path::new("/tmp/secret")));
    }

    #[test]
    fn parses_file_url_fragment_and_percent_encoding() {
        let link = parse_link("file:///tmp/a%20file.rs#L42C7").unwrap();
        assert_eq!(link.path, PathBuf::from("/tmp/a file.rs"));
        assert_eq!(link.line, Some(42));
    }

    #[test]
    fn parses_file_url_line_query() {
        let link = parse_link("file:///tmp/main.rs?line=17").unwrap();
        assert_eq!(link.path, PathBuf::from("/tmp/main.rs"));
        assert_eq!(link.line, Some(17));
    }

    #[test]
    fn parses_absolute_path_line_and_column_suffix() {
        let link = parse_link("/tmp/main.rs:23:9").unwrap();
        assert_eq!(link.path, PathBuf::from("/tmp/main.rs"));
        assert_eq!(link.line, Some(23));
    }

    #[cfg(not(windows))]
    #[test]
    fn rejects_remote_file_url() {
        assert!(parse_link("file://server/share/main.rs").is_err());
    }
}
