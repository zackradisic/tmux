#!/bin/sh
# A link that cannot come up says why. The reason shows in the placeholder
# window's name, in remote_error and in show-messages; once the cause is
# gone the link comes up and the error clears. Two causes: the remote has
# no such session, and the ssh command itself fails on stderr.

. ./remote-common.inc

$TMUX2 new-session -d -s work -x 80 -y 24 || exit 1
$TMUX new-session -d -s local -x 80 -y 24 || exit 1
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" || exit 1

# No such session on B.
$TMUX remote-attach -t nosuch fakehost || fail "remote-attach"
wait_for 6 "$TMUX display-message -p -t fakehost/nosuch '#{remote_error}' | \
    grep -q \"can't find session: nosuch\"" ||
    fail "remote_error: $($TMUX display-message -p -t fakehost/nosuch '#{remote_error}')"
[ "$($TMUX display-message -p -t fakehost/nosuch '#{remote_connected}')" = 0 ] ||
    fail "connected"
$TMUX list-windows -t fakehost/nosuch -F '#{window_name}' |
    grep -q "^connecting to fakehost: can't find session: nosuch" ||
    fail "window name: $($TMUX list-windows -t fakehost/nosuch -F '#{window_name}')"
$TMUX show-messages | grep -q "remote fakehost: can't find session: nosuch" ||
    fail "no message"
$TMUX capture-pane -p -t fakehost/nosuch | grep -q "remote: fakehost: can't find session" ||
    fail "placeholder pane text"

# The cause goes away: the link comes up on a retry and the error clears.
$TMUX2 new-session -d -s nosuch -x 80 -y 24 || fail "new-session nosuch"
wait_for 12 "[ \"\$($TMUX display-message -p -t fakehost/nosuch \
    '#{remote_connected}')\" = 1 ]" || fail "did not connect"
[ -z "$($TMUX display-message -p -t fakehost/nosuch '#{remote_error}')" ] ||
    fail "error not cleared"
[ "$($TMUX display-message -p -t fakehost/nosuch '#{remote_state}')" = connected ] ||
    fail "remote_state while up"

# The remote dies: one disconnected line per pane, not one per retry, and
# the status line format from example_tmux.conf shows the state.
STATUS="#{?session_remote_host,#{?remote_connected,[#{session_remote_host}],#{session_remote_host} #{remote_state}#{?remote_error,: #{=/48/...:remote_error},}},local}"
[ "$($TMUX display-message -p -t fakehost/nosuch "$STATUS")" = '[fakehost]' ] ||
    fail "status while up: $($TMUX display-message -p -t fakehost/nosuch "$STATUS")"
[ "$($TMUX display-message -p -t local "$STATUS")" = local ] ||
    fail "status on a local session"
$TMUX2 kill-server || fail "kill-server B"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/nosuch \
    '#{remote_state}')\" = disconnected ]" || fail "remote_state while down"
sleep 4
n=$($TMUX capture-pane -p -t fakehost/nosuch:0.0 | grep -c 'fakehost disconnected')
[ "$n" -eq 1 ] || fail "disconnected lines after retries: $n"
$TMUX display-message -p -t fakehost/nosuch "$STATUS" | grep -q '^fakehost disconnected: ' ||
    fail "status while down: $($TMUX display-message -p -t fakehost/nosuch "$STATUS")"
$TMUX kill-session -t fakehost/nosuch || fail "kill-session"

# The ssh command fails with text on stderr.
$TMUX set -s remote-ssh-command \
    "sh -c 'echo Host key verification failed. >&2; exit 255'" || exit 1
$TMUX remote-attach -t work otherhost || fail "remote-attach otherhost"
wait_for 6 "[ \"\$($TMUX display-message -p -t otherhost/work '#{remote_error}')\" = \
    'Host key verification failed.' ]" ||
    fail "stderr error: $($TMUX display-message -p -t otherhost/work '#{remote_error}')"

# remote_host reaches the command (a table format; the tree alone would
# not do), so does remote_session.
$TMUX set -s remote-ssh-command \
    "sh -c 'echo host=#{q:remote_host} session=#{q:remote_session} >&2; exit 1'" || exit 1
$TMUX remote-attach -t work user@10.0.0.5 || fail "remote-attach dotted"
wait_for 6 "[ \"\$($TMUX display-message -p -t user@10_0_0_5/work '#{remote_error}')\" = \
    'host=user@10.0.0.5 session=work' ]" ||
    fail "expansion: $($TMUX display-message -p -t user@10_0_0_5/work '#{remote_error}')"

# A command that exits without a word reports its status.
$TMUX set -s remote-ssh-command "sh -c 'exit 3'" || exit 1
$TMUX remote-attach -t work quiethost || fail "remote-attach quiethost"
wait_for 6 "[ \"\$($TMUX display-message -p -t quiethost/work '#{remote_error}')\" = \
    'command exited with status 3' ]" ||
    fail "status error: $($TMUX display-message -p -t quiethost/work '#{remote_error}')"
exit 0
