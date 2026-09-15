#!/bin/sh
# The mailbox plugin: a message left for a box on one server is read back
# there, and a message sent to a box on a linked server crosses the plugin
# bridge and lands in that server's store. No keystrokes, no ssh for the
# message itself.
#
# Needs the wasm example built:
#   cargo build -p mailbox --target wasm32-unknown-unknown --release

. ./remote-common.inc

WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/mailbox.wasm
[ -f "$WASM" ] || fail "no $WASM"

XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B"

XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "set remote-ssh-command"

CAPS="-c db -c service-serve -c service-call -c write-options -c display-message"
$TMUX load-plugin -s server $CAPS "$WASM" || fail "load-plugin on A"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $CAPS "$WASM" || fail "load-plugin on B"

AOPT="$TMUX show-options -s -v"

# Local: leave a message for box alpha on A, read it back.
$TMUX plugin-command mailbox "send alpha hello there" || fail "send local"
wait_for 6 "[ -n \"\$($AOPT @mailbox_alpha 2>/dev/null)\" ] || true; \
    $TMUX plugin-command mailbox 'inbox alpha'; \
    echo \"\$($AOPT @mailbox_alpha)\" | grep -q 'hello there'" ||
    fail "local inbox: $($AOPT @mailbox_alpha 2>/dev/null)"

# Reading marked it read: a second inbox without -a is empty.
$TMUX plugin-command mailbox "inbox alpha" || fail "inbox again"
wait_for 6 "[ \"\$($AOPT @mailbox_alpha)\" = '[]' ]" ||
    fail "not marked read: $($AOPT @mailbox_alpha)"

# Link A to B; then send from A to a box on B over the bridge.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'mailbox: .*running'" || fail "no mailbox on B"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"

$TMUX plugin-command mailbox "send beta@fakehost ping over the bridge" ||
    fail "send remote"
# It landed in B's store: read B's own box beta there.
BOPT="XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v"
wait_for 10 "XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox 'inbox beta'; \
    XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_beta | grep -q 'ping over the bridge'" ||
    fail "remote inbox: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_beta 2>/dev/null)"

# The sender is qualified with the sending server (B's own peer name for
# A, its hostname here), so B sees the message came from another machine.
XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_beta |
    grep -Eq '"sender":"[^"]+@[^"]+"' ||
    fail "sender not qualified: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_beta)"

# A plugin loaded AFTER a link is up still reaches the remote: adding
# mailbox to a live server needs no reconnect. Link C to B with no plugin,
# then load mailbox on C and send across the already-up link.
$TMUX2 new-session -d -s late -x 80 -y 24 2>/dev/null
XDG_C="$TMP/c"
mkdir -p "$XDG_C"
TMUXC="$TEST_TMUX -Ltestc$$ -f/dev/null"
XDG_DATA_HOME="$XDG_C" $TMUXC new-session -d -s c -x 80 -y 24 || fail "new-session on C"
XDG_DATA_HOME="$XDG_C" $TMUXC set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "C ssh-command"
XDG_DATA_HOME="$XDG_C" $TMUXC remote-attach -t late fakelate || fail "C link"
wait_for 10 "[ \"\$(XDG_DATA_HOME=$XDG_C $TMUXC display-message -p -t fakelate/late '#{remote_connected}')\" = 1 ]" ||
    fail "C link not up"
XDG_DATA_HOME="$XDG_C" $TMUXC load-plugin -s server $CAPS "$WASM" ||
    fail "load mailbox on C after link"
wait_for 10 "XDG_DATA_HOME=$XDG_B $TMUX2 show-plugins | grep -q 'mailbox: .*running'" ||
    fail "mailbox not pushed to B after a live link"
XDG_DATA_HOME="$XDG_C" $TMUXC plugin-command mailbox "send gamma@fakelate after the link" ||
    fail "C send"
wait_for 10 "XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox 'inbox gamma'; XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_gamma | grep -q 'after the link'" ||
    fail "late-load delivery"
XDG_DATA_HOME="$XDG_C" $TMUXC kill-server 2>/dev/null
exit 0
