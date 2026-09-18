#!/bin/sh
# "You are here" border. The row for the pane the picker was opened from
# (the pick command's target pane) gets a bright left border. Opened from a
# non-agent pane, no row matches and no border shows.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-here-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d); export XDG_DATA_HOME

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d' >&2; cleanup; exit 1; }
# Open the picker from a specific target pane.
open_from() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command -t $1 agents pick"; sleep 60 ) |
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
border_count() { $TMUX capture-pane -M -p -t "$FORM" | grep -c '▎'; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
# P1, P2 are agents; P3 is a plain shell (no agent).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
$TMUX split-window -t alpha "sh -c 'AI_AGENT=claude exec sleep 600'"
$TMUX split-window -t alpha "sh -c 'unset AI_AGENT OPENCODE; exec sleep 600'"
$TMUX select-layout -t alpha tiled
sleep 0.5
P1=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 1p)
P2=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 2p)
P3=$($TMUX list-panes -t alpha -F '#{pane_id}' | sed -n 3p)

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$WASM" || fail "load-plugin"
sleep 1.5

$TMUX plugin-command -t "$P2" agents "working"; sleep 0.5

# Opened from P1: its row shows the here border.
#
# On the row the cursor is on, the border is drawn as the cursor glyph
# rather than ▎, so ▎ only appears while the cursor is elsewhere - and
# which of the two rows P1 sorted into is not something this test can pin
# down (both are fresh, both are `working`). So look from both ends: gg
# parks the cursor on the first row, G on the last, and P1 is not the
# cursor row in at least one of them.
open_from "$P1"
$TMUX capture-pane -M -p -t "$FORM" | grep -q '2 live' || fail "expected 2 live agents"
$TMUX send-keys -t "$FORM" g; $TMUX send-keys -t "$FORM" g; sleep 0.4
top=$(border_count)
$TMUX send-keys -t "$FORM" G; sleep 0.4
bot=$(border_count)
[ "$top" -ge 1 ] || [ "$bot" -ge 1 ] ||
    fail "no here-border when opened from an agent pane (gg $top, G $bot)"

# Opened from the non-agent pane P3: no row matches, no border.
$TMUX send-keys -t "$FORM" q; sleep 0.4
open_from "$P3"
[ "$(border_count)" -eq 0 ] ||
    fail "here-border shown when opened from a non-agent pane (count $(border_count))"

cleanup
exit 0
