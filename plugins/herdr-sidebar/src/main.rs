//! herdr-sidebar — the VS Code sidebar for herdr: file explorer and source
//! control in ONE binary. In unified mode both views share a pane and the
//! activity bar switches between them IN PROCESS (instant, no flash); in
//! separated mode the same binary runs one pane per view, pinned with
//! `--view explorer|git|search`. `--preview <ctl>` runs the file-preview pane.
//!
//! The `--*` stdin→stdout helper modes serve the launcher scripts — see
//! launch.rs.

mod explorer_app;
mod search_app;
mod scm_app;

use std::io::Read;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    MouseEventKind,
};
use herdr_sidebar::{launch, state, viewer};
use herdr_sidebar::workspace_sync::Session as SyncSession;
use state::{Exit, View};

/// How often the source-control view re-reads `git status` while idle.
const REFRESH_EVERY: Duration = Duration::from_millis(1500);

fn main() -> std::io::Result<()> {
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("--launch-decision") => {
            // Optional second arg picks the source-control decision (the
            // open-git launcher); default is the explorer/sidebar decision.
            let now = state::unix_now();
            let out = if std::env::args().nth(2).as_deref() == Some("git") {
                launch::launch_decision_git(&read_stdin()?, now)
            } else {
                launch::launch_decision(&read_stdin()?, now)
            };
            println!("{out}");
            return Ok(());
        }
        Some("--focused-pane") => {
            println!("{}", launch::focused_pane(&read_stdin()?));
            return Ok(());
        }
        Some("--open-plan") => {
            println!("{}", launch::open_plan(&read_stdin()?));
            return Ok(());
        }
        Some("--focused-tab") => {
            println!("{}", launch::focused_tab(&read_stdin()?));
            return Ok(());
        }
        Some("--auto-open") => {
            // For the unix ensure hook: skip auto-docking when the user
            // turned "Auto-open sidebar" off in ⚙ Settings (issue #8).
            println!("{}", if state::load_state().auto_open { "on" } else { "off" });
            return Ok(());
        }
        Some("--preview") => {
            let Some(control) = std::env::args().nth(2) else {
                eprintln!("herdr-sidebar: --preview needs a control-file path");
                std::process::exit(2);
            };
            return viewer::run(std::path::Path::new(&control));
        }
        Some("--view") => {}
        Some(other) => {
            eprintln!("herdr-sidebar: unknown argument `{other}`");
            eprintln!(
                "usage: herdr-sidebar [--view explorer|git|search|--preview <ctl>|--launch-decision [git]|--focused-pane|--open-plan|--focused-tab|--auto-open]"
            );
            std::process::exit(2);
        }
        None => {}
    }

    // Starting view: an explicit `--view` pin (separated panes), else the
    // last-active view when the unified sidebar is on.
    let pinned = if mode.as_deref() == Some("--view") {
        std::env::args().nth(2).as_deref().and_then(View::from_view_flag)
    } else {
        None
    };
    let persisted = state::load_state();
    let mut view = pinned.unwrap_or(if persisted.merged {
        persisted.active
    } else {
        View::Explorer
    });
    let mut sync = if pinned.is_none() && persisted.merged {
        std::env::current_dir()
            .ok()
            .and_then(|root| SyncSession::connect(&root, true))
    } else {
        None
    };
    if let Some(session) = sync.as_mut() {
        if let Some(shared) = session.poll() {
            view = shared.active;
        }
        if session.pane_focused() {
            session.note_focus_gained();
        }
    }

    // ONE terminal session for every view: switching drops the old view's
    // state and draws the other in the same alternate screen — instant, and
    // the shell prompt underneath never flashes through.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::Purge),
        crossterm::cursor::MoveTo(0, 0),
    );
    // A TUI's colors are interface, not pipeable output: ignore NO_COLOR,
    // which otherwise leaks in whenever the herdr server was (re)started
    // from an agent shell (Claude Code's tool env sets it) and silently
    // turns every pane we draw monochrome.
    crossterm::style::force_color_output(true);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableFocusChange);
    // First run on a machine without a Nerd Font: offer to install one
    // before any icons render. The prompt stamps the pane's identity token
    // itself (the app loops haven't started yet, and a token-less pane gets
    // REPLACE-killed by the corpse rule while the user reads the prompt).
    herdr_sidebar::fontsetup::maybe_prompt(&mut terminal, view, persisted.merged)?;
    let result = loop {
        let exit = match view {
            View::Explorer => run_explorer(&mut terminal, &mut sync),
            View::SourceControl => run_scm(&mut terminal, &mut sync),
            View::Search => run_search(&mut terminal, &mut sync),
        };
        match exit {
            Ok(Exit::Quit) => break Ok(()),
            Ok(Exit::Switch) => {
                view = sync
                    .as_ref()
                    .and_then(SyncSession::latest)
                    .map(|state| state.active)
                    .unwrap_or_else(|| state::load_state().active);
            }
            Err(e) => break Err(e),
        }
    };
    let _ = crossterm::execute!(std::io::stdout(), DisableFocusChange, DisableMouseCapture);
    ratatui::restore();
    result
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

