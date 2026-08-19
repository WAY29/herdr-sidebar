use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use serde::{Deserialize, Serialize};

use herdr_sidebar::icons::{IconTheme, icon};
use herdr_sidebar::ui::{draw_scrollbar, hits, icon_button_style, input_tail, truncate_to};
use herdr_sidebar::{ipc, launch, state};

const MAX_RESULTS: usize = 500;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenRequest {
    pub root: PathBuf,
    pub path: PathBuf,
    pub is_dir: bool,
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
        let Some(path) = &self.path else { return false };
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
}

#[derive(Clone, Debug)]
struct Target {
    pane_id: String,
    root: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    is_dir: bool,
}

struct App {
    target: Option<Target>,
    root: PathBuf,
    query: String,
    entries: Vec<Entry>,
    results: Vec<usize>,
    match_count: usize,
    selected: usize,
    scroll: usize,
    page: usize,
    body: Rect,
    retry_button: Rect,
    mouse_pos: Option<(u16, u16)>,
    indexing: bool,
    index_rx: Option<Receiver<Result<Vec<Entry>, String>>>,
    error: Option<String>,
    theme: IconTheme,
}

impl App {
    fn new() -> Self {
        let sidebar_state = state::load_state();
        let theme = IconTheme::resolve(
            std::env::var("HERDR_SIDEBAR_ICONS").ok().as_deref(),
            sidebar_state.icons,
        );
        let mut app = Self {
            target: None,
            root: std::env::current_dir().unwrap_or_default(),
            query: String::new(),
            entries: Vec::new(),
            results: Vec::new(),
            match_count: 0,
            selected: 0,
            scroll: 0,
            page: 1,
            body: Rect::default(),
            retry_button: Rect::default(),
            mouse_pos: None,
            indexing: false,
            index_rx: None,
            error: None,
            theme,
        };
        app.retry();
        app
    }

    fn retry(&mut self) {
        self.error = None;
        match locate_target() {
            Ok(target) => {
                self.root.clone_from(&target.root);
                self.target = Some(target);
                self.start_index();
            }
            Err(error) => {
                self.target = None;
                self.indexing = false;
                self.index_rx = None;
                self.error = Some(error);
            }
        }
    }

