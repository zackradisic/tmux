#!/bin/sh
# A shim's report is proof an agent runs in the pane. Without `trust_env`
# a pane whose command is a shell or a `sleep` is no agent by itself:
#
#   a pane with AI_AGENT=claude in its environment gets no row on its
#     own, and a row of kind claude once its shim reports `working`;
#   a pane with nothing in its environment but a Claude session file
#     naming it (the `tmux` field, pane and window) gets a row on its
#     report, and the picker's render moves it to the durable id - the
#     case of a Claude started by `zsh -c 'claude ...; exec zsh'`, whose
#     pane command reads as the shell (same process group);
#   a pane with neither stays off the roster even when reported.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX_L="$TEST_TMUX -Lagents-reported-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
SHIM=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/shims/agent-status.sh

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
mkdir -p "$HOME/.claude/sessions"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read",
            "pane-fds","fs-read","fs-list"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
[caps.fs-read]
paths = ["~/.claude/sessions"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX_L kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX_L kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { [ -n "$FORM" ] && $TMUX_L capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
q() { db=$(find "$XDG_DATA_HOME" -name store.db | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
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
# Run the shim the way a hook would: $1 = pane, $2 = verb.
shim() {
	printf '%s\n' '{}' |
	    TMUX="$SOCK,0,0" TMUX_PANE="$1" TMUXBIN="$TEST_TMUX" sh "$SHIM" "$2"
	sleep 0.8
	$TMUX_L plugin-command agents nothing 2>/dev/null
}
row_of() { q "SELECT id || '|' || kind || '|' || status FROM agents WHERE pane = ${1#%} AND ended_ms IS NULL"; }

[ -f "$WASM" ] || fail "agents.wasm not built"
[ -f "$SHIM" ] || fail "shim not found"

$TMUX_L kill-server 2>/dev/null
sleep 0.5
$TMUX_L -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
$TMUX_L split-window -t alpha "sleep 600"
$TMUX_L split-window -t alpha "sleep 600"
sleep 0.5
P1=$($TMUX_L list-panes -t alpha -F '#{pane_id}' | sed -n 1p)
P2=$($TMUX_L list-panes -t alpha -F '#{pane_id}' | sed -n 2p)
P3=$($TMUX_L list-panes -t alpha -F '#{pane_id}' | sed -n 3p)
WIN=$($TMUX_L display-message -p -t alpha '#{window_id}')
SOCK=$($TMUX_L display-message -p '#{socket_path}')
SID=11111111-2222-3333-4444-555555555555
NOW=$(date +%s)000
cat >"$HOME/.claude/sessions/424242.json" <<EOF
{"pid":424242,"sessionId":"$SID",
 "cwd":"$HOME","startedAt":$NOW,"tmux":"alpha:$WIN.$P2",
 "name":"vllm512","status":"busy","updatedAt":$NOW}
EOF

# No trust_env: the environment is a hint, not proof.
$TMUX_L load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.5
$TMUX_L plugin-command agents nothing 2>/dev/null

n=$(q "SELECT count(*) FROM agents WHERE ended_ms IS NULL" | tr -d ' ')
[ "$n" = 0 ] || fail "rows before any report: $n (a sleep is not an agent)"

# 1. AI_AGENT in the environment + a report: a claude.
shim "$P1" working
r=$(row_of "$P1")
case $r in
prov-claude-*'|claude|working') ;;
*) fail "AI_AGENT pane after its report: '$r'" ;;
esac

# 2. Nothing in the environment, a session file naming the pane: a
#    claude, and the render moves it to the durable id.
shim "$P2" working
r=$(row_of "$P2")
case $r in
prov-claude-*'|claude|working') ;;
*) fail "session-file pane after its report: '$r'" ;;
esac
open_picker
sleep 1
r=$(row_of "$P2")
[ "$r" = "claude:$SID|claude|working" ] || fail "session-file pane after a render: '$r'"
screen | grep -q 'vllm512' || fail "the picker does not show vllm512"

# 3. Neither: a report alone does not make an agent.
shim "$P3" working
r=$(row_of "$P3")
[ -z "$r" ] || fail "bare sleep pane got a row from a report: '$r'"

echo PASS
cleanup
exit 0
