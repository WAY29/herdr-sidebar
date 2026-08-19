//! TUI state and rendering for VS Code-style text search in the unified
//! Sidebar pane: query/include/exclude inputs, regex/case/whole-word toggles,
//! grouped streaming results, and preview opening on a clicked match.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use herdr_sidebar::icons::{IconTheme, icon};
use herdr_sidebar::state::Exit;
use herdr_sidebar::state::{self as sidebar, View};
use herdr_sidebar::syntax;
use herdr_sidebar::ui::{
    KEYCAP_BG, activity_button_style, activity_icons, draw_option_picker, draw_scrollbar,
    gear_icon, hits, hits_collapse_button, icon_button_style, input_tail, option_picker_index,
    redraw_button, truncate_to, wrap_footer_message, wrap_hints,
};
use herdr_sidebar::viewer::{SEARCH_HIGHLIGHT_BG, SEARCH_HIGHLIGHT_FG};
use herdr_sidebar::workspace_sync::SearchState;

const MY_VIEW: View = View::Search;
const DEFAULT_EXPANDED_WIDTH: u16 = 32;
const APP_BG: Color = Color::Rgb(0x1f, 0x22, 0x33);
const SURFACE_BG: Color = Color::Rgb(0x3b, 0x40, 0x56);
const SURFACE_BG_MUTED: Color = Color::Rgb(0x2a, 0x2f, 0x43);
const SURFACE_BORDER: Color = Color::Rgb(0x70, 0x75, 0x8f);
const ACCENT: Color = Color::Rgb(0xd5, 0xae, 0xff);
const TEXT_MAIN: Color = Color::Rgb(0xe7, 0xeb, 0xf7);
const TEXT_MUTED: Color = Color::Rgb(0xac, 0xb3, 0xc8);
const HOVER_BG: Color = Color::Rgb(0x30, 0x35, 0x49);
const MATCH_BG: Color = SEARCH_HIGHLIGHT_BG;
const MATCH_FG: Color = SEARCH_HIGHLIGHT_FG;
const COUNT_BG: Color = Color::Rgb(0x4c, 0x50, 0x67);
const COUNT_FG: Color = Color::Rgb(0xf0, 0xf3, 0xfa);
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

struct PaneCtl {
    pane_id: String,
}

impl PaneCtl {
    fn from_env() -> Option<Self> {
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty())?;
        Some(Self { pane_id })
    }

    fn report_tokens(&self, my: View, merged: bool) {
        herdr_sidebar::ipc::report_identity(&self.pane_id, my, merged);
    }

    fn set_label(&self, label: Option<&str>) {
        let mut params = serde_json::json!({ "pane_id": self.pane_id });
        if let Some(label) = label {
            params["label"] = serde_json::Value::String(label.to_string());
        }
        let _ = herdr_sidebar::ipc::call_text("pane.rename", params);
    }

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
}

#[derive(Clone, Copy, Default)]
struct BodyGeom {
    top: u16,
    height: u16,
    offset: usize,
}

#[derive(Clone, Copy)]
struct ActivityZones {
    row: u16,
    explorer: (u16, u16),
    source_control: (u16, u16),
    search: (u16, u16),
}

