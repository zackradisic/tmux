#!/bin/sh
# "You are here" across a remote link. Sitting in a shadow pane and opening
# the picker must put the border on the remote agent's row: the pane we sit
# in is local (the shadow), while the row carries the pane id from the
# remote server, so the two only meet through the mirror map.
#
# Regression: the marker was gated on the row being local, so it never
# showed for a remote agent no matter which pane the picker was opened from.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

. ./remote-common.inc
. ./fake-bin.inc

BUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
[ -f "$BUILT" ] || fail "agents.wasm not built"
SIDECAR=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml
[ -f "$SIDECAR" ] || fail "no agents.toml"

XDG_A="$TMP/a"
XDG_B="$TMP/b"
BIN="$TMP/bin"
mkdir -p "$XDG_A" "$XDG_B" "$BIN"
fake_bin "$BIN/codex"

mkdir -p "$TMP/deploy"
cp "$BUILT" "$TMP/deploy/agents.wasm"
cp "$SIDECAR" "$TMP/deploy/agents.toml"
WASM="$TMP/deploy/agents.wasm"

screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
border_count() { $TMUX capture-pane -M -p -t "$FORM" | grep -c '▎'; }

# Open the picker from a given pane on A, and wait for it to render.
open_from() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command -t $1 agents pick"; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 2.0
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q '2 servers'" ||
	    fail "picker did not render both servers: $(screen)"
}

# B: a codex agent. A: a claude agent, so both groups have a row and the
# border cannot be attributed to there being only one row on screen.
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude CLAUDE_CODE_SESSION_ID=sess-abc exec sleep 600'" \
    || fail "new-session on A"
APANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode -c db \
    -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve \
    -c service-call "$WASM" || fail "load-plugin"
sleep 1.0

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'agents: .*role provider'" ||
    fail "not pushed: $($TMUX2 show-plugins)"
wait_for 10 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
sleep 1.5

# The shadow pane on A that mirrors B's agent pane.
BPANE=$($TMUX2 list-panes -t work -F '#{pane_id}' | head -1)
SHADOW=$($TMUX list-panes -t fakehost/work \
    -F '#{pane_id} #{pane_remote_id}' | awk -v r="$BPANE" '$2 == r { print $1 }')
[ -n "$SHADOW" ] || fail "no shadow pane mirroring $BPANE"
[ "$SHADOW" != "$BPANE" ] ||
    fail "shadow and remote pane ids coincide ($SHADOW); the test cannot tell them apart"

# Opened from the shadow pane: the border shows, and on the codex row.
open_from "$SHADOW"
screen | grep -q 'codex' || fail "codex missing from the picker: $(screen)"
[ "$(border_count)" -ge 1 ] ||
    fail "no here-border when opened from a shadow pane: $(screen)"
screen | grep '▎' | grep -q 'codex' ||
    fail "here-border is not on the remote agent's row: $(screen)"

# Opened from A's own agent pane: the border moves to the local row. This is
# what catches a fix that marks every remote row unconditionally.
$TMUX send-keys -t "$FORM" q; sleep 0.5
open_from "$APANE"
# The local row is also the cursor row, where the here-border is drawn as the
# cursor glyph; step the cursor off it so the border shows as ▎ and can be
# told apart from a plain selection.
$TMUX send-keys -t "$FORM" Down; sleep 0.5
[ "$(border_count)" -ge 1 ] ||
    fail "no here-border when opened from the local agent pane: $(screen)"
screen | grep '▎' | grep -q 'codex' &&
    fail "here-border stayed on the remote row: $(screen)"

kill $CTL 2>/dev/null
exit 0
