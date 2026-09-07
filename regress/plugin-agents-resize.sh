#!/bin/sh
# The picker opens at a fraction of the window (bigger than the old fixed
# 110x22), `+`/`-` resize it, and the size is remembered across opens.
# Check:
#
#   the default popup is sized to the window (wider than the old default);
#   pressing `+` grows it;
#   closing and reopening keeps the resized width (persisted in the db).
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-resize-test"
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
	cleanup
	exit 1
}
width() { $TMUX display-message -p -t "$FORM" '#{pane_width}'; }

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

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# A roomy window so the default has headroom to grow.
$TMUX -f/dev/null new-session -d -s alpha -x 150 -y 46 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
    || fail "load-plugin"
sleep 1.0

open_picker
DEF=$(width)
[ -n "$DEF" ] || fail "no default width"
[ "$DEF" -gt 120 ] || fail "default popup not window-sized (width $DEF, want >120)"

# Grow it with `+`.
$TMUX send-keys -t "$FORM" +; sleep 0.5
BIG=$(width)
[ "$BIG" -gt "$DEF" ] || fail "+ did not grow the popup ($DEF -> $BIG)"

# Close (Esc) and reopen: the resized width is remembered.
$TMUX send-keys -t "$FORM" Escape; sleep 0.5
open_picker
KEPT=$(width)
[ "$KEPT" = "$BIG" ] ||
    fail "resized width not remembered (grew to $BIG, reopened at $KEPT)"

cleanup
exit 0
