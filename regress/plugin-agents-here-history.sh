#!/bin/sh
# Opened from a pane whose agent has finished, the picker shows the
# history and puts the cursor on that agent's row; opened from one whose
# agent was archived, it shows the archive and lands there too. Rows
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
open_from() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command -t $1 agents pick"; sleep 60 ) |
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
sleep 0.5
P=$($TMUX list-panes -t alpha:1 -F '#{pane_id}')
P0=$($TMUX list-panes -t alpha:0 -F '#{pane_id}')

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$WASM" || fail "load-plugin"
sleep 1
$TMUX plugin-command -t "$P" agents "working"
sleep 3.5

# The agent is gone from the live list...
open_from "$P"
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
shot "from the archived pane"
screen | grep -q 'archive)' || fail "the picker did not open into the archive for an archived agent's pane"
cursor_row | grep -q 'claude.*archived' || fail "the cursor is not on the archived agent's row"

# From a pane that never had an agent: the plain live list, no history.
close_picker
open_from "$P0"
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
shot "from the live archived pane"
screen | grep -q 'archive)' || fail "the picker did not open into the archive for a live archived agent's pane"
cursor_row | grep -q 'claude.*archived' || fail "the cursor is not on the live archived agent's row"

cleanup
echo ok
