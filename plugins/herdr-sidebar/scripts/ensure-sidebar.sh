#!/usr/bin/env bash
# ensure-explorer.sh — unix [[events]] hook body: make sure the FOCUSED tab has
# an Explorer pane docked on the left, WITHOUT stealing the user's focus.
#
# Runs on tab.focused / workspace.focused, so it must be idempotent and quiet:
# already present → exit; else open unfocused (see ensure-explorer.ps1 for the
# focus-follows-the-slot rationale behind the final `pane focus`).
set -uo pipefail

herdr_bin="${HERDR_BIN_PATH:-herdr}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
bin="$script_dir/../target/release/herdr-sidebar"
[ -x "$bin" ] || exit 0

# "Auto-open sidebar: off" (⚙ Settings): hooks leave closed tabs alone; only
# the explicit open-sidebar toggle docks one (issue #8). Checked before the
# lock so a disabled hook never contends with a user toggle.
[ "$("$bin" --auto-open 2>/dev/null || echo on)" = "off" ] && exit 0

# Focus events arrive in bursts (tab.focused + workspace.focused for one switch)
# and concurrent ensures each open an explorer — serialize with an atomic mkdir
# lock. Losing the race skips this ensure; the next focus event re-fires it.
lock_dir="${TMPDIR:-/tmp}/herdr-sidebar-ensure.lock"
if ! mkdir "$lock_dir" 2>/dev/null; then
  # Break locks older than 30s (a crashed ensure), otherwise yield.
  now="$(date +%s)"
  born="$(stat -c %Y "$lock_dir" 2>/dev/null || stat -f %m "$lock_dir" 2>/dev/null || echo "$now")"
  [ $((now - born)) -gt 30 ] || exit 0
  rm -rf "$lock_dir" 2>/dev/null
  mkdir "$lock_dir" 2>/dev/null || exit 0
fi
trap 'rmdir "$lock_dir" 2>/dev/null' EXIT

# Snapshot AFTER acquiring the lock, so a just-finished ensure's rename is visible.
panes="$("$herdr_bin" pane list 2>/dev/null || true)"
[ -n "$panes" ] || exit 0

decision="$(printf '%s' "$panes" | "$bin" --launch-decision 2>/dev/null || true)"
if [[ "$decision" == "REPLACE "* ]]; then
  # Closing the focused corpse invalidates this pane-list snapshot. Refresh it
  # before deriving the split target; otherwise open-plan uses a dead pane id.
  corpse="${decision#REPLACE }"
  "$herdr_bin" pane close "$corpse" >/dev/null 2>&1 || true
  panes="$("$herdr_bin" pane list 2>/dev/null || true)"
  [ -n "$panes" ] || exit 0
  decision="$(printf '%s' "$panes" | "$bin" --launch-decision 2>/dev/null || true)"
fi
[ "$decision" = "OPEN" ] || exit 0

# Respect a tab the user toggled closed (open-explorer.sh writes the marker) —
# otherwise the very next focus event would reopen what they just closed.
snooze_dir="${TMPDIR:-/tmp}/herdr-sidebar-snooze"
tab="$(printf '%s' "$panes" | "$bin" --focused-tab 2>/dev/null || true)"
[ -n "$tab" ] && [ -f "$snooze_dir/${tab//:/_}" ] && exit 0

fp="$(printf '%s' "$panes" | "$bin" --focused-pane 2>/dev/null || true)"
fid="${fp%%	*}"
fcwd="${fp#*	}"
[ -n "$fid" ] || exit 0

target="$fid"
ratio="0.25"
plan="$("$herdr_bin" pane layout --pane "$fid" 2>/dev/null | "$bin" --open-plan 2>/dev/null || true)"
if [ -n "$plan" ]; then
  target="${plan%%	*}"
  ratio="${plan#*	}"
fi

out="$("$herdr_bin" plugin pane open --plugin herdr-sidebar \
  --entrypoint sidebar --placement split --target-pane "$target" --direction right \
  ${fcwd:+--cwd "$fcwd"} --no-focus 2>/dev/null || true)"
np="$(printf '%s' "$out" | sed -n 's/.*"pane_id":"\([^"]*\)".*/\1/p' | head -n1)"
[ -n "$np" ] || exit 0

# Direct manifest argv launch avoids exposing a shell prompt/command. Restore
# the open-plan share after plugin pane open's default 50/50 split.
"$herdr_bin" pane swap --source-pane "$np" --target-pane "$target" >/dev/null 2>&1 || true
amount="$(awk -v ratio="$ratio" 'BEGIN { d = 0.5 - ratio; if (d > 0.000001) printf "%.6f", d }')"
if [ -n "$amount" ]; then
  "$herdr_bin" pane resize --pane "$np" --direction left --amount "$amount" >/dev/null 2>&1 || true
fi

# Hand focus back if the swap left it on the explorer (focus follows the slot).
if [ "$target" = "$fid" ]; then
  "$herdr_bin" pane focus --direction right --pane "$np" >/dev/null 2>&1 || true
fi
exit 0
