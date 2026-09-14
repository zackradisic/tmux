#!/bin/sh
# When the control mode connection drops, the shadow objects stay with their
# ids, show that they are disconnected, and refill when the link returns.
# A server restart on A keeps the shadow session id and the link comes back.

. ./remote-common.inc

link_setup

pane=$($TMUX display-message -p -t fakehost/work:0 '#{pane_id}')
sid=$($TMUX display-message -p -t fakehost/work '#{session_id}')

# Drop B's control client.
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
[ -n "$client" ] || fail "no control client on B"
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_connected}')\" = 0 ]" || fail "still connected"
[ "$($TMUX display-message -p -t fakehost/work:0 '#{pane_id}')" = "$pane" ] ||
    fail "pane id changed"
wait_for 6 "$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q 'fakehost disconnected'" ||
    fail "no disconnected message"

# Typed on B while the link is down; visible on A after the refill.
$TMUX2 send-keys -t work:0.0 'echo while-down' Enter
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_connected}')\" = 1 ]" || fail "did not reconnect"
wait_for 6 "$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q '^while-down'" ||
    fail "text typed while down missing"
$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q 'disconnected' &&
    fail "disconnected message survived the refill"
[ "$($TMUX display-message -p -t fakehost/work:0 '#{pane_id}')" = "$pane" ] ||
    fail "pane id changed after reconnect"

# restart-server on A: the session id stays and the link returns.
$TMUX restart-server || fail "restart-server"
wait_for 10 "$TMUX has-session -t fakehost/work" || fail "shadow session lost"
[ "$($TMUX display-message -p -t fakehost/work '#{session_id}')" = "$sid" ] ||
    fail "session id changed"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_connected}')\" = 1 ]" || fail "link did not return after restart"
$TMUX2 send-keys -t work:0.0 'echo after-restart' Enter
wait_for 6 "$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q '^after-restart'" ||
    fail "output after restart missing"
exit 0
