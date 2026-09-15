#!/bin/sh
# The agents picker addresses an agent by its durable id and leaves a
# message in that agent's mailbox. A local agent's mailbox is here; a
# remote agent's is on its own server, reached over the bridge. The reader
# pulls it with plugin-command mailbox inbox <id>. No keystrokes.
#
# Needs both wasm examples built:
#   cargo build -p agents -p mailbox --target wasm32-unknown-unknown --release

. ./remote-common.inc

AG=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
MB=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/mailbox.wasm
[ -f "$AG" ] || fail "no agents.wasm"
[ -f "$MB" ] || fail "no mailbox.wasm"

XDG_A="$TMP/a"; XDG_B="$TMP/b"; BIN="$TMP/bin"
mkdir -p "$XDG_A" "$XDG_B" "$BIN"
cp /bin/sh "$BIN/codex"

AGCAPS="-c capture-pane -c run-command -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve -c service-call"
MBCAPS="-c db -c service-serve -c service-call -c write-options -c display-message"

# B: a codex agent in session work.
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session on B"
BPANE=$($TMUX2 list-panes -t work -F '#{pane_id}' | head -1 | tr -d '%')
BID="prov-codex-$BPANE"

# A: a codex agent locally, plus agents + mailbox.
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session on A"
APANE=$($TMUX list-panes -t local -F '#{pane_id}' | head -1 | tr -d '%')
AID="prov-codex-$APANE"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "ssh-command"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $AGCAPS "$AG" || fail "load agents A"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $MBCAPS "$MB" || fail "load mailbox A"
# mailbox reaches B by push on link-up (auto-allowed), the real flow.
sleep 1.5

AOPT="$TMUX show-options -s -v"

# Message the LOCAL agent by its id: stored in A's mailbox under that id.
$TMUX plugin-command agents "message $AID hello local agent" || fail "message local"
wait_for 6 "$TMUX plugin-command mailbox \"inbox $AID\"; \
    $TMUX show-options -s -v @mailbox_$AID | grep -q 'hello local agent'" ||
    fail "local message not stored: $($AOPT @mailbox_$AID 2>/dev/null)"

# Link to B and let A's view learn B's roster over the changed topic.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"
wait_for 10 "$TMUX2 show-plugins | grep -q 'mailbox: .*running'" || fail "mailbox not on B"
sleep 2

# Message the REMOTE agent by its id: the view routes it to B's mailbox
# once its roster has learned the agent over the changed topic. Re-send
# each poll until it lands (a send before the roster loads goes local; a
# duplicate on B is harmless), reading with -a so a poll does not clear it.
wait_for 20 "$TMUX plugin-command agents 'message $BID ping remote agent'; \
    sleep 0.4; \
    XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command mailbox 'inbox $BID -a'; \
    XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_$BID | grep -q 'ping remote agent'" ||
    fail "remote message not routed to B: $(XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_$BID 2>/dev/null)"

# The remote store qualifies the sender with the sending server.
XDG_DATA_HOME=$XDG_B $TMUX2 show-options -s -v @mailbox_$BID |
    grep -Eq '"sender":"[^"]+@[^"]+"' ||
    fail "sender not qualified on B"
exit 0