impl Default for ActivityZones {
    fn default() -> Self {
        Self {
            row: u16::MAX,
            explorer: (0, 0),
            source_control: (0, 0),
            search: (0, 0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Query,
    Include,
    Exclude,
    Toggles,
    Results,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToggleKind {
    Regex,
    Case,
    WholeWord,
}

const SEARCH_TOGGLES: [ToggleKind; 3] =
    [ToggleKind::Case, ToggleKind::WholeWord, ToggleKind::Regex];

#[derive(Clone, Copy)]
enum FilterField {
    Include,
    Exclude,
}

impl FilterField {
    fn focus(self) -> Focus {
        match self {
            Self::Include => Focus::Include,
            Self::Exclude => Focus::Exclude,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Include => "Include files",
            Self::Exclude => "Exclude files",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Include => "e.g. *.ts, src/**/include",
            Self::Exclude => "e.g. *.ts, src/**/exclude",
        }
    }
}

enum Overlay {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    UnifiedSidebar,
    IconTheme,
    DiffTheme,
    HideUnmodified,
    AutoOpen,
    Hotkeys,
}

type SettingRow = (Setting, &'static str, String, bool);

#[derive(Clone, Debug)]
struct SearchMatch {
    line_number: usize,
    line_text: String,
    submatches: Vec<(usize, usize)>,
}

#[derive(Clone, Debug)]
struct SearchGroup {
    path: String,
    matches: Vec<SearchMatch>,
}

#[derive(Clone, Copy)]
enum DisplayRow {
    Header(usize),
    Match { group: usize, item: usize },
}

impl DisplayRow {
    fn selectable(self) -> bool {
        matches!(self, Self::Match { .. })
    }
}

#[derive(Clone)]
struct UiMatch {
    path: String,
    line_number: usize,
    line_text: String,
    submatches: Vec<(usize, usize)>,
}

enum SearchEvent {
    Match(UiMatch),
    Finished,
}

struct RunningSearch {
    child: Child,
    rx: Receiver<SearchEvent>,
}

pub struct App {
    root: PathBuf,
    pane_ctl: Option<PaneCtl>,
    sidebar_state: sidebar::State,
    theme: IconTheme,
    last_width: u16,
    last_height: u16,
    page: usize,
    body: BodyGeom,
    activity: ActivityZones,
    gear: Rect,
    redraw: Rect,
    redraw_requested: bool,
    search_button: Rect,
    clear_button: Rect,
    retry_button: Rect,
    field_query: Rect,
    field_include: Rect,
    field_exclude: Rect,
    filters_toggle: Rect,
    toggle_rects: Vec<(Rect, ToggleKind)>,
    focus: Focus,
    toggle_focus: usize,
    overlay: Option<Overlay>,
    query: String,
    include: String,
    exclude: String,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    filters_collapsed: bool,
    groups: Vec<SearchGroup>,
    rows: Vec<DisplayRow>,
    selected: Option<usize>,
    hovered: Option<usize>,
    scroll: usize,
    snap: bool,
    last_mouse: Option<std::time::Instant>,
    mouse_pos: Option<(u16, u16)>,
    last_beat: std::time::Instant,
    pending_search: Option<std::time::Instant>,
    running: Option<RunningSearch>,
    searching: bool,
    summary: Option<String>,
    inline_error: Option<String>,
    notice: Option<String>,
    rg_error: Option<String>,
    pending_exit: Option<Exit>,
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        let sidebar_state = sidebar::load_state();
        let theme = IconTheme::resolve(
            std::env::var("HERDR_SIDEBAR_ICONS")
                .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                .ok()
                .as_deref(),
            sidebar_state.icons,
        );
        let pane_ctl = if cfg!(test) {
            None
        } else {
            PaneCtl::from_env()
        };
        let app = Self {
            root,
            pane_ctl,
            sidebar_state,
            theme,
            last_width: DEFAULT_EXPANDED_WIDTH,
            last_height: 24,
            page: 20,
            body: BodyGeom::default(),
            activity: ActivityZones::default(),
            gear: Rect::default(),
            redraw: Rect::default(),
            redraw_requested: false,
            search_button: Rect::default(),
            clear_button: Rect::default(),
            retry_button: Rect::default(),
            field_query: Rect::default(),
            field_include: Rect::default(),
            field_exclude: Rect::default(),
            filters_toggle: Rect::default(),
            toggle_rects: Vec::new(),
            focus: Focus::Query,
            toggle_focus: 0,
            overlay: None,
            query: String::new(),
            include: String::new(),
            exclude: String::new(),
            regex: false,
            case_sensitive: false,
            whole_word: false,
            filters_collapsed: false,
            groups: Vec::new(),
            rows: Vec::new(),
            selected: None,
            hovered: None,
            scroll: 0,
            snap: false,
            last_mouse: None,
            mouse_pos: None,
            last_beat: std::time::Instant::now(),
            pending_search: None,
            running: None,
            searching: false,
            summary: None,
            inline_error: None,
            notice: None,
            rg_error: rg_available().err(),
            pending_exit: None,
        };
        if app.pane_ctl.is_some() {
            app.apply_identity();
        }
        app
    }

    pub fn heartbeat(&mut self) {
        if self.last_beat.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        self.last_beat = std::time::Instant::now();
        if let Some(ctl) = &self.pane_ctl {
            ctl.report_tokens(MY_VIEW, self.merged());
        }
    }

    fn merged(&self) -> bool {
        self.sidebar_state.merged
    }

    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn root(&self) -> PathBuf {
        self.root.clone()
    }

    pub fn workspace_width(&self) -> u16 {
        self.last_width
    }

    pub fn workspace_sync_enabled(&self) -> bool {
        self.merged()
    }

    pub fn workspace_state(&self) -> SearchState {
        SearchState {
            query: self.query.clone(),
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
        }
    }

    pub fn apply_workspace_state(&mut self, state: &SearchState) {
        let current = self.workspace_state();
        if &current == state {
            return;
        }
        self.query.clone_from(&state.query);
        self.include.clone_from(&state.include);
        self.exclude.clone_from(&state.exclude);
        self.regex = state.regex;
        self.case_sensitive = state.case_sensitive;
        self.whole_word = state.whole_word;
        self.inline_error = None;
        self.clear_results();
    }

    pub fn apply_workspace_width(&self, width: u16) {
        if width > 0
            && width != self.last_width
            && let Some(ctl) = &self.pane_ctl
        {
            ctl.resize_to(self.last_width, width);
        }
    }

    pub fn take_redraw_request(&mut self) -> bool {
        std::mem::take(&mut self.redraw_requested)
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.draw_in(frame, area);
    }

    pub fn draw_in(&mut self, frame: &mut Frame, area: Rect) {
        self.last_width = area.width;
        self.last_height = area.height;
        frame.render_widget(Block::default().style(Style::default().bg(APP_BG)), area);
        let footer_height = self.footer_height(area.width);
        let activity_height = if self.merged() { 3 } else { 0 };
        let include_height = if self.filters_collapsed { 0 } else { 3 };
        let exclude_height = if self.filters_collapsed { 0 } else { 3 };
        let [
            activity,
            header,
            query,
            filters,
            include,
            exclude,
            status,
            body,
            footer,
        ] = Layout::vertical([
            Constraint::Length(activity_height),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(include_height),
            Constraint::Length(exclude_height),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .areas(area);
        self.page = body.height.saturating_sub(1).max(1) as usize;

        if self.merged() {
            self.draw_activity_bar(frame, activity);
        }
        self.draw_header(frame, header);
        let query_value = self.query.clone();
        let include_value = self.include.clone();
        let exclude_value = self.exclude.clone();
        self.draw_query_box(frame, query, &query_value, self.rg_error.is_none());
        self.draw_filters_toggle(frame, filters);
        if !self.filters_collapsed {
            self.draw_filter_box(
                frame,
                include,
                FilterField::Include,
                &include_value,
                self.rg_error.is_none(),
            );
            self.draw_filter_box(
                frame,
                exclude,
                FilterField::Exclude,
                &exclude_value,
                self.rg_error.is_none(),
            );
        } else {
            self.field_include = Rect::default();
            self.field_exclude = Rect::default();
        }
        self.draw_status(frame, status);
        if self.rg_error.is_some() {
            self.draw_rg_error(frame, body);
        } else {
            self.draw_results(frame, body);
        }
        self.draw_footer(frame, footer);
        match self.overlay {
            Some(Overlay::Settings { .. }) => self.draw_settings(frame),
            Some(Overlay::ThemePicker { .. }) => self.draw_theme_picker(frame),
            None => {}
        }
    }

    fn pane_label(&self) -> &'static str {
        if self.merged() {
            sidebar::SIDEBAR_LABEL
        } else {
            "Search"
        }
    }

    fn apply_identity(&self) {
        let Some(ctl) = &self.pane_ctl else { return };
        ctl.set_label(Some(self.pane_label()));
        ctl.report_tokens(MY_VIEW, self.merged());
    }

    fn footer_height(&self, width: u16) -> u16 {
        if self.notice.is_some() {
            return wrap_footer_message(self.notice.as_deref().unwrap_or(""), width, 4).len()
                as u16;
        }
        if self.overlay.is_some() || !self.sidebar_state.show_hotkeys {
            return 1;
        }
        wrap_hints(&self.hints(), width, 3).len() as u16
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("tab", "next field"),
            ("⏎", "search/open"),
            ("↑↓", "results"),
            ("1 2 3", "views"),
            ("s", "settings"),
            ("b", "hide"),
        ]
    }

    fn draw_activity_bar(&mut self, frame: &mut Frame, area: Rect) {
        let outer_top = area.y;
        let outer_bottom = area.y + 2;
        let area = Rect::new(area.x, area.y + 1, area.width, 1);
        let (exp_icon, git_icon, search_icon) = activity_icons(self.theme);
        let slack = if self.theme == IconTheme::Material {
            " "
        } else {
            ""
        };
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
            Span::styled(exp_chip, activity_button_style(false, hovered((exp_start, exp_end)))),
            Span::raw(" "),
            Span::styled(git_chip, activity_button_style(false, hovered((git_start, git_end)))),
            Span::raw(" "),
            Span::styled(
                search_chip,
                activity_button_style(true, hovered((search_start, search_end))),
            ),
        ];
        self.activity = ActivityZones {
            row: area.y,
            explorer: (exp_start, exp_end),
            source_control: (git_start, git_end),
            search: (search_start, search_end),
        };
        let (chip_start, chip_end) = self.activity.search;
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
        self.gear = Rect::new(gear_x, area.y, gear_w, 1);
        let gear = Span::styled(
            gear_chip,
            icon_button_style(
                self.mouse_pos.is_some_and(|(x, y)| hits(self.gear, x, y)),
                true,
            ),
        );
        let redraw_x = gear_x.saturating_sub(3);
        let (redraw, redraw_rect) = redraw_button(self.theme, redraw_x, area.y, self.mouse_pos);
        self.redraw = redraw_rect;
        let pad = usize::from(area.width)
            .saturating_sub(spans.iter().map(Span::width).sum::<usize>() + 3 + usize::from(gear_w));
        let mut line = spans.to_vec();
        line.push(Span::raw(" ".repeat(pad)));
        line.push(redraw);
        line.push(gear);
        frame.render_widget(Paragraph::new(Line::from(line)), area);
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        let title = Span::styled(" Search", Style::default().bold().fg(ACCENT));
        let gear = (!self.merged()).then(|| format!("{} ", gear_icon(self.theme)));
        let gear_w = gear
            .as_ref()
            .map(|chip| Span::raw(chip.as_str()).width())
            .unwrap_or(0) as u16;
        let redraw_w = if gear.is_some() { 3 } else { 0 };
        let run_chip = format!(
            " {} ",
            if self.searching {
                action_icon_busy(self.theme)
            } else {
                action_icon_search(self.theme)
            }
        );
        let clear_chip = format!(" {} ", action_icon_clear(self.theme));
        let run_w = Span::raw(run_chip.as_str()).width() as u16;
        let clear_w = Span::raw(clear_chip.as_str()).width() as u16;
        let clear_x = area.x + area.width.saturating_sub(gear_w + redraw_w + clear_w);
        let search_x = clear_x.saturating_sub(run_w);
        self.search_button = Rect::new(search_x, area.y, run_w, 1);
        self.clear_button = Rect::new(clear_x, area.y, clear_w, 1);
        let pad = usize::from(search_x.saturating_sub(area.x + title.width() as u16));
        let mut spans = vec![title, Span::raw(" ".repeat(pad))];
        spans.push(Span::styled(
            run_chip,
            icon_button_style(
                self.mouse_pos
                    .is_some_and(|(x, y)| hits(self.search_button, x, y)),
                self.rg_error.is_none(),
            ),
        ));
        spans.push(Span::styled(
            clear_chip,
            icon_button_style(
                self.mouse_pos
                    .is_some_and(|(x, y)| hits(self.clear_button, x, y)),
                true,
            ),
        ));
        if redraw_w > 0 {
            let rx = area.x + area.width.saturating_sub(gear_w + redraw_w);
            let (redraw, rect) = redraw_button(self.theme, rx, area.y, self.mouse_pos);
            self.redraw = rect;
            spans.push(redraw);
        }
        if let Some(gear) = gear {
            let gx = area.x + area.width.saturating_sub(gear_w);
            self.gear = Rect::new(gx, area.y, gear_w, 1);
            spans.push(Span::styled(
                gear,
                icon_button_style(
                    self.mouse_pos.is_some_and(|(x, y)| hits(self.gear, x, y)),
                    true,
                ),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_query_box(&mut self, frame: &mut Frame, area: Rect, value: &str, enabled: bool) {
        self.field_query = area;
        let hovered = self.mouse_pos.is_some_and(|(x, y)| hits(area, x, y));
        let border = if matches!(self.focus, Focus::Query | Focus::Toggles) {
            ACCENT
        } else if hovered {
            TEXT_MUTED
        } else {
            SURFACE_BORDER
        };
        let block = Block::bordered()
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(if enabled {
                SURFACE_BG
            } else {
                SURFACE_BG_MUTED
            }));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);

        let labels: Vec<String> = SEARCH_TOGGLES
            .iter()
            .map(|kind| toggle_label(*kind))
            .collect();
        let [text_area, toggle_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        let query_style = if enabled {
            Style::default()
                .fg(TEXT_MAIN)
                .bg(SURFACE_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_MUTED).bg(SURFACE_BG_MUTED).dim()
        };
        let avail = usize::from(text_area.width)
            .saturating_sub(usize::from(self.focus == Focus::Query && enabled))
            .max(1);
        let mut text_spans = if value.is_empty() && self.focus != Focus::Query {
            vec![Span::styled(
                "Search".to_string(),
                Style::default().fg(TEXT_MUTED).bg(SURFACE_BG).dim(),
            )]
        } else {
            vec![Span::styled(input_tail(value, avail), query_style)]
        };
        if self.focus == Focus::Query && enabled {
            text_spans.push(Span::styled(
                "█",
                Style::default().fg(ACCENT).bg(SURFACE_BG),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(text_spans)), text_area);

        self.toggle_rects.clear();
        let mut spans = Vec::new();
        let total_width = labels
            .iter()
            .map(|label| Span::raw(label.as_str()).width() + 1)
            .sum::<usize>()
            .saturating_sub(1) as u16;
        let start_x = toggle_area.x + toggle_area.width.saturating_sub(total_width);
        let mut x = start_x;
        for (index, (kind, label)) in SEARCH_TOGGLES.into_iter().zip(labels).enumerate() {
            let w = Span::raw(label.as_str()).width() as u16;
            let rect = Rect::new(x, toggle_area.y, w, 1);
            self.toggle_rects.push((rect, kind));
            let on = toggle_on(self, kind);
            let active = self.focus == Focus::Toggles && self.toggle_focus == index;
            let hovered = self.mouse_pos.is_some_and(|(mx, my)| hits(rect, mx, my));
            spans.push(Span::styled(
                label,
                toggle_style(kind, on, active || hovered, enabled),
            ));
            x += w;
            if x < toggle_area.x + toggle_area.width {
                spans.push(Span::styled(" ", Style::default().bg(SURFACE_BG)));
                x += 1;
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).right_aligned(),
            toggle_area,
        );
    }

    fn draw_filters_toggle(&mut self, frame: &mut Frame, area: Rect) {
        let toggle = if self.filters_collapsed { "▸" } else { "▾" };
        let label = format!(" {toggle} Filters");
        self.filters_toggle = area;
        let hovered = self.mouse_pos.is_some_and(|(x, y)| hits(area, x, y));
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                label,
                Style::default()
                    .fg(TEXT_MUTED)
                    .bg(if hovered { HOVER_BG } else { APP_BG })
                    .bold(),
            )])),
            area,
        );
    }

    fn draw_filter_box(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        field: FilterField,
        value: &str,
        enabled: bool,
    ) {
        let focus = field.focus();
        let label = field.label();
        let hovered = self.mouse_pos.is_some_and(|(x, y)| hits(area, x, y));
        let border = if self.focus == focus {
            ACCENT
        } else if hovered {
            TEXT_MUTED
        } else {
            SURFACE_BORDER
        };
        match field {
            FilterField::Include => self.field_include = area,
            FilterField::Exclude => self.field_exclude = area,
        }
        let block = Block::bordered()
            .title(format!(" {label} "))
            .title_style(Style::default().fg(TEXT_MUTED))
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(if enabled {
                SURFACE_BG
            } else {
                SURFACE_BG_MUTED
            }));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        frame.render_widget(block, area);
        let style = if enabled {
            Style::default().fg(TEXT_MAIN).bg(SURFACE_BG)
        } else {
            Style::default().fg(TEXT_MUTED).bg(SURFACE_BG_MUTED).dim()
        };
        let avail = usize::from(inner.width)
            .saturating_sub(usize::from(self.focus == focus && enabled))
            .max(4);
        let mut spans = if value.is_empty() && self.focus != focus {
            vec![Span::styled(
                field.placeholder().to_string(),
                Style::default().fg(TEXT_MUTED).bg(SURFACE_BG).dim(),
            )]
        } else {
            vec![Span::styled(input_tail(value, avail), style)]
        };
        if self.focus == focus && enabled {
            spans.push(Span::styled(
                "█",
                Style::default().fg(ACCENT).bg(SURFACE_BG),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
    }

    fn draw_status(&mut self, frame: &mut Frame, area: Rect) {
        let line = if let Some(err) = &self.inline_error {
            Line::from(Span::styled(
                format!(" {err}"),
                Style::default().fg(Color::Red),
            ))
        } else if self.searching {
            Line::from(Span::styled(" Searching…", Style::default().dim()))
        } else if let Some(summary) = &self.summary {
            Line::from(Span::styled(format!(" {summary}"), Style::default().dim()))
        } else {
            Line::default()
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_results(&mut self, frame: &mut Frame, area: Rect) {
        if self.rows.is_empty() {
            if self.searching {
                frame.render_widget(Paragraph::new("  searching…".dim().italic()), area);
            }
            self.body = BodyGeom {
                top: area.y,
                height: area.height,
                offset: 0,
            };
            return;
        }
        let h = usize::from(area.height).max(1);
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(h));
        if self.snap {
            if let Some(sel) = self.selected {
                if sel < self.scroll {
                    self.scroll = sel;
                } else if sel >= self.scroll + h {
                    self.scroll = sel + 1 - h;
                }
            }
            self.snap = false;
        }
        let content_width = list_content_width(area.width, self.rows.len(), h);
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(h)
            .map(|(index, row)| {
                result_item(
                    self,
                    *row,
                    index == self.hovered.unwrap_or(usize::MAX),
                    self.selected == Some(index),
                    usize::from(content_width),
                )
            })
            .collect();
        frame.render_widget(List::new(items), area);
        draw_scrollbar(frame, area, self.rows.len(), h, self.scroll);
        self.body = BodyGeom {
            top: area.y,
            height: area.height,
            offset: self.scroll,
        };
    }

    fn draw_rg_error(&mut self, frame: &mut Frame, area: Rect) {
        let msg = self.rg_error.as_deref().unwrap_or("rg is unavailable");
        let lines = vec![
            Line::from(Span::styled(
                "Search needs ripgrep (`rg`).",
                Style::default().bold(),
            )),
            Line::from(Span::styled(msg.to_string(), Style::default().dim())),
            Line::default(),
        ];
        let width = area.width.clamp(16, 44);
        let height = 5;
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 3,
            width,
            height,
        );
        let inner = popup.inner(ratatui::layout::Margin::new(1, 1));
        let [text_area, button_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        let button_label = " Retry ";
        let button_w = Span::raw(button_label).width() as u16;
        let bx = text_area.x + text_area.width.saturating_sub(button_w) / 2;
        self.retry_button = Rect::new(bx, button_area.y, button_w, 1);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            ratatui::widgets::Block::bordered().border_style(Style::default().dim()),
            popup,
        );
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            text_area,
        );
        let style = icon_button_style(
            self.mouse_pos
                .is_some_and(|(x, y)| hits(self.retry_button, x, y)),
            true,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(button_label, style)).alignment(Alignment::Center),
            self.retry_button,
        );
    }

    fn draw_footer(&mut self, frame: &mut Frame, footer: Rect) {
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
        let footer_lines: Vec<Line> = if let Some(notice) = &self.notice {
            wrap_footer_message(notice, footer.width, 4)
                .into_iter()
                .map(|l| l.fg(Color::Yellow).into())
                .collect()
        } else if self.overlay.is_some() || !self.sidebar_state.show_hotkeys {
            Vec::new()
        } else {
            wrap_hints(&self.hints(), footer.width, 3)
        };
        frame.render_widget(Paragraph::new(footer_lines), footer);
    }

    fn settings_rows(&self) -> Vec<SettingRow> {
        vec![
            (
                Setting::UnifiedSidebar,
                "Unified sidebar",
                if self.merged() { "on" } else { "off" }.to_string(),
                true,
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
                if self.sidebar_state.hide_unmodified {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
            (
                Setting::Hotkeys,
                "Footer hotkeys",
                if self.sidebar_state.show_hotkeys {
                    "shown"
                } else {
                    "hidden"
                }
                .to_string(),
                true,
            ),
            (
                Setting::AutoOpen,
                "Auto-open sidebar",
                if self.sidebar_state.auto_open {
                    "on"
                } else {
                    "off"
                }
                .to_string(),
                true,
            ),
        ]
    }

    fn open_settings(&mut self) {
        self.overlay = Some(Overlay::Settings {
            selected: 0,
            rect: Rect::default(),
        });
    }

    fn open_theme_picker(&mut self) {
        let selected = syntax::diff_themes()
            .iter()
            .position(|theme| *theme == self.sidebar_state.diff_theme)
            .unwrap_or(0);
        self.overlay = Some(Overlay::ThemePicker {
            selected,
            scroll: 0,
            rect: Rect::default(),
        });
    }

    fn open_settings_at(&mut self, setting: Setting) {
        let selected = self
            .settings_rows()
            .iter()
            .position(|(row, ..)| *row == setting)
            .unwrap_or(0);
        self.overlay = Some(Overlay::Settings {
            selected,
            rect: Rect::default(),
        });
    }

    fn toggle_setting(&mut self, index: usize) {
        let rows = self.settings_rows();
        let Some((setting, _, _, enabled)) = rows
            .get(index)
            .map(|(setting, label, value, enabled)| (*setting, *label, value.clone(), *enabled))
        else {
            return;
        };
        if !enabled {
            return;
        }
        match setting {
            Setting::UnifiedSidebar => {
                let on = !self.merged();
                self.sidebar_state.merged = on;
                self.sidebar_state.active = if on { MY_VIEW } else { View::Explorer };
                sidebar::save_state(self.sidebar_state);
                self.apply_identity();
                if !on {
                    self.pending_exit = Some(Exit::Switch);
                }
                self.overlay = None;
            }
            Setting::IconTheme => {
                self.theme = self.theme.toggled();
                self.sidebar_state.icons = Some(self.theme);
                sidebar::save_state(self.sidebar_state);
            }
            Setting::DiffTheme => self.open_theme_picker(),
            Setting::HideUnmodified => {
                self.sidebar_state.hide_unmodified = !self.sidebar_state.hide_unmodified;
                sidebar::save_state(self.sidebar_state);
            }
            Setting::Hotkeys => {
                self.sidebar_state.show_hotkeys = !self.sidebar_state.show_hotkeys;
                sidebar::save_state(self.sidebar_state);
            }
            Setting::AutoOpen => {
                self.sidebar_state.auto_open = !self.sidebar_state.auto_open;
                sidebar::save_state(self.sidebar_state);
            }
        }
    }

    fn choose_diff_theme(&mut self, index: usize) {
        let Some(theme) = syntax::diff_themes().get(index).copied() else {
            return;
        };
        self.sidebar_state.diff_theme = theme;
        sidebar::save_state(self.sidebar_state);
        self.open_settings_at(Setting::DiffTheme);
    }

    fn draw_settings(&mut self, frame: &mut Frame) {
        let rows = self.settings_rows();
        let width = 30.min(self.last_width).max(24);
        let height = (rows.len() as u16 + 2).min(self.last_height).max(6);
        let popup = Rect::new(
            self.last_width.saturating_sub(width) / 2,
            self.last_height.saturating_sub(height) / 3,
            width,
            height,
        );
        let selected = match self.overlay.as_mut() {
            Some(Overlay::Settings { selected, rect }) => {
                *rect = popup;
                *selected
            }
            _ => return,
        };
        let items: Vec<ListItem> =
            rows.iter()
                .enumerate()
                .map(|(index, (_, label, value, enabled))| {
                    let mut style = if index == selected {
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    if !enabled {
                        style = style.dim();
                    }
                    let line = Line::from(vec![
                        Span::raw(format!(" {label}")),
                        Span::raw(" ".repeat(
                            usize::from(width).saturating_sub(label.len() + value.len() + 4),
                        )),
                        Span::styled(value.clone(), Style::default().dim()),
                    ]);
                    ListItem::new(line).style(style)
                })
                .collect();
        frame.render_widget(Clear, popup);
        frame.render_widget(
            ratatui::widgets::Block::bordered()
                .title(" Settings ")
                .border_style(Style::default().dim()),
            popup,
        );
        frame.render_widget(
            List::new(items),
            popup.inner(ratatui::layout::Margin::new(1, 1)),
        );
    }

    fn draw_theme_picker(&mut self, frame: &mut Frame) {
        let Some(Overlay::ThemePicker {
            selected,
            scroll,
            rect,
        }) = self.overlay.as_mut()
        else {
            return;
        };
        *rect = draw_option_picker(
            frame,
            Rect::new(0, 0, self.last_width, self.last_height),
            "Diff theme",
            &syntax::diff_themes()
                .iter()
                .map(|theme| theme.as_name())
                .collect::<Vec<_>>(),
            *selected,
            scroll,
        );
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.notice = None;
        if self.handle_overlay_key(key) {
            return self.pending_exit.take();
        }
        let typing_focus = matches!(self.focus, Focus::Query | Focus::Include | Focus::Exclude);
        if !typing_focus {
            match key.code {
                KeyCode::Char('s') => self.open_settings(),
                KeyCode::Char('1') => return self.switch_to(View::Explorer),
                KeyCode::Char('2') => return self.switch_to(View::SourceControl),
                KeyCode::Char('3') => return self.switch_to(View::Search),
                KeyCode::Char('b') => self.hide(),
                _ => {}
            }
        }
        if self.rg_error.is_some() {
            if matches!(key.code, KeyCode::Enter | KeyCode::Char('r')) {
                self.retry_rg();
            }
            return self.pending_exit.take();
        }
        match self.focus {
            Focus::Query | Focus::Include | Focus::Exclude => self.on_input_key(key),
            Focus::Toggles => self.on_toggle_key(key),
            Focus::Results => self.on_results_key(key),
        }
        self.pending_exit.take()
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        let Some(overlay) = self.overlay.take() else {
            return false;
        };
        match overlay {
            Overlay::Settings { mut selected, rect } => {
                let rows = self.settings_rows();
                let len = rows.len();
                let mut restore = true;
                match key.code {
                    KeyCode::Esc => restore = false,
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(len.saturating_sub(1))
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let setting = rows.get(selected).map(|row| row.0);
                        self.toggle_setting(selected);
                        restore =
                            !matches!(setting, Some(Setting::UnifiedSidebar | Setting::DiffTheme));
                    }
                    _ => {}
                }
                if restore && self.overlay.is_none() {
                    self.overlay = Some(Overlay::Settings { selected, rect });
                }
                true
            }
            Overlay::ThemePicker {
                mut selected,
                mut scroll,
                rect,
            } => {
                let len = syntax::diff_themes().len();
                let mut restore = true;
                match key.code {
                    KeyCode::Esc => {
                        self.open_settings_at(Setting::DiffTheme);
                        restore = false;
                    }
                    KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(len.saturating_sub(1))
                    }
                    KeyCode::Home | KeyCode::Char('g') => selected = 0,
                    KeyCode::End | KeyCode::Char('G') => selected = len.saturating_sub(1),
                    KeyCode::PageUp => selected = selected.saturating_sub(10),
                    KeyCode::PageDown => selected = (selected + 10).min(len.saturating_sub(1)),
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        self.choose_diff_theme(selected);
                        restore = false;
                    }
                    _ => {}
                }
                if selected < scroll {
                    scroll = selected;
                }
                if restore && self.overlay.is_none() {
                    self.overlay = Some(Overlay::ThemePicker {
                        selected,
                        scroll,
                        rect,
                    });
                }
                true
            }
        }
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::BackTab => self.focus = self.prev_focus(),
            KeyCode::Enter => self.start_search(true),
            KeyCode::Esc if self.focus == Focus::Query => {
                self.query.clear();
                self.inline_error = None;
                self.schedule_search();
            }
            KeyCode::Backspace => {
                self.active_input_mut().pop();
                self.inline_error = None;
                self.schedule_search();
            }
            KeyCode::Char('u')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.active_input_mut().clear();
                self.inline_error = None;
                self.schedule_search();
            }
            KeyCode::Char(c) => {
                self.active_input_mut().push(c);
                self.inline_error = None;
                self.schedule_search();
            }
            KeyCode::Down => {
                self.focus = Focus::Results;
                self.select_first_result();
            }
            _ => {}
        }
    }

