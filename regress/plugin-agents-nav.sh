#!/bin/sh
# Picker navigation: vim `gg`/`G` jump to the ends of the list, and the
# search box is focused by navigating the cursor up past the top row (Esc
# unfocuses it and keeps the query). No `/` hotkey anymore.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-nav-test"
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
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
# The screen line the cursor marker sits on.
curline() { $TMUX capture-pane -M -p -t "$FORM" | grep -n '▸' | head -1 | cut -d: -f1; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	# Wait for the first render (enrich is slower with several agents).
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render a cursor"
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
    -c db -c env-read -c pane-fds "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

open_picker
screen | grep -q '3 live' || fail "expected 3 live agents"
TOP=$(curline)
[ -n "$TOP" ] || fail "no cursor marker at open"

# G goes to the bottom: the marker moves to a later screen line.
keys G
BOT=$(curline)
[ -n "$BOT" ] || fail "no cursor after G"
[ "$BOT" -gt "$TOP" ] || fail "G did not move the cursor down ($TOP -> $BOT)"

# gg goes back to the top.
keys g; keys g
BACK=$(curline)
[ "$BACK" = "$TOP" ] || fail "gg did not return to the top ($BOT -> $BACK, want $TOP)"

# Navigate up from the top row: the search box takes focus. Its footer is
# distinct ("Esc unfocus"), and typed text lands in the query.
keys Up
screen | grep -q 'Esc unfocus' || fail "up at top did not focus the search box"
$TMUX send-keys -t "$FORM" zzqz; sleep 0.4
screen | grep -q 'zzqz' || fail "typing did not reach the search box"

# Esc unfocuses but KEEPS the query.
keys Escape
screen | grep -q 'Esc unfocus' && fail "Esc did not unfocus the search box"
screen | grep -q 'zzqz' || fail "Esc cleared the query (it should keep it)"

cleanup
exit 0
