#!/bin/sh
# Copy mode over the agents picker. The picker is a plugin mode with its
# own screen. Entering copy mode used to clone the pane's empty real grid
# and show nothing; now copy mode backs onto the plugin mode's screen (it
# sits directly below on the mode stack), so it shows what the picker drew.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-copymode-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; $TMUX capture-pane -M -p -t "$FORM" | head -8 >&2; cleanup; exit 1; }
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
		$TMUX capture-pane -M -p -t "$FORM" | grep -q 'agents' && return 0
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
    -c db -c env-read -c pane-fds "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.0

open_picker
[ "$(mode_of)" = "plugin-mode" ] || fail "picker pane not in plugin-mode"

# Enter copy mode over the picker.
$TMUX copy-mode -t "$FORM"; sleep 0.5
[ "$(mode_of)" = "copy-mode" ] || fail "copy-mode did not activate (got '$(mode_of)')"

# Copy mode must show what the picker drew, not a blank pane.
$TMUX capture-pane -M -p -t "$FORM" | grep -q 'agents' ||
    fail "copy mode did not show the picker header"
$TMUX capture-pane -M -p -t "$FORM" | grep -q 'claude' ||
    fail "copy mode did not show the agent row"

# `q` exits copy mode and returns to the picker (plugin mode below).
$TMUX send-keys -t "$FORM" q; sleep 0.5
[ "$(mode_of)" = "plugin-mode" ] || fail "did not return to the picker (got '$(mode_of)')"
$TMUX capture-pane -M -p -t "$FORM" | grep -q 'agents' ||
    fail "picker not shown after leaving copy mode"

cleanup
exit 0
