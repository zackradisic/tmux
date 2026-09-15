#!/bin/sh
# remote-attach attaches a session or creates it. B has no session "fresh":
# the link makes it (new-session -A on the remote) with the given working
# directory and mirrors it. After the first sync a reconnect only
# attaches: a session killed on B stays dead. -L lists B's sessions. The
# template uses #{remote_command}, the form the default ssh command uses.

. ./remote-common.inc

$TMUX2 new-session -d -s work -x 80 -y 24 || exit 1
$TMUX new-session -d -s local -x 80 -y 24 || exit 1
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || exit 1

# Create: the session appears on B, in the given directory.
$TMUX remote-attach -t fresh -c /tmp fakehost || fail "remote-attach fresh"
wait_for 6 "$TMUX2 has-session -t fresh" || fail "session not created on B"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/fresh \
    '#{remote_connected}')\" = 1 ]" || fail "not connected"
[ "$($TMUX2 display-message -p -t fresh '#{pane_current_path}')" = /tmp ] ||
    fail "working directory: $($TMUX2 display-message -p -t fresh '#{pane_current_path}')"
[ "$($TMUX2 list-sessions | wc -l)" -eq 2 ] || fail "B session count"

# A name with a space survives the quoting.
$TMUX remote-attach -t 'my sess' fakehost || fail "remote-attach my sess"
wait_for 6 "$TMUX2 has-session -t 'my sess'" || fail "spaced session not created"
wait_for 6 "[ \"\$($TMUX display-message -p -t 'fakehost/my sess' \
    '#{remote_connected}')\" = 1 ]" || fail "spaced session not connected"

# Attach: an existing session is mirrored as it is, not made again.
$TMUX2 send-keys -t work:0.0 'echo already-here' Enter
$TMUX remote-attach -t work fakehost || fail "remote-attach work"
wait_for 6 "$TMUX capture-pane -p -t fakehost/work:0.0 | grep -q '^already-here'" ||
    fail "existing session not mirrored"
[ "$($TMUX2 list-sessions | wc -l)" -eq 3 ] || fail "attach made a session"

# -L lists B's sessions and makes no link.
$TMUX remote-attach -L fakehost >"$TMP/list" || fail "remote-attach -L"
grep -qx fresh "$TMP/list" || fail "list lacks fresh: $(cat "$TMP/list")"
grep -qx work "$TMP/list" || fail "list lacks work"
grep -qx 'my sess' "$TMP/list" || fail "list lacks my sess"
[ "$($TMUX list-sessions | wc -l)" -eq 4 ] || fail "-L made a session"

# The default remote-ssh-command, quoting and all, through a stand-in for
# ssh: it drops the options and the host, then runs the rest the way a
# remote login shell would, joined with spaces and parsed again, with a
# HOME whose ~/.local/bin holds a tmux2 that is B.
FAKEHOME="$TMP/remotehome"
mkdir -p "$FAKEHOME/.local/bin" "$TMP/bin"
cat >"$FAKEHOME/.local/bin/tmux2" <<EOF
#!/bin/sh
exec $TEST_TMUX -LtestB$$ -f/dev/null "\$@"
EOF
cat >"$TMP/bin/ssh" <<EOF
#!/bin/sh
while [ \$# -gt 0 ]; do
	case "\$1" in
	-o) shift 2 ;;
	-T) shift ;;
	*) break ;;
	esac
done
shift
HOME="$FAKEHOME"; export HOME
exec sh -c "\$*"
EOF
chmod +x "$FAKEHOME/.local/bin/tmux2" "$TMP/bin/ssh"
$TMUX set -su remote-ssh-command || fail "unset option"
default=$($TMUX show-options -s -v remote-ssh-command)
case "$default" in ssh\ *) ;; *) fail "default does not start with ssh: $default" ;; esac
$TMUX set -s remote-ssh-command "$TMP/bin/ssh ${default#ssh }" || fail "set default"
$TMUX remote-attach -t 'via default' -c /tmp otherhost || fail "remote-attach via default"
wait_for 6 "$TMUX2 has-session -t 'via default'" || fail "default command did not create"
wait_for 6 "[ \"\$($TMUX display-message -p -t 'otherhost/via default' \
    '#{remote_connected}')\" = 1 ]" ||
    fail "default command not connected: $($TMUX display-message -p -t 'otherhost/via default' '#{remote_error}')"
$TMUX remote-attach -L otherhost >"$TMP/list2" || fail "remote-attach -L via default"
grep -qx 'via default' "$TMP/list2" || fail "default list: $(cat "$TMP/list2")"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || exit 1

# Killed on B after the link was up: the reconnect attaches only, so the
# session stays dead and the link says why.
$TMUX2 kill-session -t fresh || fail "kill-session on B"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/fresh \
    '#{remote_connected}')\" = 0 ]" || fail "link did not drop"
sleep 4
$TMUX2 has-session -t fresh 2>/dev/null && fail "killed session came back"
$TMUX display-message -p -t fakehost/fresh '#{remote_error}' |
    grep -q "can't find session" || fail "no reason: $($TMUX display-message -p -t fakehost/fresh '#{remote_error}')"
exit 0