/// The explorer's event loop: short poll so the liveness heartbeat keeps
/// stamping even while idle.
fn run_explorer(
    terminal: &mut ratatui::DefaultTerminal,
    sync: &mut Option<SyncSession>,
) -> std::io::Result<Exit> {
    let root = std::env::current_dir()?;
    let mut app = explorer_app::App::new(root);
    configure_sync(sync, &app.root(), app.workspace_sync_enabled());
    let mut preview = viewer::InlinePreview::for_current_pane();
    let mut target_width = None;
    let mut width_dirty = false;
    let mut ignore_resize_until = None;
    let mut needs_initial_publish = sync
        .as_ref()
        .and_then(SyncSession::latest)
        .and_then(|state| state.explorer.as_ref())
        .is_none();
    if let Some(shared) = sync.as_ref().and_then(SyncSession::latest).cloned()
        && shared.active == View::Explorer
    {
        if let Some(explorer) = &shared.explorer {
            app.apply_workspace_state(explorer);
        }
        target_width = Some(shared.width);
    }
    loop {
        configure_sync(sync, &app.root(), app.workspace_sync_enabled());
        if let Some(session) = sync.as_mut() {
            session.set_unified(app.workspace_sync_enabled());
            session.set_root(&app.root());
            if !app.has_overlay() && !preview.is_open()
                && let Some(shared) = session.poll()
            {
                if shared.active != View::Explorer {
                    return Ok(Exit::Switch);
                }
                if let Some(explorer) = &shared.explorer {
                    app.apply_workspace_state(explorer);
                }
                target_width = Some(shared.width);
            }
        }
        preview.sync();
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some((sidebar, viewer)) = preview.areas(area) {
                app.draw_in(frame, sidebar);
                preview.draw(frame, viewer);
            } else {
                app.draw(frame);
            }
        })?;
        let width = app.workspace_width();
        if !preview.is_open() {
            if let Some(target) = target_width.take() {
                if target > 0 && target != width {
                    app.apply_workspace_width(target);
                    ignore_resize_until = Some(Instant::now() + Duration::from_secs(1));
                }
                width_dirty = false;
            } else if needs_initial_publish {
                if let Some(session) = sync.as_mut() {
                    session.publish_explorer(View::Explorer, width, app.workspace_state());
                }
                needs_initial_publish = false;
            } else if width_dirty {
                if let Some(session) = sync.as_mut() {
                    session.publish_active(View::Explorer, width);
                }
                width_dirty = false;
            }
        }
        let before_root = app.root();
        let before = app.workspace_state();
        // 500ms: quick enough that a finished folder pick lands promptly,
        // still cheap for the heartbeat.
        let had_event = if event::poll(Duration::from_millis(500))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                if ignore_resize_until.is_some_and(|until| Instant::now() <= until) {
                    ignore_resize_until = None;
                } else {
                    width_dirty = true;
                }
            }
            note_sync_focus(sync, &event);
            let exit = match event {
                Event::Key(key) if preview.is_open() && !app.has_overlay() => {
                    preview.on_key(key);
                    None
                }
                Event::Mouse(mouse) if preview.owns_mouse(&mouse) => {
                    preview.on_mouse(mouse);
                    None
                }
                Event::Key(key) => {
                    preview.claim_focus();
                    app.on_key(key)
                }
                Event::Mouse(mouse) => {
                    preview.observe_mouse();
                    let exit = app.on_mouse(mouse);
                    preview.sync();
                    if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        preview.claim_focus();
                    }
                    exit
                }
                Event::FocusGained => {
                    preview.claim_focus();
                    None
                }
                Event::FocusLost => {
                    preview.on_focus_lost();
                    None
                }
                _ => None, // resize simply falls through to a redraw
            };
            if app.take_redraw_request() {
                terminal.clear()?;
            }
            if let Some(exit) = exit {
                if let Some(session) = sync.as_mut() {
                    session.set_root(&app.root());
                    let active = if exit == Exit::Switch {
                        state::load_state().active
                    } else {
                        View::Explorer
                    };
                    session.publish_explorer(
                        active,
                        app.workspace_width(),
                        app.workspace_state(),
                    );
                    if exit == Exit::Quit {
                        session.clear_focus();
                    }
                }
                return Ok(exit);
            }
            true
        } else {
            app.poll_picker();
            false
        };
        app.heartbeat();
        preview.sync();
        preview.tick();
        let after_root = app.root();
        let after = app.workspace_state();
        let can_publish = had_event
            || sync.as_ref().is_some_and(SyncSession::pane_focused);
        if (before_root != after_root || before != after) && can_publish
            && let Some(session) = sync.as_mut()
        {
            session.set_root(&after_root);
            session.publish_explorer(View::Explorer, app.workspace_width(), after);
        }
    }
}

