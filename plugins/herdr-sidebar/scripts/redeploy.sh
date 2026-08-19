#!/usr/bin/env bash
# redeploy.sh -- refresh every workspace onto the latest plugin builds.
# See redeploy.ps1 for the full story; this is the unix twin.
set -u

HERDR_BIN="${HERDR_BIN_PATH:-herdr}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
focus_info="$("$HERDR_BIN" pane list | python3 -c '
import json, os, sys
panes = json.load(sys.stdin)["result"]["panes"]
try:
    context = json.loads(os.environ.get("HERDR_PLUGIN_CONTEXT_JSON", "{}"))
except json.JSONDecodeError:
    context = {}
focused_id = context.get("focused_pane_id")
focused = next((pane for pane in panes if pane.get("pane_id") == focused_id), None)
if focused is None:
    focused = next((pane for pane in panes if pane.get("focused")), None)
labels = {"Explorer", "Source Control", "Sidebar", "Preview"}
if focused is not None:
    tokens = focused.get("tokens") or {}
    is_plugin = focused.get("label") in labels or any(k.startswith("herdr-sidebar") for k in tokens)
    print(
        focused["pane_id"],
        context.get("tab_id") or focused["tab_id"],
        int(is_plugin and not focused.get("agent")),
        sep="\t",
    )
')"
IFS=$'\t' read -r focused_pane focused_tab focused_plugin <<< "$focus_info"

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
        is_plugin = pane.get("label") in labels or any(k.startswith("herdr-sidebar") for k in tokens)
        if is_plugin and not pane.get("agent"):
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

HERDR_BIN_PATH="$HERDR_BIN" bash "$script_dir/open-sidebar.sh" >/dev/null 2>&1
restore_pane="$focused_pane"
if [ "${focused_plugin:-0}" = "1" ]; then
  restore_pane="$("$HERDR_BIN" pane list | python3 -c '
import json, sys
tab_id = sys.argv[1]
labels = {"Explorer", "Source Control", "Sidebar", "Preview"}
for pane in json.load(sys.stdin)["result"]["panes"]:
    tokens = pane.get("tokens") or {}
    is_plugin = pane.get("label") in labels or any(k.startswith("herdr-sidebar") for k in tokens)
    if pane.get("tab_id") == tab_id and is_plugin and not pane.get("agent"):
        print(pane["pane_id"])
        break
' "$focused_tab")"
fi
if [ -n "$restore_pane" ]; then
  "$HERDR_BIN" pane zoom "$restore_pane" --on >/dev/null 2>&1 || true
  "$HERDR_BIN" pane zoom "$restore_pane" --off >/dev/null 2>&1 || true
fi
echo 'redeploy complete - other workspaces re-dock on next focus'
