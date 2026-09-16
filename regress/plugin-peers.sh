#!/bin/sh
# Peer grants, corrected direction. A links to B (A is the initiator, B the
# remote). A -> B calls are always allowed (A ssh'd in). B -> A calls are
# gated: only a method the callee declares `serve_remote` (mailbox: deliver)
# can even reach `pending`, and A must allow the (server, plugin) pair. A
# plugin that declares nothing (agents) never gets a row.
#
# Needs the wasm examples built:
#   cargo build -p mailbox -p agents --target wasm32-unknown-unknown --release

. ./remote-common.inc

MBBUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/mailbox.wasm
AGBUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
MBSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/mailbox/mailbox.toml
AGSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml
[ -f "$MBBUILT" ] || fail "no mailbox.wasm"
[ -f "$AGBUILT" ] || fail "no agents.wasm"

XDG_A="$TMP/a"; XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B" "$TMP/dep"
# Deploy the wasm with its sidecar so serve_remote is read (restrictive).
cp "$MBBUILT" "$TMP/dep/mailbox.wasm"; cp "$MBSIDE" "$TMP/dep/mailbox.toml"
cp "$AGBUILT" "$TMP/dep/agents.wasm"; cp "$AGSIDE" "$TMP/dep/agents.toml"
MB="$TMP/dep/mailbox.wasm"; AG="$TMP/dep/agents.wasm"
HOST=$(hostname)

MBCAPS="-c db -c service-serve -c service-call -c write-options -c display-message"
AGCAPS="-c capture-pane -c run-command -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve -c service-call"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "ssh-command"
# Both run mailbox and agents.
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $MBCAPS "$MB" || fail "mb A"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $AGCAPS "$AG" || fail "ag A"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $MBCAPS "$MB" || fail "mb B"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $AGCAPS "$AG" || fail "ag B"
sleep 1

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"
sleep 1.5

# The initiator has a pending row for the remote's mailbox (declared), and
# NO row for agents (declares nothing): the menu lists only declared plugins.
wait_for 8 "$TMUX plugin-peers list | grep -q 'fakehost	mailbox	pending'" ||
    fail "no mailbox pending on A: $($TMUX plugin-peers list)"
$TMUX plugin-peers list | grep -q 'agents' && fail "agents got a grant row: $($TMUX plugin-peers list)"

# The inbound side (B) has NO rows: an inbound peer needs no grant.
[ -z "$(XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers list | grep -v '^no peer grants')" ] ||
    fail "B has grant rows: $(XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers list)"

# A -> B (initiator -> remote) is always allowed, no grant needed.
$TMUX plugin-command mailbox "send fromA@fakehost hello B" || fail "A send"
wait_for 8 "XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox 'inbox fromA -a'; \
    XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_fromA | grep -q 'hello B'" ||
    fail "A->B not delivered: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_fromA 2>/dev/null)"

# B -> A (remote -> initiator) is denied until A allows.
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox "send toA@$HOST first from B" || fail "B send 1"
sleep 1
$TMUX plugin-command mailbox "inbox toA" >/dev/null 2>&1
V=$($TMUX show-options -s -v @mailbox_toA 2>/dev/null)
[ "$V" = '[]' ] || [ -z "$V" ] || fail "B->A delivered while denied: $V"

$TMUX plugin-peers allow fakehost mailbox || fail "allow"
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox "send toA@$HOST second from B" || fail "B send 2"
wait_for 8 "$TMUX plugin-command mailbox 'inbox toA -a'; \
    $TMUX show-options -s -v @mailbox_toA | grep -q 'second from B'" ||
    fail "B->A not delivered after allow: $($TMUX show-options -s -v @mailbox_toA 2>/dev/null)"

# Revoke: B -> A denied again.
$TMUX plugin-peers revoke fakehost mailbox || fail "revoke"
$TMUX plugin-command mailbox "inbox toA" >/dev/null 2>&1
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox "send toA@$HOST third from B" || fail "B send 3"
sleep 1.5
$TMUX plugin-command mailbox "inbox toA" >/dev/null 2>&1
V=$($TMUX show-options -s -v @mailbox_toA 2>/dev/null)
[ "$V" = '[]' ] || [ -z "$V" ] || fail "B->A delivered after revoke: $V"
exit 0