    fn on_toggle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.toggle_focus = self.toggle_focus.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.toggle_focus = (self.toggle_focus + 1).min(2)
            }
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::BackTab => self.focus = self.prev_focus(),
            KeyCode::Enter | KeyCode::Char(' ') => self.flip_toggle(self.focused_toggle()),
            KeyCode::Down => {
                self.focus = Focus::Results;
                self.select_first_result();
            }
            _ => {}
        }
    }

    fn on_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page),
            KeyCode::PageDown => {
                self.scroll = (self.scroll + self.page).min(self.rows.len().saturating_sub(1))
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                self.select_first_result();
            }
            KeyCode::End | KeyCode::Char('G') => self.select_last_result(),
            KeyCode::Enter => self.open_selected_preview(),
            KeyCode::Tab => self.focus = self.next_focus(),
            KeyCode::BackTab => self.focus = self.prev_focus(),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        self.last_mouse = Some(std::time::Instant::now());
        self.mouse_pos = Some((mouse.column, mouse.row));
        self.notice = None;
        if self.handle_overlay_mouse(mouse) {
            return self.pending_exit.take();
        }
        if self.merged()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && mouse.row == self.activity.row
        {
            if within(mouse.column, self.activity.explorer) {
                return self.switch_to(View::Explorer);
            }
            if within(mouse.column, self.activity.source_control) {
                return self.switch_to(View::SourceControl);
            }
            if within(mouse.column, self.activity.search) {
                return self.switch_to(View::Search);
            }
        }
        if hits(self.redraw, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.redraw_requested = true;
            return None;
        }
        if hits(self.gear, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.open_settings();
            return self.pending_exit.take();
        }
        if hits_collapse_button(mouse.column, mouse.row, self.last_width, self.last_height)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.hide();
            return self.pending_exit.take();
        }
        if self.rg_error.is_some() {
            if hits(self.retry_button, mouse.column, mouse.row)
                && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            {
                self.retry_rg();
            }
            return self.pending_exit.take();
        }
        if hits(self.search_button, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.start_search(true);
            return self.pending_exit.take();
        }
        if hits(self.clear_button, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.pending_search = None;
            self.cancel_running();
            self.query.clear();
            self.clear_results();
            self.inline_error = None;
            return self.pending_exit.take();
        }
        if hits(self.filters_toggle, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.filters_collapsed = !self.filters_collapsed;
            if self.filters_collapsed && matches!(self.focus, Focus::Include | Focus::Exclude) {
                self.focus = Focus::Results;
            }
            return self.pending_exit.take();
        }
        if hits(self.field_include, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.focus = Focus::Include;
            return self.pending_exit.take();
        }
        if hits(self.field_exclude, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.focus = Focus::Exclude;
            return self.pending_exit.take();
        }
        if let Some(index) = self
            .toggle_rects
            .iter()
            .position(|(rect, _)| hits(*rect, mouse.column, mouse.row))
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            let kind = self.toggle_rects[index].1;
            if self.focus == Focus::Toggles {
                self.toggle_focus = index;
            }
            self.flip_toggle(kind);
            return self.pending_exit.take();
        }
        if hits(self.field_query, mouse.column, mouse.row)
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            self.focus = Focus::Query;
            return self.pending_exit.take();
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                self.scroll = (self.scroll + 3).min(self.rows.len().saturating_sub(1));
            }
            MouseEventKind::Moved => {
                self.hovered = row_index_at(self.body, self.rows.len(), mouse.row)
                    .filter(|index| self.rows.get(*index).is_some_and(|row| row.selectable()));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = row_index_at(self.body, self.rows.len(), mouse.row)
                    && self.rows.get(index).is_some_and(|row| row.selectable())
                {
                    self.focus = Focus::Results;
                    self.selected = Some(index);
                    self.open_selected_preview();
                }
            }
            _ => {}
        }
        self.pending_exit.take()
    }

    fn handle_overlay_mouse(&mut self, mouse: MouseEvent) -> bool {
        let Some(overlay) = self.overlay.take() else {
            return false;
        };
        match overlay {
            Overlay::Settings { rect, mut selected } => {
                let mut restore = true;
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    if let Some(index) =
                        settings_index(rect, mouse.column, mouse.row, self.settings_rows().len())
                    {
                        selected = index;
                        let setting = self.settings_rows().get(index).map(|row| row.0);
                        self.toggle_setting(index);
                        restore =
                            !matches!(setting, Some(Setting::UnifiedSidebar | Setting::DiffTheme));
                    } else if !hits(rect, mouse.column, mouse.row) {
                        restore = false;
                    }
                }
                if restore && self.overlay.is_none() {
                    self.overlay = Some(Overlay::Settings { selected, rect });
                }
                true
            }
            Overlay::ThemePicker {
                rect,
                mut selected,
                scroll,
            } => {
                let mut restore = true;
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    if let Some(index) = option_picker_index(
                        rect,
                        scroll,
                        mouse.column,
                        mouse.row,
                        syntax::diff_themes().len(),
                    ) {
                        selected = index;
                        self.choose_diff_theme(index);
                        restore = false;
                    } else if !hits(rect, mouse.column, mouse.row) {
                        self.open_settings_at(Setting::DiffTheme);
                        restore = false;
                    }
                }
                if restore && self.overlay.is_none() {
                    self.overlay = Some(Overlay::ThemePicker {
                        selected,
                        scroll,
                        rect,
                    });
                }
                true
            }
        }
    }

    pub fn on_focus_lost(&mut self) {
        self.mouse_pos = None;
        self.hovered = None;
    }

    pub fn tick(&mut self) {
        if self
            .pending_search
            .is_some_and(|at| std::time::Instant::now() >= at)
        {
            self.start_search(false);
        }
        self.poll_running();
    }

    fn active_input_mut(&mut self) -> &mut String {
        match self.focus {
            Focus::Query => &mut self.query,
            Focus::Include => &mut self.include,
            Focus::Exclude => &mut self.exclude,
            _ => &mut self.query,
        }
    }

    fn focused_toggle(&self) -> ToggleKind {
        SEARCH_TOGGLES[self.toggle_focus.min(SEARCH_TOGGLES.len() - 1)]
    }

    fn flip_toggle(&mut self, kind: ToggleKind) {
        match kind {
            ToggleKind::Regex => self.regex = !self.regex,
            ToggleKind::Case => self.case_sensitive = !self.case_sensitive,
            ToggleKind::WholeWord => self.whole_word = !self.whole_word,
        }
        self.inline_error = None;
        self.start_search(false);
    }

    fn schedule_search(&mut self) {
        self.pending_search = Some(std::time::Instant::now() + SEARCH_DEBOUNCE);
    }

    fn next_focus(&self) -> Focus {
        match self.focus {
            Focus::Query => Focus::Toggles,
            Focus::Toggles if self.filters_collapsed => Focus::Results,
            Focus::Toggles => Focus::Include,
            Focus::Include => Focus::Exclude,
            Focus::Exclude => Focus::Results,
            Focus::Results => Focus::Query,
        }
    }

    fn prev_focus(&self) -> Focus {
        match self.focus {
            Focus::Query => Focus::Results,
            Focus::Toggles => Focus::Query,
            Focus::Include => Focus::Toggles,
            Focus::Exclude => Focus::Include,
            Focus::Results if self.filters_collapsed => Focus::Toggles,
            Focus::Results => Focus::Exclude,
        }
    }

    fn switch_to(&mut self, view: View) -> Option<Exit> {
        if !self.merged() || view == MY_VIEW {
            return None;
        }
        self.sidebar_state.active = view;
        sidebar::save_state(self.sidebar_state);
        Some(Exit::Switch)
    }

    fn hide(&mut self) {
        let Some(ctl) = &self.pane_ctl else { return };
        if let Ok(json) = herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({})) {
            let tab = herdr_sidebar::launch::tab_of(&json, &ctl.pane_id);
            herdr_sidebar::snooze::set(&herdr_sidebar::snooze::dir(), &tab);
        }
        let _ = herdr_sidebar::ipc::call_text(
            "pane.close",
            serde_json::json!({ "pane_id": ctl.pane_id }),
        );
        self.pending_exit = Some(Exit::Quit);
    }

    fn clear_results(&mut self) {
        self.groups.clear();
        self.rows.clear();
        self.selected = None;
        self.hovered = None;
        self.scroll = 0;
        self.snap = false;
        self.summary = None;
    }

    fn retry_rg(&mut self) {
        self.rg_error = rg_available().err();
    }

    fn start_search(&mut self, manual: bool) {
        self.pending_search = None;
        self.cancel_running();
        self.inline_error = None;
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.clear_results();
            if manual {
                self.inline_error = Some("Enter a search term".into());
            }
            return;
        }
        if let Err(err) = validate_pattern(&query, self.regex, self.case_sensitive, self.whole_word)
        {
            self.clear_results();
            self.inline_error = Some(err);
            return;
        }
        self.clear_results();
        self.searching = true;
        let state = self.workspace_state();
        let (tx, rx) = mpsc::channel();
        let mut command = search_command(&self.root, &state, &query);
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            self.searching = false;
            self.rg_error = Some("ripgrep (`rg`) is not available.".into());
            return;
        };
        let Some(stdout) = child.stdout.take() else {
            self.searching = false;
            self.inline_error = Some("Search failed to capture rg output".into());
            let _ = child.kill();
            let _ = child.wait();
            return;
        };
        std::thread::spawn(move || read_rg_stream(stdout, tx));
        self.running = Some(RunningSearch { child, rx });
    }

    fn poll_running(&mut self) {
        let mut finished = false;
        let mut events = Vec::new();
        if let Some(running) = self.running.as_mut() {
            while let Ok(event) = running.rx.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                SearchEvent::Match(found) => self.push_match(found),
                SearchEvent::Finished => finished = true,
            }
        }
        if finished {
            if let Some(mut running) = self.running.take() {
                let _ = running.child.wait();
            }
            self.searching = false;
            let files = self.groups.len();
            let matches = self
                .groups
                .iter()
                .map(|group| group.matches.len())
                .sum::<usize>();
            self.summary = Some(if matches == 0 {
                "No results".into()
            } else {
                format!("{matches} matches in {files} files")
            });
        }
    }

    fn cancel_running(&mut self) {
        if let Some(mut running) = self.running.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
        self.searching = false;
    }

    fn push_match(&mut self, found: UiMatch) {
        let group = if let Some(index) = self
            .groups
            .iter()
            .position(|group| group.path == found.path)
        {
            index
        } else {
            self.groups.push(SearchGroup {
                path: found.path.clone(),
                matches: Vec::new(),
            });
            self.groups.len() - 1
        };
        self.groups[group].matches.push(SearchMatch {
            line_number: found.line_number,
            line_text: found.line_text,
            submatches: found.submatches,
        });
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (group, entry) in self.groups.iter().enumerate() {
            self.rows.push(DisplayRow::Header(group));
            self.rows
                .extend((0..entry.matches.len()).map(|item| DisplayRow::Match { group, item }));
        }
    }

    fn select_first_result(&mut self) {
        self.selected = self.rows.iter().position(|row| row.selectable());
        self.snap = self.selected.is_some();
    }

    fn select_last_result(&mut self) {
        self.selected = self.rows.iter().rposition(|row| row.selectable());
        self.snap = self.selected.is_some();
    }

    fn move_selection(&mut self, direction: isize) {
        let start = self
            .selected
            .or_else(|| self.rows.iter().position(|row| row.selectable()));
        let Some(mut index) = start.map(|i| i as isize) else {
            return;
        };
        loop {
            index += direction;
            if index < 0 || index >= self.rows.len() as isize {
                break;
            }
            if self.rows[index as usize].selectable() {
                self.selected = Some(index as usize);
                self.snap = true;
                return;
            }
        }
    }

    fn open_selected_preview(&mut self) {
        let Some(index) = self.selected else { return };
        let Some(DisplayRow::Match { group, item }) = self.rows.get(index).copied() else {
            return;
        };
        let Some(found) = self
            .groups
            .get(group)
            .and_then(|group| group.matches.get(item))
        else {
            return;
        };
        let Some(pane_id) = self.pane_ctl.as_ref().map(|ctl| ctl.pane_id.clone()) else {
            self.notice = Some("preview needs a herdr pane".into());
            return;
        };
        let path = self.root.join(&self.groups[group].path);
        let payload = herdr_sidebar::viewer::search_file_request(
            &path,
            found.line_number,
            &self.query,
            self.regex,
            self.case_sensitive,
            self.whole_word,
        );
        if let Err(e) = herdr_sidebar::viewer::open_in_pane(&pane_id, &self.root, &payload) {
            self.notice = Some(e);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel_running();
    }
}

