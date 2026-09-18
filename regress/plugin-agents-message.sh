#!/bin/sh
# Messaging across the gate. A (initiator) links to B (remote). A messages
# its own local agent freely. B messages an agent on A with an explicit
# <id>@<server>; that is a remote -> initiator call, denied until A allows
# the pair. A bare unknown id is refused, never resolved by fetching a
# roster from an inbound peer.
#
# Needs the wasm examples built:
#   cargo build -p agents -p mailbox --target wasm32-unknown-unknown --release

. ./remote-common.inc
. ./fake-bin.inc

AGBUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
MBBUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/mailbox.wasm
AGSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml
MBSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/mailbox/mailbox.toml
[ -f "$AGBUILT" ] || fail "no agents.wasm"
[ -f "$MBBUILT" ] || fail "no mailbox.wasm"

XDG_A="$TMP/a"; XDG_B="$TMP/b"; BIN="$TMP/bin"
mkdir -p "$XDG_A" "$XDG_B" "$BIN" "$TMP/dep"
fake_bin "$BIN/codex"
cp "$AGBUILT" "$TMP/dep/agents.wasm"; cp "$AGSIDE" "$TMP/dep/agents.toml"
cp "$MBBUILT" "$TMP/dep/mailbox.wasm"; cp "$MBSIDE" "$TMP/dep/mailbox.toml"
AG="$TMP/dep/agents.wasm"; MB="$TMP/dep/mailbox.wasm"
HOST=$(hostname)

AGCAPS="-c capture-pane -c run-command -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve -c service-call"
MBCAPS="-c db -c service-serve -c service-call -c write-options -c display-message"

# B: a codex agent too (so the roster is non-trivial), agents + mailbox.
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session B"
# A: a local codex agent (the message target), agents + mailbox.
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session A"
APANE=$($TMUX list-panes -t local -F '#{pane_id}' | head -1 | tr -d '%')
AID="prov-codex-$APANE"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "ssh-command"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $AGCAPS "$AG" || fail "ag A"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $MBCAPS "$MB" || fail "mb A"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $AGCAPS "$AG" || fail "ag B"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $MBCAPS "$MB" || fail "mb B"
sleep 1.5

AOPT="$TMUX show-options -s -v"

# A messages its own local agent by bare id: stored in A's mailbox.
$TMUX plugin-command agents "message $AID hello local" || fail "local message"
wait_for 6 "$TMUX plugin-command mailbox \"inbox $AID -a\"; \
    $TMUX show-options -s -v @mailbox_$AID | grep -q 'hello local'" ||
    fail "local not stored: $($AOPT @mailbox_$AID 2>/dev/null)"

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"
sleep 2

# B messages A's agent with an explicit <id>@<server>: remote -> initiator,
# denied until A allows.
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command agents "message $AID@$HOST from B denied" ||
    fail "B message 1"
sleep 1
$TMUX plugin-command mailbox "inbox $AID" >/dev/null 2>&1
$TMUX show-options -s -v @mailbox_$AID 2>/dev/null | grep -q 'from B denied' &&
    fail "B->A delivered while denied"

$TMUX plugin-peers allow fakehost mailbox || fail "allow"
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command agents "message $AID@$HOST from B allowed" ||
    fail "B message 2"
wait_for 10 "$TMUX plugin-command mailbox \"inbox $AID -a\"; \
    $TMUX show-options -s -v @mailbox_$AID | grep -q 'from B allowed'" ||
    fail "B->A not delivered after allow: $($AOPT @mailbox_$AID 2>/dev/null)"

# A bare unknown id on B is refused, with no inbound roster fetch.
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command agents "message no-such-agent hi" || fail "B message 3"
sleep 0.5
XDG_DATA_HOME=$XDG_B $TMUX2 show-messages | grep -q 'unknown agent no-such-agent' ||
    fail "bare unknown id not refused: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-messages | tail -2)"
exit 0
