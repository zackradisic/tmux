#!/bin/sh
# remote-attach mirrors a remote session: windows, panes and their ids, text
# output, and the format cache for pane_current_command.

. ./remote-common.inc

$TMUX2 new-session -d -s work -x 80 -y 24 || exit 1
$TMUX2 split-window -t work:0 || exit 1
$TMUX2 new-window -t work -n two || exit 1

$TMUX new-session -d -s local -x 80 -y 24 || exit 1
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" || exit 1
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 6 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_connected}')\" = 1 ]" || fail "not connected"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{session_windows}')\" = 2 ]" || fail "window count"

# Window names and indexes match.
$TMUX list-windows -t fakehost/work -F '#{window_index} #{window_name}' >"$TMP/a"
$TMUX2 list-windows -t work -F '#{window_index} #{window_name}' >"$TMP/b"
cmp -s "$TMP/a" "$TMP/b" || fail "windows differ: $(cat "$TMP/a" | tr '\n' ' ') vs $(cat "$TMP/b" | tr '\n' ' ')"

# The shadow's remote ids are B's pane ids, in the same order and geometry.
$TMUX list-panes -s -t fakehost/work -F \
    '#{window_remote_id} #{pane_remote_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}' >"$TMP/a"
$TMUX2 list-panes -s -t work -F \
    '#{window_id} #{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}' >"$TMP/b"
cmp -s "$TMP/a" "$TMP/b" || fail "panes differ"

# Text printed on B shows up in the shadow pane on A.
$TMUX2 send-keys -t work:0.0 'echo marker-from-B' Enter
wait_for 6 "$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q '^marker-from-B'" ||
    fail "output did not arrive"

# The format cache: pane_current_command comes from B's subscription.
cmd=$($TMUX2 display-message -p -t work:0.0 '#{pane_current_command}')
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work:0.0 \
    '#{pane_current_command}')\" = '$cmd' ]" || fail "pane_current_command"

# Format variables for local objects stay empty.
[ -z "$($TMUX display-message -p -t local '#{remote_host}#{pane_remote_id}')" ] ||
    fail "local pane has remote formats"
[ "$($TMUX display-message -p -t fakehost/work '#{session_remote_host}')" = fakehost ] ||
    fail "session_remote_host"

# A second link to the same session is refused.
$TMUX remote-attach -t work fakehost 2>/dev/null && fail "duplicate link allowed"

# kill-session on the shadow session drops the link and B's control client.
$TMUX kill-session -t fakehost/work || fail "kill-session"
wait_for 6 "! $TMUX has-session -t fakehost/work" || fail "shadow session survived"
wait_for 6 "[ \"\$($TMUX2 list-clients | wc -l)\" -eq 0 ]" || fail "B client survived"
exit 0
