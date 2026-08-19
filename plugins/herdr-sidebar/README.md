# herdr-sidebar

**The sidebar your terminal was missing** — a VS Code-inspired file explorer, full-text
search, and source-control panel in one dockable herdr pane.

<img src="docs/media/hero.png" alt="The sidebar: explorer view with a live file preview beside it" width="860">

**The full tour lives in the [repo README](../../README.md)** — features, screenshots,
keys, and settings.

## Install

Install this fork from a local checkout:

```bash
git clone https://github.com/way29/herdr-sidebar.git
cd herdr-sidebar/plugins/herdr-sidebar
cargo build --release
herdr plugin link .
```

Open it (or just focus a tab — the hook docks it):

```
herdr plugin action invoke herdr-sidebar.open-sidebar-windows   # windows
herdr plugin action invoke herdr-sidebar.open-sidebar           # linux / macos
```
