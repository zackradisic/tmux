#!/bin/sh
# The selection must not jump when agents update. The picker fixes each
# agent's intra-band order at open, so a later activity bump (a status
# report, or the refresh timer) changes badges in place without
# reshuffling rows under the cursor. Here: select a (renamed) middle
# agent, bump every agent's status, and assert the cursor stays on that
# agent AND on the same screen line.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-stable-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d); export XDG_DATA_HOME

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d' >&2; cleanup; exit 1; }
curline() { $TMUX capture-pane -M -p -t "$FORM" | grep -n '▸' | head -1 | cut -d: -f1; }
selname() { $TMUX capture-pane -M -p -t "$FORM" | grep '▸' | head -1; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render"
}

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
$TMUX split-window -t alpha "sh -c 'AI_AGENT=claude exec sleep 600'"
$TMUX split-window -t alpha "sh -c 'AI_AGENT=claude exec sleep 600'"
$TMUX select-layout -t alpha tiled
sleep 0.5

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$WASM" || fail "load-plugin"
sleep 1.5

open_picker
$TMUX capture-pane -M -p -t "$FORM" | grep -q '3 live' || fail "expected 3 live"

# Select the middle row and pin it with a distinctive name.
$TMUX send-keys -t "$FORM" j; sleep 0.4
$TMUX send-keys -t "$FORM" r; sleep 0.3
$TMUX send-keys -t "$FORM" PINME; sleep 0.3
$TMUX send-keys -t "$FORM" Enter; sleep 0.6
selname | grep -q PINME || fail "cursor not on PINME after rename"
L=$(curline); [ -n "$L" ] || fail "no cursor line"

# Bump every agent's activity. Under the old sort this reshuffles the band
# by activity time and moves PINME; with the stable order it must not.
for P in $($TMUX list-panes -t alpha -F '#{pane_id}'); do
	$TMUX plugin-command -t "$P" agents "working"
done
sleep 1.5

# The cursor stayed on PINME, and on the same screen line.
selname | grep -q PINME || fail "cursor left PINME after activity bumps (jumped): $(selname)"
L2=$(curline)
[ "$L2" = "$L" ] || fail "cursor line jumped ($L -> $L2) on an activity bump"

cleanup
exit 0
