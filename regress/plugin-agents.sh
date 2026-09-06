#!/bin/sh
# The agents roster plugin. Create two agent panes - one detected by its
# command name (a `codex` symlink, the Codex case: no env marker) and one
# detected by its environment (AI_AGENT=claude with a session id) - load
# the server-scoped plugin, and check that:
#
#   init reconcile discovers both agents and reads Claude's session id;
#   a status report on the wire updates the row and its task;
#   the picker (needs -c mode) lists both agents with their kinds;
#   killing an agent pane retires it (it leaves the live list).
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
BIN=$(mktemp -d)
ln -s "$(command -v sleep)" "$BIN/codex"

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME" "$BIN"' EXIT

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME" "$BIN"
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

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Pane one: the Codex case - detected by command name, no env marker.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'exec $BIN/codex 600'" || fail "new-session alpha"
# Pane two: the Claude case - detected by env, with a session id.
$TMUX new-session -d -s beta -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude CLAUDE_CODE_SESSION_ID=sess-abc exec sleep 600'" \
    || fail "new-session beta"
sleep 0.5

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list "$WASM" \
    || fail "load-plugin"
# init reconcile scans the panes; give it a moment.
sleep 1.0

# The Claude pane's id (%N) to address the status wire.
BETA=$($TMUX list-panes -t beta -F '#{pane_id}' | head -1)
[ -n "$BETA" ] || fail "no beta pane"

# A status report over the wire: pane travels in the command scope.
$TMUX plugin-command -t "$BETA" agents "needs_input review the diff"
sleep 0.5

# A control client gives the picker a current window to open on.
( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

screen | grep -q 'codex' || fail "codex agent missing from the roster"
screen | grep -q 'claude' || fail "claude agent missing from the roster"
screen | grep -q 'review the diff' || fail "the reported task is missing"
screen | grep -q 'jump' || fail "footer is missing the key hints"

# Archive the LAST row: this shrinks the roster, which once indexed the
# new (shorter) rows with a stale index and trapped the guest, tearing
# down the mode. The picker must survive and stay open.
keys Down; keys Down; keys Down
$TMUX send-keys -t "$FORM" a; sleep 0.6
$TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode ||
    fail "picker closed after archiving the last row"

# Retire: kill the codex pane; it should leave the live list.
$TMUX kill-session -t alpha
sleep 0.8
screen | grep -q 'codex' && fail "codex still listed after its pane closed"

cleanup
exit 0
