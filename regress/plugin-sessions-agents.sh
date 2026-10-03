#!/bin/sh
# The sessions chooser badges the rows that hold an agent, asking the
# agents plugin: a claude-like pane (detected by environment, with the
# agents plugin's trust_env) badges its pane, its window and its session;
# a session without one has no badge; `!working` narrows to the badged
# rows.
#
# Needs the wasm examples built:
#   cargo build -p sessions -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
# A shell inside an agent carries its marker; the server (and so every
# pane) would inherit it and the plain panes would badge too.
unset AI_AGENT CLAUDECODE CLAUDE_CODE_SESSION_ID CLAUDE_CODE_CHILD_SESSION OPENCODE

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lsessions-agents-test"
[ -z "$TEST_WASM_DIR" ] && TEST_WASM_DIR=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d)
cp "$TEST_WASM_DIR/sessions.wasm" "$TEST_WASM_DIR/sessions.toml" "$DEPLOY/"
cp "$TEST_WASM_DIR/agents.wasm" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","service-serve","service-call"]
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
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }

[ -f "$DEPLOY/sessions.wasm" ] || fail "sessions.wasm not built"
[ -f "$DEPLOY/agents.wasm" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 "sh -c 'exec sleep 600'" || fail "new-session"
$TMUX new-window -t alpha -n agent "sh -c 'AI_AGENT=claude exec sleep 600'"
$TMUX new-session -d -s quiet -x 200 -y 50 "sh -c 'exec sleep 600'"
sleep 0.5

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c service-serve -c service-call "$DEPLOY/agents.wasm" \
    || fail "load-plugin agents"
$TMUX load-plugin -s server -c read-state -c run-command -c mode \
    -c service-serve -c service-call -c send-keys "$DEPLOY/sessions.wasm" \
    || fail "load-plugin sessions"
sleep 2

( sleep 0.3; echo "plugin-command sessions 'pick w'"; sleep 60 ) |
    $TMUX -C attach -t alpha >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

# The agent's window and its session carry a badge; quiet carries none.
i=0
while [ "$i" -lt 20 ]; do
	screen | grep -q '1 agents' && break
	sleep 0.4; i=$((i + 1))
done
screen | grep -q '1 agents' || fail "header does not count the agent"
screen | grep -qE '[●◍◉] .*alpha' || fail "alpha has no badge: $(screen)"
screen | grep -qE '[●◍◉] .*1: agent' || fail "the agent window has no badge: $(screen)"
screen | grep -E 'quiet' | grep -qE '[●◍◉]' && fail "quiet got a badge: $(screen)"

# `!` narrows to rows with that agent status (whatever it is right now).
STATUS=$($TMUX display -p '#{@agent_status}' 2>/dev/null)
keys /
$TMUX send-keys -t "$FORM" '!w'; sleep 0.5
screen | grep -q 'alpha' || fail "!w lost the badged session: $(screen)"
screen | grep -q 'quiet' && fail "!w kept the unbadged session: $(screen)"
keys Escape

cleanup
exit 0
