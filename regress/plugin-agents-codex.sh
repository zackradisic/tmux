#!/bin/sh
# Detecting an interpreter-wrapped agent. Codex ships as `node /usr/bin/
# codex`, so the pane's foreground command is `node`, not `codex`, and it
# sets no AI_AGENT. The roster detects it from the `_` env var: when the
# foreground command is an interpreter, the basename of the launched script
# names the agent. Here `env _=.../codex <node> -c 'sleep 600'` reproduces
# a node pane whose `_` points at a "codex" script.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-codex-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
XDG_DATA_HOME=$(mktemp -d); export XDG_DATA_HOME
FAKE=$(mktemp -d); cp /bin/sh "$FAKE/node"   # a binary whose comm is "node"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE","_"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME" "$FAKE" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME" "$FAKE" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
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
# A node pane whose `_` names a "codex" script (the npm launcher shape).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "env _=/opt/agents/codex $FAKE/node -c 'sleep 600'" || fail "new-session"
sleep 0.5
[ "$($TMUX display-message -p -t alpha '#{pane_current_command}')" = "node" ] ||
    fail "test setup: foreground command is not node"

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.5

open_picker
screen | grep -q '1 live' || fail "the node-wrapped codex agent is not on the roster"
screen | grep -q 'codex' || fail "the agent was not classified as codex"

cleanup
exit 0