fn search_command(root: &Path, state: &SearchState, query: &str) -> Command {
    let mut command = Command::new("rg");
    command
        .arg("--json")
        .arg("--line-buffered")
        .arg("--no-messages")
        .arg("--line-number")
        .current_dir(root);
    if !state.regex {
        command.arg("--fixed-strings");
    }
    if !state.case_sensitive {
        command.arg("--ignore-case");
    }
    if state.whole_word {
        command.arg("--word-regexp");
    }
    for pattern in split_globs(&state.include) {
        command.arg("-g").arg(pattern);
    }
    for pattern in split_globs(&state.exclude) {
        command.arg("-g").arg(format!("!{pattern}"));
    }
    command.arg("-e").arg(query).arg(".");
    command
}

fn rg_available() -> Result<(), String> {
    Command::new("rg")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|_| "ripgrep (`rg`) was not found on PATH.".into())
}

fn validate_pattern(
    query: &str,
    regex_mode: bool,
    case_sensitive: bool,
    whole_word: bool,
) -> Result<(), String> {
    build_preview_regex(query, regex_mode, case_sensitive, whole_word)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn build_preview_regex(
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

fn split_globs(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn read_rg_stream(stdout: std::process::ChildStdout, tx: mpsc::Sender<SearchEvent>) {
    use std::io::BufRead as _;
    let reader = std::io::BufReader::new(stdout);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<RgMessage>(&line) else {
            continue;
        };
        if msg.kind != "match" {
            continue;
        }
        let Some(path) = msg.data.path.text else {
            continue;
        };
        let Some(line_text) = msg.data.lines.text else {
            continue;
        };
        let found = UiMatch {
            path: path.trim_start_matches("./").to_string(),
            line_number: msg.data.line_number.unwrap_or(1),
            line_text: line_text.trim_end_matches(['\n', '\r']).to_string(),
            submatches: msg
                .data
                .submatches
                .into_iter()
                .map(|m| (m.start, m.end))
                .collect(),
        };
        let _ = tx.send(SearchEvent::Match(found));
    }
    let _ = tx.send(SearchEvent::Finished);
}

#[derive(serde::Deserialize)]
struct RgMessage {
    #[serde(rename = "type")]
    kind: String,
    data: RgMatchData,
}

#[derive(Default, serde::Deserialize)]
struct RgMatchData {
    #[serde(default)]
    path: RgText,
    #[serde(default)]
    lines: RgText,
    line_number: Option<usize>,
    #[serde(default)]
    submatches: Vec<RgSubmatch>,
}

#[derive(Default, serde::Deserialize)]
struct RgText {
    text: Option<String>,
}

#[derive(serde::Deserialize)]
struct RgSubmatch {
    start: usize,
    end: usize,
}

fn settings_index(rect: Rect, column: u16, row: u16, total: usize) -> Option<usize> {
    let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
    (column >= inner.x
        && column < inner.x + inner.width
        && row >= inner.y
        && row < inner.y + inner.height)
        .then(|| usize::from(row - inner.y))
        .filter(|index| *index < total)
}

fn within(x: u16, (start, end): (u16, u16)) -> bool {
    (start..end).contains(&x)
}

fn row_index_at(body: BodyGeom, row_count: usize, mouse_row: u16) -> Option<usize> {
    if mouse_row < body.top || mouse_row >= body.top + body.height {
        return None;
    }
    let index = body.offset + usize::from(mouse_row - body.top);
    (index < row_count).then_some(index)
}

fn list_content_width(width: u16, total: usize, visible: usize) -> u16 {
    width.saturating_sub(u16::from(total > visible))
}

fn result_item(
    app: &App,
    row: DisplayRow,
    hovered: bool,
    selected: bool,
    width: usize,
) -> ListItem<'static> {
    match row {
        DisplayRow::Header(group) => {
            let Some(group) = app.groups.get(group) else {
                return ListItem::new(Line::raw(""));
            };
            let file = group
                .path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(group.path.as_str());
            let parent = group
                .path
                .strip_suffix(file)
                .unwrap_or("")
                .trim_end_matches(['/', '\\']);
            let file_icon = icon(app.theme, file, false, false);
            let icon_style = match file_icon.rgb {
                Some((r, g, b)) => Style::default().fg(Color::Rgb(r, g, b)).bg(APP_BG),
                None => Style::default().fg(ACCENT).bg(APP_BG),
            };
            let badge = format!(" {} ", group.matches.len());
            let badge_w = Span::raw(badge.as_str()).width();
            let head = format!("▾ {} {}", file_icon.glyph, file);
            let avail = width.saturating_sub(Span::raw(head.as_str()).width() + badge_w + 2);
            let mut spans = vec![
                Span::styled("▾ ", Style::default().fg(ACCENT).bg(APP_BG)),
                Span::styled(format!("{} ", file_icon.glyph), icon_style),
                Span::styled(
                    file.to_string(),
                    Style::default().fg(TEXT_MAIN).bg(APP_BG).bold(),
                ),
            ];
            if !parent.is_empty() && avail > 4 {
                spans.push(Span::styled(
                    format!("  {}", truncate_to(parent.to_string(), avail)),
                    Style::default().fg(TEXT_MUTED).bg(APP_BG),
                ));
            }
            let used = spans.iter().map(Span::width).sum::<usize>() + badge_w;
            let mut flat = spans;
            flat.push(Span::raw(" ".repeat(width.saturating_sub(used))));
            flat.push(Span::styled(
                badge,
                Style::default()
                    .bg(COUNT_BG)
                    .fg(COUNT_FG)
                    .add_modifier(Modifier::BOLD),
            ));
            ListItem::new(Line::from(flat))
        }
        DisplayRow::Match { group, item } => {
            let Some(found) = app
                .groups
                .get(group)
                .and_then(|group| group.matches.get(item))
            else {
                return ListItem::new(Line::raw(""));
            };
            let base = if selected {
                Style::default().bg(Color::Rgb(0x2b, 0x30, 0x45))
            } else if hovered {
                Style::default().bg(HOVER_BG)
            } else {
                Style::default().bg(APP_BG)
            };
            let prefix = format!("  {:>4} ", found.line_number);
            let prefix_w = Span::raw(prefix.as_str()).width();
            let avail = width.saturating_sub(prefix_w);
            let (snippet, ranges) = snippet_window(&found.line_text, &found.submatches, avail);
            let mut spans = vec![Span::styled(
                prefix,
                base.patch(Style::default().fg(TEXT_MUTED)),
            )];
            spans.extend(highlight_spans(
                &snippet,
                &ranges,
                base.patch(Style::default().fg(TEXT_MAIN)),
                Style::default().bg(MATCH_BG).fg(MATCH_FG),
            ));
            let used = spans.iter().map(Span::width).sum::<usize>();
            if used < width {
                spans.push(Span::styled(" ".repeat(width - used), base));
            }
            ListItem::new(Line::from(spans))
        }
    }
}

fn toggle_label(kind: ToggleKind) -> String {
    match kind {
        ToggleKind::Case => " Aa ".to_string(),
        ToggleKind::WholeWord => " ab ".to_string(),
        ToggleKind::Regex => " .* ".to_string(),
    }
}

fn toggle_on(app: &App, kind: ToggleKind) -> bool {
    match kind {
        ToggleKind::Regex => app.regex,
        ToggleKind::Case => app.case_sensitive,
        ToggleKind::WholeWord => app.whole_word,
    }
}

fn toggle_style(kind: ToggleKind, on: bool, active: bool, enabled: bool) -> Style {
    let mut style = Style::default().bg(SURFACE_BG).fg(TEXT_MUTED);
    if kind == ToggleKind::WholeWord {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if on {
        style = style.bg(KEYCAP_BG).fg(ACCENT).add_modifier(Modifier::BOLD);
    } else if active {
        style = style.bg(HOVER_BG).fg(TEXT_MAIN);
    }
    if !enabled {
        style = style.dim();
    }
    style
}

fn action_icon_search(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f002}",
        IconTheme::Emoji => "🔎",
    }
}

