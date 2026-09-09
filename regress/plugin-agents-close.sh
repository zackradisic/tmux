#!/bin/sh
# Closing the picker. `q` closes it from the list. Esc closes it from the
# list too, and from an EMPTY search box (so a stray move up into the box
# never swallows a close); Esc in a NON-empty box only unfocuses and keeps
# the query (a second Esc then closes).
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-close-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d); export XDG_DATA_HOME

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() { echo "FAIL: $*" >&2; cleanup; exit 1; }
mode_of() { $TMUX display-message -p -t "$FORM" '#{pane_mode}'; }
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
sleep 0.5
$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$WASM" || fail "load-plugin"
sleep 1.0

# q closes from the list.
open_picker
$TMUX send-keys -t "$FORM" q; sleep 0.5
[ -z "$(mode_of)" ] || fail "q did not close the picker (mode '$(mode_of)')"

# Esc closes from the list.
open_picker
$TMUX send-keys -t "$FORM" Escape; sleep 0.5
[ -z "$(mode_of)" ] || fail "Esc did not close the picker (mode '$(mode_of)')"

# Esc closes from an EMPTY search box (navigate up into it, then Esc).
open_picker
$TMUX send-keys -t "$FORM" Up; sleep 0.4
$TMUX capture-pane -M -p -t "$FORM" | grep -q 'Esc unfocus' ||
    fail "up did not focus the search box"
$TMUX send-keys -t "$FORM" Escape; sleep 0.5
[ -z "$(mode_of)" ] || fail "Esc on an empty box did not close (mode '$(mode_of)')"

# Esc in a NON-empty box only unfocuses; a second Esc then closes.
open_picker
$TMUX send-keys -t "$FORM" /; sleep 0.3
$TMUX send-keys -t "$FORM" x; sleep 0.3
$TMUX send-keys -t "$FORM" Escape; sleep 0.5
[ "$(mode_of)" = "plugin-mode" ] ||
    fail "Esc in a non-empty box should unfocus, not close (mode '$(mode_of)')"
$TMUX send-keys -t "$FORM" Escape; sleep 0.5
[ -z "$(mode_of)" ] || fail "second Esc did not close (mode '$(mode_of)')"

cleanup
exit 0