    fn start_index(&mut self) {
        self.entries.clear();
        self.results.clear();
        self.match_count = 0;
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
        self.indexing = true;
        let root = self.root.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(index_entries(&root));
        });
        self.index_rx = Some(rx);
    }

    fn poll_index(&mut self) {
        let Some(rx) = &self.index_rx else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.index_rx = None;
        self.indexing = false;
        match result {
            Ok(entries) => {
                self.entries = entries;
                self.refresh_results();
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn refresh_results(&mut self) {
        let query = self.query.trim();
        let mut ranked = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(rank) = entry_rank(entry, query) {
                ranked.push((rank, entry.path.to_string_lossy().to_lowercase(), index));
            }
        }
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        self.match_count = ranked.len();
        self.results = ranked
            .into_iter()
            .take(MAX_RESULTS)
            .map(|(_, _, index)| index)
            .collect();
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let max = self.results.len() - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(max)
        };
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + self.page {
            self.scroll = self.selected + 1 - self.page;
        }
    }

    fn activate(&mut self) -> bool {
        let Some(target) = self.target.clone() else {
            return false;
        };
        let Some(entry) = self
            .results
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
            .cloned()
        else {
            return false;
        };
        match send_request(&target, &entry) {
            Ok(()) => true,
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return true;
        }
        if matches!(key.code, KeyCode::Esc)
            || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return false;
        }
        if self.error.is_some() {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('r')) {
                self.retry();
            }
            return true;
        }
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-(self.page as isize)),
            KeyCode::PageDown => self.move_selection(self.page as isize),
            KeyCode::Home => self.move_selection(-(self.results.len() as isize)),
            KeyCode::End => self.move_selection(self.results.len() as isize),
            KeyCode::Enter if !self.indexing => return !self.activate(),
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh_results();
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                self.query.clear();
                self.refresh_results();
            }
            KeyCode::Char('n') if key.modifiers == KeyModifiers::CONTROL => self.move_selection(1),
            KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => self.move_selection(-1),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.query.push(ch);
                self.refresh_results();
            }
            _ => {}
        }
        true
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> bool {
        self.mouse_pos = Some((mouse.column, mouse.row));
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                let max = self.results.len().saturating_sub(self.page);
                self.scroll = self.scroll.saturating_add(3).min(max);
            }
            MouseEventKind::Down(MouseButton::Left) if self.error.is_some() => {
                if hits(self.retry_button, mouse.column, mouse.row) {
                    self.retry();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if hits(self.body, mouse.column, mouse.row) => {
                let index = self.scroll + usize::from(mouse.row.saturating_sub(self.body.y));
                if index < self.results.len() {
                    self.selected = index;
                    return !self.activate();
                }
            }
            _ => {}
        }
        true
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [input, status, body] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);
        self.draw_input(frame, input);
        self.draw_status(frame, status);
        if let Some(error) = self.error.clone() {
            self.draw_error(frame, body, &error);
        } else if self.indexing {
            self.body = Rect::default();
            frame.render_widget(
                Paragraph::new("Indexing files…")
                    .alignment(Alignment::Center)
                    .style(Style::default().dim()),
                body,
            );
        } else {
            self.draw_results(frame, body);
        }
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().border_style(Style::default().fg(Color::LightBlue));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let available = usize::from(inner.width.saturating_sub(2));
        let query = input_tail(&self.query, available);
        let query_width = Span::raw(query.as_str()).width() as u16;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::LightBlue).bold()),
                Span::raw(query),
            ])),
            inner,
        );
        frame.set_cursor_position(Position::new(
            inner.x + 2 + query_width.min(inner.width.saturating_sub(2)),
            inner.y,
        ));
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let count = if self.indexing {
            "indexing".to_string()
        } else if self.match_count > self.results.len() {
            format!(
                "{} matches · showing {}",
                self.match_count,
                self.results.len()
            )
        } else {
            format!("{} matches", self.match_count)
        };
        let reserve = Span::raw(count.as_str()).width().saturating_add(3);
        let root = truncate_to(
            self.root.display().to_string(),
            usize::from(area.width).saturating_sub(reserve),
        );
        let gap = usize::from(area.width)
            .saturating_sub(Span::raw(root.as_str()).width())
            .saturating_sub(Span::raw(count.as_str()).width());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(root, Style::default().dim()),
                Span::raw(" ".repeat(gap)),
                Span::styled(count, Style::default().dim()),
            ])),
            area,
        );
    }

    fn draw_results(&mut self, frame: &mut Frame, area: Rect) {
        self.page = usize::from(area.height.max(1));
        self.scroll = self
            .scroll
            .min(self.results.len().saturating_sub(self.page));
        self.body = area;
        if self.results.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching files or folders")
                    .alignment(Alignment::Center)
                    .style(Style::default().dim()),
                area,
            );
            return;
        }
        let items = self
            .results
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(self.page)
            .filter_map(|(result_index, entry_index)| {
                self.entries.get(*entry_index).map(|entry| {
                    result_item(
                        entry,
                        self.theme,
                        result_index == self.selected,
                        self.mouse_pos.is_some_and(|(x, y)| {
                            hits(area, x, y)
                                && self.scroll + usize::from(y.saturating_sub(area.y))
                                    == result_index
                        }),
                        usize::from(area.width.saturating_sub(1)),
                    )
                })
            })
            .collect::<Vec<_>>();
        frame.render_widget(List::new(items), area);
        draw_scrollbar(frame, area, self.results.len(), self.page, self.scroll);
    }

    fn draw_error(&mut self, frame: &mut Frame, area: Rect, error: &str) {
        self.body = Rect::default();
        let needs_rg = error.contains("ripgrep") || error.contains("`rg`");
        let title = if needs_rg {
            "Quick Open needs ripgrep (`rg`)."
        } else {
            "Quick Open is unavailable."
        };
        let width = area.width.clamp(20, 52);
        let height = 6.min(area.height);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 3,
            width,
            height,
        );
        let inner = popup.inner(Margin::new(1, 1));
        let [text_area, button_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        let button_label = " Retry ";
        let button_width = Span::raw(button_label).width() as u16;
        self.retry_button = Rect::new(
            button_area.x + button_area.width.saturating_sub(button_width) / 2,
            button_area.y,
            button_width,
            1,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Block::bordered().border_style(Style::default().dim()),
            popup,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(title, Style::default().bold())),
                Line::from(Span::styled(error.to_string(), Style::default().dim())),
            ])
            .alignment(Alignment::Center),
            text_area,
        );
        let hovered = self
            .mouse_pos
            .is_some_and(|(x, y)| hits(self.retry_button, x, y));
        frame.render_widget(
            Paragraph::new(Span::styled(button_label, icon_button_style(hovered, true)))
                .alignment(Alignment::Center),
            self.retry_button,
        );
    }
}

pub fn run() -> std::io::Result<()> {
    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let mut app = App::new();
    let result = loop {
        app.poll_index();
        terminal.draw(|frame| app.draw(frame))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let keep_open = match event::read()? {
            Event::Key(key) => app.on_key(key),
            Event::Mouse(mouse) => app.on_mouse(mouse),
            _ => true,
        };
        if !keep_open {
            break Ok(());
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn result_item(
    entry: &Entry,
    theme: IconTheme,
    selected: bool,
    hovered: bool,
    width: usize,
) -> ListItem<'static> {
    let name = entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.path.display().to_string());
    let icon = icon(theme, &name, entry.is_dir, false);
    let icon_style = icon.rgb.map_or_else(Style::default, |(r, g, b)| {
        Style::default().fg(Color::Rgb(r, g, b))
    });
    let parent = entry
        .path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let icon_width = Span::raw(icon.glyph).width().saturating_add(1);
    let available = width.saturating_sub(icon_width);
    let name_limit = available.saturating_mul(3).saturating_div(5).max(1);
    let shown_name = truncate_to(
        if entry.is_dir {
            format!("{name}/")
        } else {
            name
        },
        name_limit,
    );
    let name_width = Span::raw(shown_name.as_str()).width();
    let suffix_width = available.saturating_sub(name_width).saturating_sub(2);
    let shown_parent = truncate_to(parent, suffix_width);
    let mut spans = vec![
        Span::styled(icon.glyph, icon_style),
        Span::raw(" "),
        Span::styled(shown_name, Style::default().bold()),
    ];
    if !shown_parent.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(shown_parent, Style::default().dim()));
    }
    let style = if selected {
        Style::default().bg(Color::DarkGray)
    } else if hovered {
        Style::default().bg(herdr_sidebar::ui::KEYCAP_BG)
    } else {
        Style::default()
    };
    ListItem::new(Line::from(spans)).style(style)
}

