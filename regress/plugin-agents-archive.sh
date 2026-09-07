#!/bin/sh
# The archive lifecycle of the agents roster. Archiving hides a row, and it
# must STAY hidden across a plugin reload (the same reconcile/activate path a
# `restart-server` runs) - a plain re-sighting of the same session must not
# resurrect it. A `working` report, though, is a new turn (the user messaged
# the agent), so it brings the archived row back. Check:
#
#   archiving a live agent drops it from the live roster;
#   a reload (init reconcile re-sights the pane) keeps it archived;
#   a `working` report un-archives it - the row returns.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-archive-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	cleanup
	exit 1
}
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }

# Open the picker on a fresh control client and set FORM to its mode pane.
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
}

load() {
	$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
	    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
	    || fail "load-plugin"
	sleep 1.0
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# One Claude pane, detected by its env marker.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
[ -n "$PANE" ] || fail "no pane id"

load
open_picker
screen | grep -q '1 live' || fail "the agent is not on the roster"

# Archive it: the live roster empties.
$TMUX send-keys -t "$FORM" a; sleep 0.6
screen | grep -q '0 live' || fail "archiving did not drop the agent"

# Reload the plugin: init reconcile re-sights the same live pane. This is
# the exact activate path a restart-server runs. The archive must survive.
$TMUX unload-plugin agents 2>/dev/null; sleep 0.3
load
open_picker
screen | grep -q '0 live' ||
    fail "a reload resurrected the archived agent (restart bug)"

# A `working` report is a new turn - the user messaged the agent. It brings
# the archived row back into the live roster.
$TMUX plugin-command -t "$PANE" agents "working editing the file"
sleep 0.6
screen | grep -q '1 live' || fail "a working report did not un-archive"
screen | grep -q 'claude' || fail "the un-archived agent is missing"

cleanup
exit 0
