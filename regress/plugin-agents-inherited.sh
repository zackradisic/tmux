#!/bin/sh
# AI_AGENT alone is not proof of a Claude. A tmux server started from
# inside Claude Code hands AI_AGENT to every pane it spawns, so an idle
# shell or a sleep carries it too. Only a pane that a Claude session file
# names (~/.claude/sessions/<pid>.json, `tmux` field) is a claude row.
# The server here carries the variable itself, as such a server does.
# A stale file from an earlier server names a live pane id with another
# window id; that pane is not a row either.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
unset TMUX

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Ltest$$ -f/dev/null"
$TMUX kill-server 2>/dev/null

BUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
[ -f "$BUILT" ] || { echo "SKIP: agents.wasm not built" >&2; exit 0; }
SIDECAR=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml

TMP=$(mktemp -d)
HOME_DIR="$TMP/home"
mkdir -p "$HOME_DIR/.claude/sessions"

# The sidecar next to the wasm gives the scoped fs-read prefix
# ~/.claude/sessions (under the server's HOME), which the session file
# check reads through.
mkdir -p "$TMP/deploy"
cp "$BUILT" "$TMP/deploy/agents.wasm"
cp "$SIDECAR" "$TMP/deploy/agents.toml"
WASM="$TMP/deploy/agents.wasm"

cleanup()
{
	[ -n "$CTL" ] && kill "$CTL" 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$TMP"
}
trap cleanup EXIT

fail()
{
	echo "FAIL: $*" >&2
	exit 1
}

screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }

# The server inherits AI_AGENT and a HOME of its own; both go to every
# pane. Pane %0: a sleep that only inherited the variable.
AI_AGENT=claude-code_2-1-270_agent HOME="$HOME_DIR" XDG_DATA_HOME="$TMP/data" \
    $TMUX new-session -d -s alpha -x 200 -y 50 "sh -c 'exec sleep 600'" ||
    fail "new-session alpha"
[ "$($TMUX display-message -p -t alpha '#{AI_AGENT}')" = claude-code_2-1-270_agent ] ||
    fail "the server does not carry AI_AGENT"

# Pane %1: a real Claude. Its session file names the pane it runs in.
cat >"$HOME_DIR/.claude/sessions/4242.json" <<JSON
{"tmux":"beta:@1.%1","sessionId":"sess-real","name":"Real one","status":"busy"}
JSON
$TMUX new-session -d -s beta -x 200 -y 50 "sh -c 'exec sleep 600'" ||
    fail "new-session beta"
[ "$($TMUX list-panes -t beta -F '#{pane_id}')" = '%1' ] || fail "beta is not %1"

# Pane %2: a stale file from an earlier server names pane %2 in window
# @9, which this server never made.
cat >"$HOME_DIR/.claude/sessions/66622.json" <<JSON
{"tmux":"zackoverflow:@9.%2","sessionId":"sess-stale","name":"Stale one","status":"idle"}
JSON
$TMUX new-session -d -s gamma -x 200 -y 50 "sh -c 'exec sleep 600'" ||
    fail "new-session gamma"
[ "$($TMUX list-panes -t gamma -F '#{pane_id}')" = '%2' ] || fail "gamma is not %2"

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    -c service-serve -c service-call "$WASM" || fail "load-plugin"
sleep 1.5

( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
    $TMUX -C attach -t alpha >/dev/null 2>&1 &
CTL=$!
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

# One claude row, the one with the session file; the inherited pane is
# not an agent.
screen | grep -q '(1 live' || fail "expected one live row: $(screen | head -3)"
screen | grep -q 'Real one' || fail "the real Claude is missing: $(screen)"
screen | grep -q 'alpha' && fail "the inherited pane became a row: $(screen)"
screen | grep -q 'Stale one' && fail "a stale session file claimed a pane: $(screen)"
exit 0