/// The source-control view's event loop: poll + tick so external changes and
/// finished background work (✧ suggestions, syncs) show up on their own.
fn run_scm(
    terminal: &mut ratatui::DefaultTerminal,
    sync: &mut Option<SyncSession>,
) -> std::io::Result<Exit> {
    let cwd = std::env::current_dir()?;
    let mut app = scm_app::App::new(cwd);
    configure_sync(sync, &app.root(), app.workspace_sync_enabled());
    let mut preview = viewer::InlinePreview::for_current_pane();
    let mut target_width = None;
    let mut width_dirty = false;
    let mut ignore_resize_until = None;
    let mut last_refresh = Instant::now();
    let mut needs_initial_publish = sync
        .as_ref()
        .and_then(SyncSession::latest)
        .and_then(|state| state.scm.as_ref())
        .is_none();
    if let Some(shared) = sync.as_ref().and_then(SyncSession::latest).cloned()
        && shared.active == View::SourceControl
    {
        if let Some(scm) = &shared.scm {
            app.apply_workspace_state(scm);
        }
        target_width = Some(shared.width);
    }
    loop {
        configure_sync(sync, &app.root(), app.workspace_sync_enabled());
        if let Some(session) = sync.as_mut() {
            session.set_unified(app.workspace_sync_enabled());
            session.set_root(&app.root());
            if !app.has_overlay() && !preview.is_open()
                && let Some(shared) = session.poll()
            {
                if shared.active != View::SourceControl {
                    return Ok(Exit::Switch);
                }
                if let Some(scm) = &shared.scm {
                    app.apply_workspace_state(scm);
                }
                target_width = Some(shared.width);
            }
        }
        preview.sync();
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some((sidebar, viewer)) = preview.areas(area) {
                app.draw_in(frame, sidebar);
                preview.draw(frame, viewer);
            } else {
                app.draw(frame);
            }
        })?;
        let width = app.workspace_width();
        if !preview.is_open() {
            if let Some(target) = target_width.take() {
                if target > 0 && target != width {
                    app.apply_workspace_width(target);
                    ignore_resize_until = Some(Instant::now() + Duration::from_secs(1));
                }
                width_dirty = false;
            } else if needs_initial_publish {
                if let Some(session) = sync.as_mut() {
                    session.publish_scm(View::SourceControl, width, app.workspace_state());
                }
                needs_initial_publish = false;
            } else if width_dirty {
                if let Some(session) = sync.as_mut() {
                    session.publish_active(View::SourceControl, width);
                }
                width_dirty = false;
            }
        }
        let before_root = app.root();
        let before = app.workspace_state();
        let had_event = if event::poll(Duration::from_millis(500))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                if ignore_resize_until.is_some_and(|until| Instant::now() <= until) {
                    ignore_resize_until = None;
                } else {
                    width_dirty = true;
                }
            }
            note_sync_focus(sync, &event);
            let exit = match event {
                Event::Key(key) if preview.is_open() && !app.has_overlay() => {
                    preview.on_key(key);
                    None
                }
                Event::Mouse(mouse) if preview.owns_mouse(&mouse) => {
                    preview.on_mouse(mouse);
                    None
                }
                Event::Key(key) => {
                    preview.claim_focus();
                    app.on_key(key)
                }
                Event::Mouse(mouse) => {
                    preview.observe_mouse();
                    let exit = app.on_mouse(mouse);
                    preview.sync();
                    if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        preview.claim_focus();
                    }
                    exit
                }
                Event::FocusGained => {
                    preview.claim_focus();
                    None
                }
                Event::FocusLost => {
                    app.on_focus_lost();
                    preview.on_focus_lost();
                    None
                }
                _ => None,
            };
            if app.take_redraw_request() {
                terminal.clear()?;
            }
            if let Some(exit) = exit {
                if let Some(session) = sync.as_mut() {
                    session.set_root(&app.root());
                    let active = if exit == Exit::Switch {
                        state::load_state().active
                    } else {
                        View::SourceControl
                    };
                    session.publish_scm(
                        active,
                        app.workspace_width(),
                        app.workspace_state(),
                    );
                    if exit == Exit::Quit {
                        session.clear_focus();
                    }
                }
                return Ok(exit);
            }
            true
        } else {
            app.poll_picker();
            false
        };
        if last_refresh.elapsed() >= REFRESH_EVERY {
            app.poll_picker();
            app.tick();
            last_refresh = Instant::now();
        }
        app.heartbeat();
        preview.sync();
        preview.tick();
        let after_root = app.root();
        let after = app.workspace_state();
        let can_publish = had_event
            || sync.as_ref().is_some_and(SyncSession::pane_focused);
        if (before_root != after_root || before != after) && can_publish
            && let Some(session) = sync.as_mut()
        {
            session.set_root(&after_root);
            session.publish_scm(View::SourceControl, app.workspace_width(), after);
        }
    }
}

