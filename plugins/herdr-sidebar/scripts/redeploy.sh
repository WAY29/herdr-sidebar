#!/usr/bin/env bash
# redeploy.sh -- refresh every workspace onto the latest plugin builds.
# See redeploy.ps1 for the full story; this is the unix twin.
set -u

HERDR_BIN="${HERDR_BIN_PATH:-herdr}"

"$HERDR_BIN" workspace list | python3 -c '
import json, subprocess, sys

herdr = sys.argv[1]
labels = {"Explorer", "Source Control", "Sidebar", "Preview"}
workspaces = json.load(sys.stdin)["result"]["workspaces"]
for ws in workspaces:
    wid = ws["workspace_id"]
    out = subprocess.check_output([herdr, "pane", "list", "--workspace", wid])
    for pane in json.loads(out)["result"]["panes"]:
        tokens = pane.get("tokens") or {}
        if pane.get("label") in labels or any(k.startswith("herdr-aa") for k in tokens):
            subprocess.run([herdr, "pane", "close", pane["pane_id"]], capture_output=True)
            print("closed", wid, pane["pane_id"], pane.get("label"))
' "$HERDR_BIN"

# Match by process NAME, not full command line: `-f` would also kill
# launcher scripts (open-sidebar.sh etc.) and unrelated processes whose argv
# happens to contain the plugin path. Both Linux and macOS truncate `comm`
# to 15 visible characters, so `herdr-sidebar-ensure` (20 chars) shows up
# truncated as `herdr-sidebar-e` — match all three forms.
pkill -x 'herdr-sidebar' 2>/dev/null
pkill -x 'herdr-sidebar-ensure' 2>/dev/null
pkill -x 'herdr-sidebar-e' 2>/dev/null
"$HERDR_BIN" plugin action invoke herdr-sidebar.open-sidebar >/dev/null 2>&1
echo 'redeploy complete - other workspaces re-dock on next focus'
