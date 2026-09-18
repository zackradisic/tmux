#!/bin/sh
# The agents roster across a remote link. A (local) runs one claude-like
# agent; B (the "remote") runs one codex-like agent. A loads the plugin in
# role both; the link pushes it to B as a provider. The picker on A shows
# both agents grouped by server, a status report on B reaches A's picker
# through the changed topic, Enter on the remote row switches to the
# mirrored shadow pane, and a dropped link marks the server disconnected
# until it returns.
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

# Deploy the wasm with its sidecar, as a release install does: the host
# then runs it in restrictive mode, where the sidecar's requests mask the
# grants. The sidecar must ask for the service caps or the roster stays
# local. The pushed copy carries the sidecar to B too.
mkdir -p "$TMP/deploy"
cp "$BUILT" "$TMP/deploy/agents.wasm"
cp "$SIDECAR" "$TMP/deploy/agents.toml"
WASM="$TMP/deploy/agents.wasm"

screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }

# B: a codex agent (detected by command name) in session "work".
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session on B"
# A: a claude agent (detected by environment) in session "alpha".
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude CLAUDE_CODE_SESSION_ID=sess-abc exec sleep 600'" \
    || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode -c db \
    -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve \
    -c service-call "$WASM" || fail "load-plugin"
sleep 1.0

# Link: the plugin lands on B as a provider and scans B's panes.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'agents: .*role provider'" ||
    fail "not pushed: $($TMUX2 show-plugins)"
wait_for 10 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
sleep 1.5

# A control client gives the picker a current window to open on.
( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
    $TMUX -C attach -t alpha >/dev/null 2>&1 &
CTL=$!
sleep 2.0
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"

# Two servers, one agent each, grouped by server.
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q '2 servers'" ||
    fail "picker does not show 2 servers: $(screen)"
screen | grep -q '▪ local' || fail "no local server header"
screen | grep -q '▪ fakehost' || fail "no fakehost server header"
screen | grep -q 'claude' || fail "claude agent missing"
screen | grep -q 'codex' || fail "codex agent missing from the remote group"
# The remote row sits under the fakehost header, the local one above it.
screen | awk '/▪ fakehost/ { seen = 1 } seen && /codex/ { found = 1 } END { exit !found }' ||
    fail "codex is not under the fakehost header: $(screen)"
screen | awk '/▪ fakehost/ { exit found ? 0 : 1 } /claude/ { found = 1 }' ||
    fail "claude is not under the local header: $(screen)"

# A status report on B travels to A's picker through the changed topic.
BPANE=$($TMUX2 list-panes -t work -F '#{pane_id}' | head -1)
$TMUX2 plugin-command -t "$BPANE" agents "needs_input fix the build"
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q 'fix the build'" ||
    fail "the remote task did not reach the picker: $(screen)"

# Enter on the remote row jumps to its shadow pane: the pressing client
# switches to the mirrored session and lands on the shadow pane.
keys Down; keys Down; keys Down
screen | grep -q '▸.*codex' || keys Down
screen | grep -q '▸.*codex' || fail "could not put the cursor on codex: $(screen)"
keys Enter
sleep 1.0
CSESS=$($TMUX list-clients -F '#{client_session}' | head -1)
[ "$CSESS" = "fakehost/work" ] || fail "client is on $CSESS, not fakehost/work"
[ "$($TMUX display-message -p -t fakehost/work '#{pane_remote_id}')" = "$BPANE" ] ||
    fail "active shadow pane does not mirror $BPANE"
$TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode && fail "picker did not close on jump"

# Link down: the remote group shows disconnected; up again clears it.
# The link reconnects within a second on its own, so park the ssh command
# on one that fails while the down state is checked.
$TMUX set -s remote-ssh-command false || fail "set remote-ssh-command false"
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 0 ]" ||
    fail "link still up"
$TMUX plugin-command agents pick
sleep 1.5
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not reopen"
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q 'fakehost.*disconnected'" ||
    fail "no disconnected marker: $(screen)"
screen | grep -q 'codex' || fail "the remote rows vanished while down"

# Let the link come back: the marker clears and the rows stay.
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "restore remote-ssh-command"
wait_for 30 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link did not return"
wait_for 10 "! $TMUX capture-pane -M -p -t $FORM | grep -q 'disconnected'" ||
    fail "disconnected marker stayed after reconnect: $(screen)"
screen | grep -q 'codex' || fail "codex missing after reconnect"

kill $CTL 2>/dev/null
exit 0
