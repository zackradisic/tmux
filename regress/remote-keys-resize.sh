#!/bin/sh
# Keys typed into a shadow pane reach the remote, and a resize of the shadow
# window (from a client attached on A) resizes the remote window.

. ./remote-common.inc

link_setup

$TMUX send-keys -t fakehost/work:0.0 'echo typed-on-A' Enter
wait_for 6 "$TMUX2 capture-pane -p -t work:0.0 | grep -q '^typed-on-A'" ||
    fail "keys did not arrive on B"
wait_for 6 "$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q '^typed-on-A'" ||
    fail "echo did not come back to A"

# Attach a 100x30 client to the shadow session: the window follows.
$TMUX new-session -d -s outer -x 100 -y 30 \
    "env -u TMUX $TEST_TMUX -LtestA$$ -f/dev/null attach -t fakehost/work" ||
    fail "nested client"
wait_for 6 "[ \"\$($TMUX2 display-message -p -t work:0 \
    '#{window_width}x#{window_height}')\" = 100x29 ]" || fail "B not resized"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work:0 \
    '#{window_width}x#{window_height}')\" = 100x29 ]" || fail "A not resized"
[ "$(geometry "$TMUX" fakehost/work:0)" = "$(geometry "$TMUX2" work:0)" ] ||
    fail "geometry differs after resize"

# Shrink again through the outer window.
$TMUX resize-window -t outer -x 60 -y 20 || fail "resize-window outer"
wait_for 6 "[ \"\$($TMUX2 display-message -p -t work:0 \
    '#{window_width}x#{window_height}')\" = 60x19 ]" || fail "B not shrunk"
exit 0
