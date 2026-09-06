#!/bin/sh
# The agents roster, resolved from a harness session file. This exercises
# the headline path: a Claude pane is enriched from
# ~/.claude/sessions/<pid>.json, read through the SCOPED fs-read grant
# (a `[caps.fs-read] paths` prefix, not the blanket fs-read-any). Check:
#
#   the row takes the session file's name, not the bare kind;
#   the file's `status` (idle) puts the row in the "waiting" band;
#   the scoped fs-read cap reaches ~/.claude/sessions and nowhere else.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-resolve-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

# A private HOME: the session file and the scoped prefix both live here.
HOME=$(mktemp -d)
export HOME
XDG_DATA_HOME="$HOME/.local/share"
export XDG_DATA_HOME
mkdir -p "$HOME/.claude/sessions"

# A deployment dir so the sidecar toml sits next to the wasm (restrictive
# mode): effective caps = the sidecar requests, intersected with -c.
DEPLOY=$(mktemp -d)
cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane", "run-command", "mode", "db", "env-read",
            "pane-fds", "fs-read", "fs-list"]
[caps.env-read]
names = ["AI_AGENT", "OPENCODE"]
[caps.fs-read]
paths = ["~/.claude/sessions"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	cleanup
	exit 1
}
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# A Claude pane, detected by env; identity + name come from the file.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
sleep 0.5

WIN=$($TMUX list-panes -t alpha -F '#{window_id}' | head -1)
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
NUM=${PANE#%}
[ -n "$NUM" ] || fail "no pane id"
# Name the file by the pane's foreground pid, so the plugin's direct
# `<pid>.json` read (via pane_pid) hits without scanning the directory.
PID=$($TMUX list-panes -t alpha -F '#{pane_pid}' | head -1)
[ -n "$PID" ] || fail "no pane pid"

# The session file Claude would write, matched to this pane by its tmux
# field. Times are epoch ms; idle => the "waiting" band.
now=$(date +%s)000
started=$((now - 3600000))
cat >"$HOME/.claude/sessions/$PID.json" <<EOF
{"pid":424242,"sessionId":"25a936a2-cf9b-407d-9e4e-89e04a9636e7",
 "cwd":"$HOME","startedAt":$started,"tmux":"alpha:$WIN.%$NUM",
 "name":"hyperyaml-91","status":"idle","updatedAt":$now}
EOF

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0

# Open the picker; enrich-at-render reads the session file.
( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
    $TMUX -C attach >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

screen | grep -q 'hyperyaml-91' || fail "resolved name missing (name from file)"
screen | grep -q 'waiting' || fail "idle status did not land in waiting band"
screen | grep -q 'claude' || fail "kind tag missing"

cleanup
exit 0
