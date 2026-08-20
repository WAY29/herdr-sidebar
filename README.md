<div align="center">

# Herdr Sidebar

### The sidebar your terminal was missing — inspired by VS Code.

A file explorer, VS Code-style full-text search, Quick Open, and a full source-control panel
in one dockable [herdr](https://github.com/herdrdev/herdr) pane — activity-bar switching,
mouse everywhere, AI-drafted commit messages, and a file preview that takes everything
beside the sidebar until Esc puts your panes back.

<img alt="Rust" src="https://img.shields.io/badge/Rust-self--contained_crate-orange?logo=rust&logoColor=white">
<img alt="herdr" src="https://img.shields.io/badge/herdr-%E2%89%A5%200.7-5865a3">
<img alt="Platforms" src="https://img.shields.io/badge/Windows%20%C2%B7%20macOS%20%C2%B7%20Linux-supported-2ea44f">
<img alt="CI" src="https://github.com/way29/herdr-sidebar/actions/workflows/ci.yml/badge.svg">
<img alt="License" src="https://img.shields.io/badge/license-MIT-blue">

<br><br>

<img src="plugins/herdr-sidebar/docs/media/hero.png" alt="The sidebar docked beside a 2x2 fleet of Claude Code and Codex agents" width="920">

</div>

That's the sidebar on the left and a 2×2 fleet of Claude Code and Codex agents beside it —
the workflow herdr is built for. If you've ever alt-tabbed out of your terminal just to
inspect a tree, search a project, review a diff, or see what's staged, this closes that loop.
The sidebar docks on the left of every herdr tab, restores itself on focus, and is driven
entirely by click or keystroke.

---

## One pane. Three views. Zero friction.

The activity bar at the top flips between **Explorer**, **Source Control**, and **Search** —
*in process*, so switching is instant: no respawn, no flicker, no lost state on the way.
All three views ship in one Rust binary.

Unified sidebars also stay visually aligned across tabs in the same workspace when they point
at the same folder: current view, selection, scroll anchor, expanded trees and drawers, width,
Sidebar focus, and Search inputs follow the active tab. Search results, previews, menus, hover
state, and commit drafts remain local to each tab.

### 🗂 The Explorer

A real tree, not a directory dump:

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/preview.png" alt="Explorer view with a live file preview open beside the tree" width="920">
</div>

- Disclosure chevrons, nested indentation, and **two icon themes** — colored Nerd Font
  glyphs (Atom-Material style) or emoji, toggled live. The sidebar auto-picks: material
  when a Nerd Font is installed, emoji otherwise — and on first run without one it
  offers to download and install JetBrainsMono Nerd Font for you (Windows, macOS,
  Linux). If the theme ever guesses wrong (icons showing as ⌷ tofu boxes), press `i`
  once; the choice persists.
- **Click a file and it opens** across every column beside the sidebar. Existing panes
  stay untouched behind the preview; Esc reveals them exactly as they were. Line numbers,
  scrolling, and binary files are handled, and clicking another file updates in place.
- **Drag to select exact source characters** in ordinary files and structured diffs. Copy the
  raw selection, or attach comments and send the accumulated review to an agent without editing
  the file. Saved ranges stay highlighted in Preview and reveal their temporary Comment in a
  tooltip on hover. Tabs, Unicode width, diff gutters, and wrapped rows stay mapped to the source
  text.
- **Double-click folders** to fold, hover highlights, mouse wheel, a full root-relative path
  tooltip for every file and folder, and a hover **⋯ menu button**: New File, New Folder,
  Open with Default App (files
  only — hands the file to the OS-associated app, like a double click in the file
  manager), Rename, Delete, Copy Path / Relative Path, Reveal in File Explorer.
- Dotfiles toggle, live refresh, and `b` / the bottom-right `«` button to hide the Sidebar for
  the current tab. Invoke the toggle action to bring it back.
- Prefer the sidebar closed? Toggle "Auto-open sidebar" off in ⚙ Settings and it stays
  closed until you invoke the open-sidebar action yourself.

### 🔎 Search

VS Code-style project search without leaving the Sidebar:

- Type a query and Search runs automatically after a short debounce; `Enter` runs it
  immediately. A new query cancels the previous `rg` process before streaming replacement
  results.
- Toggle regex, case sensitivity, and whole-word matching with the controls below the query.
- Expand **Filters** for comma-separated include and exclude globs relative to the Sidebar root.
- Results stream from `rg --json --line-buffered`, grouped by file with the filename first and
  its directory path after it.
- Click a result or press `Enter` to open Preview at the matching line, using the same match
  highlight in the result row and source file.

Search is part of the unified Sidebar. Separated mode keeps Explorer and Source Control as the
two independent panes.

### Quick Open

Invoke the Quick Open action to show a centered popup over the current tab. It indexes the
current Sidebar root with `rg --files`, then fuzzy-matches file and folder names as you type.
Choosing a result switches the Sidebar to Explorer, expands every parent folder, and selects
the target; files also open in the existing in-process Preview. If `rg` is unavailable, the
popup shows the same Retry screen as Search.

### 🔀 Source Control

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/source-control.png" alt="Source control: multi-repo staging, per-repo commit boxes, history drawers" width="920">
</div>

Everything you reach for in an editor's source-control panel, in a terminal pane:

- **Click a change, see the diff** — every changed file opens its colored `git diff` in
  the in-process Preview (staged vs working tree respected, untracked shown as additions), and
  the diff live-updates while you edit.
- **Stage, unstage, discard, commit** — by key or click, with Staged/Changes sections,
  count badges and familiar per-file status letters; hover a file or folder for its full path and
  green/red `+N -N` line totals.
- **Tree or list layout** — changed and historical files default to a compact folder tree,
  including deleted paths; switch the global SCM file view in ⚙ Settings when a flat list
  is more useful. Folder rows fold and bulk-stage/unstage; hover `⋯` opens path-aware file
  and folder menus without taking over herdr's normal pane right click.
- **Collapse or expand repository content** — the title action toggles between Collapse All and
  Expand All for the repository area: message box, Commit, Sync Changes, repository sections,
  and Staged/Changes trees. A heavy divider separates that area from Graph and the later drawers,
  whose expansion is unchanged.
- **Project headers keep actions reachable** — hover the current project's SCM header for
  Refresh and Collapse/Expand All. Long names truncate before the fixed action area; moving to
  another row hides it immediately, and the hovered repository header gets a full-row highlight.
  With one repository, clicking the main title itself folds or opens that repository just like a
  repository header in multi-repo mode.
- **Clear commit controls** — Commit and optional Sync Changes use compact one-row color blocks;
  the message box, buttons, and Changes section sit together without extra layout or blank
  padding rows.
- Structured diffs hide long unchanged sections by default. Clicking an unmodified-lines fold
  reveals that context without moving the current viewport, and hidden rows skip unnecessary
  rendered-span allocation. Toggle **Hide unmodified lines** in ⚙ Settings to show all retained
  context immediately and center the first change.
- **✧ AI commit messages** — the sparkle button sends the pending diff to your local
  `claude` CLI and drops a drafted subject line into the message box. No claude? A clean
  filename-based fallback kicks in. Never blocks the UI.
- **Sync Changes** — a `⇅ 1↑ 2↓` button appears when you're ahead/behind upstream; one
  press runs `pull --rebase --autostash` + `push` in the background.
- **Multi-repo** — child repositories are auto-discovered, each with its
  own header (branch, dirty `*`, sync/commit icons), message box, and Commit button. Child repo
  headers use the same hover, truncation, and hitbox model as the main project header, with
  Sync/Commit as their actions. Full-width dividers frame the repository area,
  separate every repository block, and mark the shared history drawers below them.
- **History drawers**: GRAPH, COMMITS, FILE HISTORY (follows renames), BRANCHES, REMOTES,
  STASHES, TAGS. Commits, branches, stashes, and tags expand into file trees whose leaves
  open the same structured, syntax-highlighted diff as live changes.
- **Extended syntax highlighting** — Preview and structured diffs use two-face's bat grammar set,
  plus an embedded go-zero API grammar for `.api` files.
- **Auto-refreshing** — commits and edits made anywhere show up within seconds.

## Prefer two panels? Take two panels.

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/separated.png" alt="Separated mode: Source Control and Explorer as independent panes" width="920">
</div>

The ⚙ settings modal — mouse-toggleable like everything else — flips between:

- **Unified sidebar**: Explorer, Source Control, and Search share one pane; the activity bar
  switches instantly.
- **Separated panels**: Explorer and Source Control as independent side-by-side panes —
  each keeping the full sidebar width. Search is available in unified mode only.

<div align="center">
<img src="plugins/herdr-sidebar/docs/media/settings.png" alt="The settings modal" width="920">
</div>

Icon theme, dotfile visibility, and the full hotkey reference live in the same modal
(with a toggle if you'd rather keep the key hints pinned to the sidebar's footer), and
every choice persists across restarts. However you split it, the dock takes care of itself: a focus hook
re-docks the sidebar in any tab or workspace that's missing one — new project, new
worktree, new window, it's just *there*.

## Install

This fork is installed from a local checkout:

```bash
git clone https://github.com/way29/herdr-sidebar.git
cd herdr-sidebar/plugins/herdr-sidebar
cargo build --release
herdr plugin link .
```

Open it with an action (or just focus a tab and let the hook dock it):

```
herdr plugin action invoke herdr-sidebar.open-sidebar-windows   # windows
herdr plugin action invoke herdr-sidebar.open-sidebar           # linux / macos
```

**Requirements:** Rust to build and herdr ≥ 0.7. Search additionally requires
[`ripgrep`](https://github.com/BurntSushi/ripgrep) (`rg`) on `PATH`; Search and Quick Open show
a Retry screen when it is unavailable. **Recommended:** a Nerd Font terminal face for the
material icons — without one the sidebar auto-starts in its emoji theme, which renders in
any font. Note Windows Terminal's bundled Cascadia does NOT include the icon glyphs; grab a
patched font in one command and select it in your terminal profile:

```
winget install DEVCOM.JetBrainsMonoNerdFont
```

(or any font from [nerdfonts.com](https://www.nerdfonts.com/font-downloads), e.g.
CaskaydiaCove). Also recommended: the
[`claude` CLI](https://claude.com/claude-code) for ✧ commit messages.

## Keys

Herdr plugins cannot declare global default keybindings in their manifest. To use VS Code's
`F1` binding for Quick Open, add the matching entry to your Herdr `config.toml`:

```toml
[[keys.command]]
key = "f1"
type = "plugin_action"
command = "herdr-sidebar.quick-open" # linux / macos
description = "Quick Open"
```

On Windows, use `command = "herdr-sidebar.quick-open-windows"` instead. This is a direct
global binding, so it works regardless of which pane owns focus.

In unified mode, `1`, `2`, and `3` switch to Explorer, Source Control, and Search. `s` opens
Settings and `b` hides the Sidebar when focus is not inside a Search text field.

| Explorer | What it does |
|---|---|
| `↑↓` / `jk` | move selection |
| `←→` / `hl` | fold / unfold |
| `⏎` / `Space` | toggle folder · preview file |
| `r` | refresh |
| `.` | toggle dotfiles |
| `c` | change root folder |
| `i` | toggle icon theme |
| `q` | quit |

| Source Control | What it does |
|---|---|
| `↑↓` / `jk` | move selection |
| `←→` / `hl` | fold / unfold tree rows |
| `⏎` / `Space` | stage / unstage · fold folder |
| `a` / `u` | stage all / unstage all |
| `c` | focus commit message |
| `A` | ✧ suggest commit message |
| `o` | open selected diff |
| `S` | sync upstream changes |
| `r` | refresh |
| `q` | quit |

| Preview | What it does |
|---|---|
| Left drag | select characters in source files and structured diffs |
| `Cmd+C` (macOS), `Ctrl+C` (elsewhere) | copy the raw selection |
| `c` | comment on the selection; hover the saved highlight to read it |
| `y` | copy all saved comments |
| `s` | send all saved comments to an Agent in the same tab |
| `d` | clear all saved comments for the current file |
| `Esc` | clear selection, cancel a dialog, or close Preview |
| `q` | close Preview |

Comment sending only targets Agent panes in the same tab as Preview. One target sends directly;
multiple targets open a chooser. Saved comments and their highlights are temporary Preview state;
an active selection temporarily shows `c Comment` and the platform copy shortcut. Saving returns to
the outer `drag to select text` footer, which appends `y Copy`, `s Send Agent`, and `d Clear N` once
comments exist. Comments are removed after a successful copy/send, or per file with `d`, and are
never written to the source file.
Selection, saved-comment, and tooltip accent colors follow the active Diff Theme.

File hyperlinks handled by the patched Herdr host open changed files in Source Control: the
owning repository and Staged/Changes tree are expanded, the file row is selected, and Preview
shows its structured diff. Worktree/untracked changes take priority over staged-only changes. A
linked changed line is centered and highlighted when visible in the diff; clean files still open
and select normally in Explorer.

| Search | What it does |
|---|---|
| Type in query/filter fields | search automatically after 250 ms |
| `Tab` / `Shift+Tab` | move between query, toggles, filters, and results |
| `⏎` | search immediately · toggle option · open selected result |
| `Esc` | clear the query when its field is focused |
| `↑↓` / `jk` | move through results |
| `←→` / `hl` | choose regex, case, or whole-word toggle |

…and the mouse for all of it: click, double-click, scroll, hover, and `⋯` menus.

## Actions

| Action | What it does |
|---|---|
| `open-sidebar` / `open-sidebar-windows` | Toggle the sidebar: open left-docked / focus / close |
| `quick-open` / `quick-open-windows` | Open the centered file/folder palette for the current Sidebar root |
| `open-git` / `open-git-windows` | Toggle a separate Source Control pane (separated mode) |
| `redeploy` / `redeploy-windows` | After a rebuild: replace old Sidebar panes; other workspaces re-dock on next focus |

## Under the hood

- **One self-contained Rust crate and binary** — all three views share the same runtime;
  separated Explorer and Source Control panes use that binary pinned with `--view`.
- Runtime pane control uses **herdr's socket API directly**. Unix launchers open manifest
  entrypoints without exposing shell commands; Windows focus hooks use a windowless
  GUI-subsystem sidecar so nothing flashes a console window.
- Unix focus hooks keep their launch lock through the new pane's first heartbeat, so the
  focus-event burst from docking cannot mistake that fresh Sidebar for a restored dead pane.
- The left dock survives real layouts — split-the-leftmost + swap, full-height repair,
  ratio-aware resizing — all unit-tested against herdr's actual JSON.
- Windows quirks (exe locking, PowerShell 5.1 BOMs, double-width Nerd Font glyphs) are
  handled, and the hard-won findings are documented in [`CLAUDE.md`](CLAUDE.md).

---

<div align="center">
<sub>Screenshots: herdr on Windows Terminal, CaskaydiaCove Nerd Font.</sub>
</div>
