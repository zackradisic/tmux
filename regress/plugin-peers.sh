#!/bin/sh
# Peer grants gate bridge callbacks. A links to B; both run mailbox. On the
# initiator (A) the pair (fakehost, mailbox) goes pending; on the inbound
# side (B) the pair for A goes deny. A calling mailbox@B is refused until B
# runs `plugin-peers allow`, and refused again after `revoke`.
#
# Needs the wasm example built:
#   cargo build -p mailbox --target wasm32-unknown-unknown --release

. ./remote-common.inc

MB=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/mailbox.wasm
[ -f "$MB" ] || fail "no mailbox.wasm"

XDG_A="$TMP/a"; XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B"
CAPS="-c db -c service-serve -c service-call -c write-options -c display-message"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "ssh-command"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $CAPS "$MB" || fail "load A"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $CAPS "$MB" || fail "load B"
sleep 1

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"
wait_for 10 "$TMUX2 show-plugins | grep -q 'mailbox: .*running'" || fail "mailbox not on B"
sleep 1.5

APEERS="$TMUX plugin-peers"
BPEERS="XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers"

# The initiator side has a pending row for the peer's mailbox.
wait_for 6 "$TMUX plugin-peers list | grep -q 'fakehost	mailbox	pending'" ||
    fail "no pending row on A: $($TMUX plugin-peers list)"

# The inbound side (B) has a deny row for A, no prompt. A's name there is
# A's hostname; read it from B's list.
wait_for 6 "XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers list | grep -q '	mailbox	deny'" ||
    fail "no deny row on B: $(XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers list)"
ANAME=$(XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers list | awk -F'\t' '$2=="mailbox"&&$3=="deny"{print $1; exit}')
[ -n "$ANAME" ] || fail "no A name on B"

# A -> B is denied: the message never reaches B's store.
$TMUX plugin-command mailbox "send probe@fakehost first try" || fail "send 1"
sleep 1
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox "inbox probe"
[ "$(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe 2>/dev/null)" = '[]' ] ||
    [ -z "$(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe 2>/dev/null)" ] ||
    fail "delivered while denied: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe 2>/dev/null)"

# Allow on B, then A -> B succeeds.
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers allow "$ANAME" mailbox || fail "allow"
$TMUX plugin-command mailbox "send probe@fakehost after allow" || fail "send 2"
wait_for 8 "XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox 'inbox probe -a'; \
    XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe | grep -q 'after allow'" ||
    fail "not delivered after allow: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe 2>/dev/null)"

# Revoke on B, then A -> B is denied again.
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-peers revoke "$ANAME" mailbox || fail "revoke"
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox "inbox probe" >/dev/null 2>&1
$TMUX plugin-command mailbox "send probe@fakehost third try" || fail "send 3"
sleep 1.5
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox "inbox probe" >/dev/null 2>&1
[ "$(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe 2>/dev/null)" = '[]' ] ||
    fail "delivered after revoke: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_probe 2>/dev/null)"
exit 0
