#!/bin/sh
# `pick id <id>` opens on that agent wherever its row is, and
# `pick here`, opened from a pane whose agent has finished, shows the
# history and puts the cursor on that agent's row; opened from one whose
# agent was archived, it shows the archive and lands there too. Plain
# `pick` from the same panes opens the default view regardless. Rows
# carry the session the pane lives in as a column.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-here-history-test"
[ -z "$WASM" ] && WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

XDG_DATA_HOME=$(mktemp -d); export XDG_DATA_HOME
trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
shot() { [ -n "$SHOW" ] && { echo "--- $*"; screen; }; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
cursor_row() { screen | grep '▸' | head -1; }
# open_from <pane> [verb]: the picker from that pane, `pick` or `pick here`.
open_from() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command -t $1 agents '${2:-pick}'"; sleep 60 ) |
	    $TMUX -C attach -t alpha:0 >"$XDG_DATA_HOME/ctl.log" 2>&1 &
	CTL=$!
	i=0
	while [ "$i" -lt 12 ]; do
		sleep 0.4
		FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
		    awk '/plugin-mode/ { print $1 }')
		[ -n "$FORM" ] && break
		i=$((i + 1))
	done
	[ -n "$FORM" ] || { $TMUX list-panes -a -F '#{pane_id} #{pane_mode} #{pane_current_command}' >&2; grep -v '^%output' "$XDG_DATA_HOME/ctl.log" | tail -12 >&2; fail "picker did not open"; }
	sleep 0.5
}
close_picker() { keys q; kill $CTL 2>/dev/null; CTL=; sleep 0.5; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Window 0 hosts the picker (scrubbed environment, so whoever runs this
# is not an agent). Window 1's pane is an agent for two seconds, then the
# same pane prints a line (a pane's command is re-checked when it has
# output) and runs a plain command: the agent is retired, the pane stays.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 'env -i PATH=/bin:/usr/bin sleep 600' || fail "new-session"
$TMUX new-window -d -t alpha:1 \
    "sh -c 'AI_AGENT=claude sleep 2; echo over; exec env -i PATH=/bin:/usr/bin cat'" || fail "new-window"
# Window 3: the same, a second finished agent that stays plain (never
# archived), for the `pick id` cases at the end.
$TMUX new-window -d -t alpha:3 \
    "sh -c 'AI_AGENT=claude sleep 2; echo over; exec env -i PATH=/bin:/usr/bin cat'" || fail "new-window 3"
sleep 0.5
P=$($TMUX list-panes -t alpha:1 -F '#{pane_id}')
P0=$($TMUX list-panes -t alpha:0 -F '#{pane_id}')

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$WASM" || fail "load-plugin"
sleep 1
P3=$($TMUX list-panes -t alpha:3 -F '#{pane_id}')
$TMUX plugin-command -t "$P" agents "working"
$TMUX plugin-command -t "$P3" agents "working"
sleep 3.5

# The agent is gone from the live list: plain `pick` opens the default
# view, which has no row for it...
open_from "$P"
screen | grep -q '+history' && fail "plain pick opened the history"
screen | grep -q '▸' && fail "plain pick has a cursor row with nothing to land on"
close_picker
# ...and `pick here` opens the history for it.
open_from "$P" "pick here"
shot "from the finished pane"
screen | grep -q 'live' || fail "picker header missing"
# ...so the picker opened into the history, cursor on its row.
screen | grep -q '+history' || fail "the picker did not open into the history for a finished agent's pane"
cursor_row | grep -q 'claude' || fail "the cursor is not on the finished agent's row"
# The session column sits before the harness column.
cursor_row | grep -q 'alpha *claude *[0-9]' || fail "the row does not show the session before the harness"

# Archive it, close, reopen from the same pane: the archive view, cursor on it.
keys a
sleep 0.5
close_picker
open_from "$P"
screen | grep -q 'archive)' && fail "plain pick opened the archive"
close_picker
open_from "$P" "pick here"
shot "from the archived pane"
screen | grep -q 'archive)' || fail "the picker did not open into the archive for an archived agent's pane"
cursor_row | grep -q 'claude.*archived' || fail "the cursor is not on the archived agent's row"

# From a pane that never had an agent: the plain live list, no history.
close_picker
open_from "$P0" "pick here"
screen | grep -q '+history' && fail "a non-agent pane opened the history"

# A LIVE agent that is archived is out of the live list too: opened from
# its pane, the picker shows the archive with the cursor on it.
$TMUX new-window -d -t alpha:2 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-window 2"
sleep 1
P2=$($TMUX list-panes -t alpha:2 -F '#{pane_id}')
$TMUX plugin-command -t "$P2" agents "working"
sleep 0.5
close_picker
open_from "$P2"
cursor_row | grep -q 'claude' || fail "the cursor is not on the live agent's row"
keys a
sleep 0.5
close_picker
open_from "$P2"
screen | grep -q 'archive)' && fail "plain pick from a live archived agent's pane opened the archive"
close_picker
open_from "$P2" "pick here"
shot "from the live archived pane"
screen | grep -q 'archive)' || fail "pick here did not open into the archive for a live archived agent's pane"
cursor_row | grep -q 'claude.*archived' || fail "the cursor is not on the live archived agent's row"

# `pick id <id>` from a pane that is nobody's: the finished agent's id
# opens the history on its row, the archived one's opens the archive, an
# unknown id says so.
DB="$XDG_DATA_HOME/tmux/plugins/agents/store.db"
FIN=$(sqlite3 "$DB" "select id from agents where ended_ms is not null and life != 'archived' limit 1")
ARC=$(sqlite3 "$DB" "select id from agents where life = 'archived' and ended_ms is null limit 1")
[ -n "$FIN" ] && [ -n "$ARC" ] || fail "no finished / archived ids in the store"
close_picker
open_from "$P0" "pick id $FIN"
shot "pick id finished"
screen | grep -q '+history' || fail "pick id of a finished agent did not open the history"
cursor_row | grep -q 'claude' || fail "the cursor is not on the finished agent's row"
cursor_row | grep -q 'archived' && fail "pick id of the finished agent landed on the archived one"
close_picker
open_from "$P0" "pick id $ARC"
shot "pick id archived"
screen | grep -q 'archive)' || fail "pick id of an archived agent did not open the archive"
cursor_row | grep -q 'claude.*archived' || fail "the cursor is not on the archived agent's row"
# From an agent's own pane, pick id still lands on the id, not on the
# pane's agent.
close_picker
open_from "$P2" "pick id $FIN"
cursor_row | grep -q 'claude' || fail "pick id from an agent pane has no cursor row"
cursor_row | grep -q 'archived' && fail "pick id from an agent pane landed on that pane's agent instead"
close_picker
open_from "$P0" "pick id claude:00000000-0000-4000-8000-000000000000"
screen | grep -q 'no agent claude:0000' || fail "an unknown id did not say so"

cleanup
echo ok