fn index_entries(root: &Path) -> Result<Vec<Entry>, String> {
    let output = Command::new("rg")
        .args(["--files", "--hidden", "--null", "--glob=!.git/**"])
        .current_dir(root)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "ripgrep (`rg`) is not available.".to_string()
            } else {
                format!("Failed to start ripgrep: {error}")
            }
        })?;
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("ripgrep exited with {}", output.status)
        } else {
            stderr
        });
    }
    entries_from_rg(&output.stdout)
}

fn entries_from_rg(output: &[u8]) -> Result<Vec<Entry>, String> {
    let mut paths = BTreeMap::new();
    for raw in output
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
    {
        let path = PathBuf::from(String::from_utf8_lossy(raw).into_owned());
        if !safe_relative(&path) {
            continue;
        }
        // ponytail: directories are derived from rg's file list; use an
        // ignore-aware walker if searchable empty directories become necessary.
        for parent in path
            .ancestors()
            .skip(1)
            .filter(|path| !path.as_os_str().is_empty())
        {
            paths.entry(parent.to_path_buf()).or_insert(true);
        }
        paths.insert(path, false);
    }
    Ok(paths
        .into_iter()
        .map(|(path, is_dir)| Entry { path, is_dir })
        .collect())
}

fn entry_rank(entry: &Entry, query: &str) -> Option<(u8, usize, usize, usize, usize)> {
    if query.is_empty() {
        return Some((2, 0, 0, entry.path.as_os_str().len(), entry.is_dir as usize));
    }
    let display = entry.path.to_string_lossy();
    let path_rank = fuzzy_rank(&display, query).map(|(gaps, start, len)| (1, gaps, start, len, 0));
    let name_rank = entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy())
        .and_then(|name| fuzzy_rank(&name, query))
        .map(|(gaps, start, len)| (0, gaps, start, len, 0));
    name_rank.or(path_rank)
}

fn fuzzy_rank(candidate: &str, query: &str) -> Option<(usize, usize, usize)> {
    let candidate = candidate.to_lowercase().chars().collect::<Vec<_>>();
    let query = query.to_lowercase().chars().collect::<Vec<_>>();
    if query.is_empty() {
        return Some((0, 0, candidate.len()));
    }
    let mut positions = Vec::with_capacity(query.len());
    let mut from = 0;
    for wanted in query {
        let offset = candidate.get(from..)?.iter().position(|ch| *ch == wanted)?;
        let position = from + offset;
        positions.push(position);
        from = position + 1;
    }
    let gaps = positions
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0] + 1))
        .sum();
    Some((gaps, positions[0], candidate.len()))
}

fn locate_target() -> Result<Target, String> {
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

fn send_request(target: &Target, entry: &Entry) -> Result<(), String> {
    let request = OpenRequest {
        root: target.root.clone(),
        path: entry.path.clone(),
        is_dir: entry.is_dir,
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

fn request_path(pane_id: &str) -> Option<PathBuf> {
    let safe = pane_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    Some(
        state::state_dir()?
            .join("quick-open")
            .join(format!("{safe}.json")),
    )
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_prefers_basename_and_consecutive_letters() {
        let direct = Entry {
            path: PathBuf::from("src/main.rs"),
            is_dir: false,
        };
        let scattered = Entry {
            path: PathBuf::from("main/src/thing.rs"),
            is_dir: false,
        };
        assert!(entry_rank(&direct, "main") < entry_rank(&scattered, "main"));
        assert_eq!(fuzzy_rank("src/main.rs", "smr"), Some((7, 0, 11)));
        assert_eq!(fuzzy_rank("src/main.rs", "xyz"), None);
    }

    #[test]
    fn rg_files_also_create_searchable_parent_directories() {
        let entries = entries_from_rg(b"src/main.rs\0src/api/mod.rs\0").unwrap();
        assert!(entries.contains(&Entry {
            path: PathBuf::from("src"),
            is_dir: true
        }));
        assert!(entries.contains(&Entry {
            path: PathBuf::from("src/api"),
            is_dir: true
        }));
        assert!(entries.contains(&Entry {
            path: PathBuf::from("src/main.rs"),
            is_dir: false
        }));
    }

    #[test]
    fn mailbox_rejects_paths_that_escape_the_root() {
        assert!(safe_relative(Path::new("src/main.rs")));
        assert!(!safe_relative(Path::new("../secret")));
        assert!(!safe_relative(Path::new("/tmp/secret")));
    }
}
