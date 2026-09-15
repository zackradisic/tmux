#!/bin/sh
# What runs in the pane decides whether it is a Claude. A tmux server
# started from inside Claude Code hands AI_AGENT to every pane it spawns,
# so an idle shell or a sleep carries it too, and a Claude session file
# (~/.claude/sessions/<pid>.json, `tmux` field) outlives its Claude and
# can name a pane that now holds a shell. So: a pane whose command is a
# harness (by name, or a version string as the macOS launcher execs) and
# whose pane and window a session file names is a claude row. A sleep
# with the variable is not, even with a file that names it; a harness
# whose file names another window is not either. The server here
# carries the variable itself, as such a server does.
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
BIN="$TMP/bin"
mkdir -p "$HOME_DIR/.claude/sessions" "$BIN"
# The macOS launcher execs a version-named binary: the pane's command is
# "2.1.271", not "claude".
# A copy of the shell under the agent's name: a symlink to sleep would
# not do, a multi-call coreutils dispatches on argv[0] and refuses it.
cp /bin/sh "$BIN/2.1.271"

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

# Pane %1: a real Claude, a version-named command whose session file
# names the pane it runs in.
cat >"$HOME_DIR/.claude/sessions/4242.json" <<JSON
{"tmux":"beta:@1.%1","sessionId":"sess-real","name":"Real one","status":"busy"}
JSON
$TMUX new-session -d -s beta -x 200 -y 50 "sh -c 'exec $BIN/2.1.271 -c \"read -r _\"'" ||
    fail "new-session beta"
[ "$($TMUX list-panes -t beta -F '#{pane_id}')" = '%1' ] || fail "beta is not %1"

# Pane %2: a version-named command, but the only file that names %2 is a
# stale one from an earlier server, in window @9, which this server never
# made.
cat >"$HOME_DIR/.claude/sessions/66622.json" <<JSON
{"tmux":"zackoverflow:@9.%2","sessionId":"sess-stale","name":"Stale one","status":"idle"}
JSON
$TMUX new-session -d -s gamma -x 200 -y 50 "sh -c 'exec $BIN/2.1.271 -c \"read -r _\"'" ||
    fail "new-session gamma"
[ "$($TMUX list-panes -t gamma -F '#{pane_id}')" = '%2' ] || fail "gamma is not %2"

# Pane %3: a sleep with the inherited variable AND a file that names it
# exactly. An ordinary command is not an agent, however loudly a file
# claims the pane.
cat >"$HOME_DIR/.claude/sessions/7777.json" <<JSON
{"tmux":"delta:@3.%3","sessionId":"sess-shell","name":"Shell one","status":"idle"}
JSON
$TMUX new-session -d -s delta -x 200 -y 50 "sh -c 'exec sleep 600'" ||
    fail "new-session delta"
[ "$($TMUX list-panes -t delta -F '#{pane_id}')" = '%3' ] || fail "delta is not %3"

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
screen | grep -q 'Shell one' && fail "a file made a shell into an agent: $(screen)"
exit 0
