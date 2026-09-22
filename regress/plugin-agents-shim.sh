#!/bin/sh
# The Claude shim's `pretool` verb. A PreToolUse hook runs the shim with
# the tool's payload on stdin; the shim decides the status itself:
#
#   an ordinary tool (Bash) reports `working`;
#   AskUserQuestion reports `needs_input` with the question as the note,
#     and the row lands in the "needs input" band showing it;
#   ExitPlanMode reports `needs_input` with "plan ready for review";
#   a bare `needs_input` with a Notification payload still takes its
#     `message`, as before.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX_L="$TEST_TMUX -Lagents-shim-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
SHIM=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/shims/agent-status.sh

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX_L kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX_L kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { [ -n "$FORM" ] && $TMUX_L capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX_L -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX_L list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX_L capture-pane -M -p -t "$FORM" | grep -q '▸' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render a cursor"
}
# Run the shim the way a hook would: TMUX and TMUX_PANE in the
# environment, the payload on stdin, the test binary as TMUXBIN.
shim() {
	verb=$1; payload=$2
	printf '%s\n' "$payload" |
	    TMUX="$SOCK,0,0" TMUX_PANE="$P1" TMUXBIN="$TEST_TMUX" sh "$SHIM" "$verb"
	sleep 0.6
}
status_of() { $TMUX_L plugin-command agents nothing 2>/dev/null; sqlite3 "$XDG_DATA_HOME/tmux/plugins/agents/store.db" "SELECT status || '|' || COALESCE(note,'') FROM agents WHERE pane = ${P1#%}" 2>/dev/null; }

[ -f "$WASM" ] || fail "agents.wasm not built"
[ -f "$SHIM" ] || fail "shim not found"

$TMUX_L kill-server 2>/dev/null
sleep 0.5
$TMUX_L -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5
P1=$($TMUX_L list-panes -t alpha -F '#{pane_id}' | head -1)
SOCK=$($TMUX_L display-message -p '#{socket_path}')

$TMUX_L load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.5

# An ordinary tool: working.
shim pretool '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}'
open_picker
screen | grep -q 'working' || fail "pretool Bash did not report working"

# A question dialog: needs input, with the question on the row.
shim pretool '{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Ship it to prod or staging first?","header":"Target","options":[{"label":"prod"},{"label":"staging"}]}]}}'
sleep 0.8
screen | grep -q 'needs input' || fail "AskUserQuestion did not land in needs input"
screen | grep -q 'Ship it to prod or staging first?' || fail "the question is not on the row"

# The user answered: the next tool is working again.
shim pretool '{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{}}'
sleep 0.8
screen | grep -q 'needs input' && fail "the next tool did not clear needs input"

# A plan waiting for approval.
shim pretool '{"hook_event_name":"PreToolUse","tool_name":"ExitPlanMode","tool_input":{"plan":"1. do x"}}'
sleep 0.8
screen | grep -q 'plan ready for review' || fail "ExitPlanMode did not report the plan"

# A Notification payload on a bare needs_input still yields its message.
shim needs_input '{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}'
sleep 0.8
screen | grep -q 'needs your permission' || fail "a Notification message was not taken as the note"

cleanup
exit 0