fn action_icon_busy(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{eb37}",
        IconTheme::Emoji => "⟳",
    }
}

fn action_icon_clear(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f00d}",
        IconTheme::Emoji => "✕",
    }
}

fn snippet_window(
    line: &str,
    submatches: &[(usize, usize)],
    max: usize,
) -> (String, Vec<(usize, usize)>) {
    if max <= 2 || line.is_empty() {
        return (String::new(), Vec::new());
    }
    let first = submatches
        .first()
        .map(|(start, _)| *start)
        .unwrap_or(0)
        .min(line.len());
    let mut start = if line.len() <= max || first < max / 2 {
        0
    } else {
        first.saturating_sub(max / 3)
    };
    while start > 0 && !line.is_char_boundary(start) {
        start -= 1;
    }
    let budget = max.saturating_sub(usize::from(start > 0));
    let mut end = line.len().min(start.saturating_add(budget));
    while end > start && !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = String::new();
    if start > 0 {
        text.push('…');
    }
    text.push_str(&line[start..end]);
    if end < line.len() && Span::raw(text.as_str()).width() < max {
        text.push('…');
    }
    let prefix = if start > 0 { '…'.len_utf8() } else { 0 };
    let ranges = submatches
        .iter()
        .filter_map(|(ms, me)| {
            let s = (*ms).max(start);
            let e = (*me).min(end);
            (s < e).then_some((s - start + prefix, e - start + prefix))
        })
        .collect();
    (text, ranges)
}

