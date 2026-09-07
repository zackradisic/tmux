#!/bin/sh
# Un-archive an agent from the picker. Archiving hides a row from the live
# roster; the history view (h) shows it again, tagged "archived", and the
# archive key (a) on an archived row un-archives it - the row returns to
# the live roster. Check:
#
#   archiving a live agent drops it from the live roster;
#   the history view shows the archived agent, tagged "archived";
#   pressing `a` on it un-archives it - the live roster fills again.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-unarchive-test"
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

$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5

load
open_picker
screen | grep -q '1 live' || fail "the agent is not on the roster"

# Archive it: the live roster empties.
$TMUX send-keys -t "$FORM" a; sleep 0.6
screen | grep -q '0 live' || fail "archiving did not drop the agent"

# Show history: the archived agent reappears, tagged "archived".
$TMUX send-keys -t "$FORM" h; sleep 0.6
screen | grep -q 'archived' || fail "history view did not show the archived tag"
screen | grep -q 'claude' || fail "the archived agent is missing from history"

# Press `a` on the archived row: it un-archives and rejoins the roster.
$TMUX send-keys -t "$FORM" a; sleep 0.6
screen | grep -q '1 live' || fail "pressing a did not un-archive the agent"

cleanup
exit 0
