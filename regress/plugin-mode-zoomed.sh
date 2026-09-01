#!/bin/sh
# A plugin mode opened while the window is zoomed (prefix+w is choose-tree
# -Z, and the session_creator form binds S there). The float used to be
# built into the throwaway zoom layout; window_set_active_pane then
# unzoomed, freed that layout with the float's cell in it, and restored the
# dangling saved_layout_cell, so layout_fix_panes resized the float to
# garbage and the server spun for good in grid_reflow. mode_open must
# unzoom first, like new-pane does, and the form must open at a sane size.

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lmode-zoomed-test"
[ -z "$TEST_WASM" ] && TEST_WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/session_creator.wasm
WASM=$TEST_WASM
TMP=$(mktemp -d)

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	# A hung server never answers kill-server; do not wait on it.
	( $TMUX kill-server 2>/dev/null & sleep 2; kill $! 2>/dev/null ) \
	    >/dev/null 2>&1
	pkill -f -- "-Lmode-zoomed-test" 2>/dev/null
	rm -rf "$TMP"
}
fail() {
	echo "FAIL: $*" >&2
	cleanup
	exit 1
}
# Run a tmux command with a watchdog: a hung server blocks the client
# forever, which is exactly the bug, so give up after 5 seconds.
ask() {
	$TMUX "$@" >"$TMP/out" 2>&1 &
	pid=$!
	n=0
	while kill -0 $pid 2>/dev/null; do
		[ $n -ge 50 ] && { kill $pid 2>/dev/null; return 1; }
		sleep 0.1
		n=$((n + 1))
	done
	wait $pid
}

[ -f "$WASM" ] || fail "session_creator.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f /dev/null new-session -d -x 100 -y 30 -c "$TMP" || fail "new-session"
$TMUX split-window -d -c "$TMP" || fail "split-window"
$TMUX choose-tree -Zw || fail "choose-tree -Zw"
[ "$($TMUX display -p '#{window_zoomed_flag}')" = 1 ] || fail "not zoomed"
$TMUX load-plugin -c mode -c run-process -c run-command -c fs-list \
    -c fs-read-any "$WASM" || fail "load-plugin"

( sleep 0.3; echo 'plugin-command session_creator new'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
sleep 1.5

ask list-panes -a -F '#{pane_id} #{pane_mode} #{pane_width} #{pane_height}' ||
    fail "server hung after opening the form in a zoomed window"
FORM=$(awk '/plugin-mode/ { print $1 }' "$TMP/out")
[ -n "$FORM" ] || fail "form did not open: $(cat "$TMP/out")"
set -- $(awk '/plugin-mode/ { print $3, $4 }' "$TMP/out")
[ "$1" -gt 0 ] && [ "$1" -lt 100 ] || fail "form width $1"
[ "$2" -gt 0 ] && [ "$2" -lt 30 ] || fail "form height $2"

ask display -p '#{window_zoomed_flag} #{window_panes}' || fail "server hung"
[ "$(cat "$TMP/out")" = "0 3" ] ||
    fail "expected unzoomed, 3 panes: $(cat "$TMP/out")"

# The form still works and the window survives closing it.
$TMUX send-keys -t "$FORM" Escape
sleep 0.4
# A second Esc closes the form if the first only hid the list.
$TMUX send-keys -t "$FORM" Escape 2>/dev/null
sleep 0.6
ask display -p '#{window_panes}' || fail "server hung closing the form"
[ "$(cat "$TMP/out")" = 2 ] || fail "form pane did not close: $(cat "$TMP/out")"

cleanup
exit 0