fn highlight_spans(
    text: &str,
    ranges: &[(usize, usize)],
    base: Style,
    highlight: Style,
) -> Vec<Span<'static>> {
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in ranges {
        let start = (*start).min(text.len());
        let end = (*end).min(text.len());
        if cursor < start {
            spans.push(Span::styled(text[cursor..start].to_string(), base));
        }
        if start < end {
            spans.push(Span::styled(
                text[start..end].to_string(),
                base.patch(highlight),
            ));
        }
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), base));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn split_globs_trims_and_skips_empty_entries() {
        assert_eq!(
            split_globs("src/**/*.rs, Cargo.toml , ,tests/**"),
            ["src/**/*.rs", "Cargo.toml", "tests/**",]
        );
    }

    #[test]
    fn snippet_window_keeps_the_first_match_visible() {
        let line = "prefix prefix prefix SEARCH suffix suffix";
        let start = line.find("SEARCH").unwrap();
        let (snippet, ranges) = snippet_window(line, &[(start, start + 6)], 18);
        assert!(snippet.contains("SEARCH"), "{snippet:?}");
        assert_eq!(ranges.len(), 1);
        assert_eq!(&snippet[ranges[0].0..ranges[0].1], "SEARCH");
    }

    #[test]
    fn preview_regex_wraps_whole_words_for_literal_searches() {
        let re = build_preview_regex("main", false, false, true).unwrap();
        assert!(re.is_match("fn main()"));
        assert!(!re.is_match("maintain"));
    }

    #[test]
    fn auto_search_with_empty_query_clears_without_error() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.groups.push(SearchGroup {
            path: "src/main.rs".into(),
            matches: vec![SearchMatch {
                line_number: 1,
                line_text: "fn main() {}".into(),
                submatches: vec![(3, 7)],
            }],
        });
        app.rebuild_rows();
        app.summary = Some("stale".into());

        app.start_search(false);

        assert!(app.inline_error.is_none());
        assert!(app.groups.is_empty());
        assert!(app.rows.is_empty());
        assert!(app.summary.is_none());
        assert!(!app.searching);
    }

    #[test]
    fn query_toggle_clicks_update_state() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.rg_error = None;
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        assert!(app.focused_toggle() == ToggleKind::Case);
        for (rect, kind) in app.toggle_rects.clone() {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            assert!(
                toggle_on(&app, kind),
                "{} did not toggle on",
                toggle_label(kind)
            );
        }

        assert!(app.case_sensitive);
        assert!(app.whole_word);
        assert!(app.regex);
        assert!(app.focus == Focus::Query);
    }

    #[test]
    fn rg_command_applies_search_toggles_and_filters() {
        let state = SearchState {
            query: "ignored".into(),
            include: "src/**/*.rs".into(),
            exclude: "target/**".into(),
            regex: true,
            case_sensitive: true,
            whole_word: true,
        };
        let command = search_command(Path::new("/tmp"), &state, "app.*");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect();

        assert!(args.iter().any(|arg| arg == "--word-regexp"));
        assert!(!args.iter().any(|arg| arg == "--fixed-strings"));
        assert!(!args.iter().any(|arg| arg == "--ignore-case"));
        assert!(args.windows(2).any(|args| args == ["-g", "src/**/*.rs"]));
        assert!(args.windows(2).any(|args| args == ["-g", "!target/**"]));
        assert!(args.windows(2).any(|args| args == ["-e", "app.*"]));
    }

    #[test]
    fn hovering_activity_bar_does_not_switch_views() {
        let mut app = App::new(PathBuf::from("/tmp"));
        let before = app.sidebar_state.active;
        app.activity = ActivityZones {
            row: 1,
            explorer: (0, 3),
            source_control: (4, 7),
            search: (8, 11),
        };
        let exit = app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(exit.is_none());
        assert_eq!(app.sidebar_state.active, before);
    }

    #[test]
    fn hovering_header_buttons_does_not_activate_them() {
        let mut app = App::new(PathBuf::from("/tmp"));
        app.sidebar_state.merged = true;
        app.rg_error = None;
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();

        for rect in [app.gear, app.redraw] {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: rect.x,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
        }

        assert!(app.overlay.is_none());
        assert!(!app.redraw_requested);
    }
}