fn run_search(
    terminal: &mut ratatui::DefaultTerminal,
    sync: &mut Option<SyncSession>,
) -> std::io::Result<Exit> {
    let cwd = std::env::current_dir()?;
    let mut app = search_app::App::new(cwd);
    configure_sync(sync, &app.root(), app.workspace_sync_enabled());
    let mut preview = viewer::InlinePreview::for_current_pane();
    let mut target_width = None;
    let mut width_dirty = false;
    let mut ignore_resize_until = None;
    let mut needs_initial_publish = sync
        .as_ref()
        .and_then(SyncSession::latest)
        .and_then(|state| state.search.as_ref())
        .is_none();
    if let Some(shared) = sync.as_ref().and_then(SyncSession::latest).cloned()
        && shared.active == View::Search
    {
        if let Some(search) = &shared.search {
            app.apply_workspace_state(search);
        }
        target_width = Some(shared.width);
    }
    loop {
        configure_sync(sync, &app.root(), app.workspace_sync_enabled());
        if let Some(session) = sync.as_mut() {
            session.set_unified(app.workspace_sync_enabled());
            session.set_root(&app.root());
            if !app.has_overlay() && !preview.is_open()
                && let Some(shared) = session.poll()
            {
                if shared.active != View::Search {
                    return Ok(Exit::Switch);
                }
                if let Some(search) = &shared.search {
                    app.apply_workspace_state(search);
                }
                target_width = Some(shared.width);
            }
        }
        preview.sync();
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some((sidebar, viewer)) = preview.areas(area) {
                app.draw_in(frame, sidebar);
                preview.draw(frame, viewer);
            } else {
                app.draw(frame);
            }
        })?;
        let width = app.workspace_width();
        if !preview.is_open() {
            if let Some(target) = target_width.take() {
                if target > 0 && target != width {
                    app.apply_workspace_width(target);
                    ignore_resize_until = Some(Instant::now() + Duration::from_secs(1));
                }
                width_dirty = false;
            } else if needs_initial_publish {
                if let Some(session) = sync.as_mut() {
                    session.publish_search(View::Search, width, app.workspace_state());
                }
                needs_initial_publish = false;
            } else if width_dirty {
                if let Some(session) = sync.as_mut() {
                    session.publish_active(View::Search, width);
                }
                width_dirty = false;
            }
        }
        let before_root = app.root();
        let before = app.workspace_state();
        let had_event = if event::poll(Duration::from_millis(100))? {
            let event = event::read()?;
            if matches!(event, Event::Resize(..)) {
                if ignore_resize_until.is_some_and(|until| Instant::now() <= until) {
                    ignore_resize_until = None;
                } else {
                    width_dirty = true;
                }
            }
            note_sync_focus(sync, &event);
            let exit = match event {
                Event::Key(key) if preview.is_open() && !app.has_overlay() => {
                    preview.on_key(key);
                    None
                }
                Event::Mouse(mouse) if preview.owns_mouse(&mouse) => {
                    preview.on_mouse(mouse);
                    None
                }
                Event::Key(key) => {
                    preview.claim_focus();
                    app.on_key(key)
                }
                Event::Mouse(mouse) => {
                    preview.observe_mouse();
                    let exit = app.on_mouse(mouse);
                    preview.sync();
                    if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        preview.claim_focus();
                    }
                    exit
                }
                Event::FocusGained => {
                    preview.claim_focus();
                    None
                }
                Event::FocusLost => {
                    app.on_focus_lost();
                    preview.on_focus_lost();
                    None
                }
                _ => None,
            };
            if app.take_redraw_request() {
                terminal.clear()?;
            }
            if let Some(exit) = exit {
                if let Some(session) = sync.as_mut() {
                    session.set_root(&app.root());
                    let active = if exit == Exit::Switch {
                        state::load_state().active
                    } else {
                        View::Search
                    };
                    session.publish_search(active, app.workspace_width(), app.workspace_state());
                    if exit == Exit::Quit {
                        session.clear_focus();
                    }
                }
                return Ok(exit);
            }
            true
        } else {
            false
        };
        app.tick();
        app.heartbeat();
        preview.sync();
        preview.tick();
        let after_root = app.root();
        let after = app.workspace_state();
        let can_publish = had_event || sync.as_ref().is_some_and(SyncSession::pane_focused);
        if (before_root != after_root || before != after) && can_publish
            && let Some(session) = sync.as_mut()
        {
            session.set_root(&after_root);
            session.publish_search(View::Search, app.workspace_width(), after);
        }
    }
}

fn note_sync_focus(sync: &mut Option<SyncSession>, event: &Event) {
    let Some(session) = sync.as_mut() else { return };
    match event {
        Event::FocusGained => session.note_focus_gained(),
        Event::FocusLost => session.note_focus_lost(),
        Event::Key(_) | Event::Mouse(crossterm::event::MouseEvent { kind: MouseEventKind::Down(_), .. }) => {
            session.note_interaction();
        }
        _ => {}
    }
}

fn configure_sync(sync: &mut Option<SyncSession>, root: &std::path::Path, enabled: bool) {
    if enabled {
        if sync.is_none() {
            *sync = SyncSession::connect(root, true);
            if let Some(session) = sync.as_mut()
                && session.pane_focused()
            {
                session.note_focus_gained();
            }
        }
    } else if let Some(mut session) = sync.take() {
        session.set_unified(false);
    }
}
