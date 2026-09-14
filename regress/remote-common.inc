# Shared prelude for the remote link tests. Two servers on one machine: A is
# the local server, B plays the remote. A's remote-ssh-command runs B's own
# control mode client instead of ssh.

PATH=/bin:/usr/bin
TERM=screen
# The tests attach nested clients; a TMUX variable from the shell that runs
# them would make those clients refuse.
unset TMUX

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -LtestA$$ -f/dev/null"
TMUX2="$TEST_TMUX -LtestB$$ -f/dev/null"
$TMUX kill-server 2>/dev/null
$TMUX2 kill-server 2>/dev/null

TMP=$(mktemp -d)

cleanup()
{
	$TMUX kill-server 2>/dev/null
	$TMUX2 kill-server 2>/dev/null
	rm -rf "$TMP"
}
trap cleanup EXIT

fail()
{
	echo "FAIL: $*" >&2
	exit 1
}

# wait_for <timeout-seconds> <shell command>: poll until the command exits 0.
wait_for()
{
	timeout=$1
	shift
	i=0
	while [ "$i" -lt "$((timeout * 5))" ]; do
		if sh -c "$*" >/dev/null 2>&1; then
			return 0
		fi
		sleep 0.2
		i=$((i + 1))
	done
	return 1
}

# Start B with one session "work" and A with a local session, then link.
link_setup()
{
	$TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
	$TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
	$TMUX set -s remote-ssh-command \
	    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
	    fail "set remote-ssh-command"
	$TMUX remote-attach -t work fakehost || fail "remote-attach"
	wait_for 6 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
	wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work \
	    '#{remote_connected}')\" = 1 ]" || fail "link not connected"
}

# Pane geometry of a window as one string, for comparisons across servers.
geometry()
{
	$1 list-panes -t "$2" -F '#{pane_left},#{pane_top} #{pane_width}x#{pane_height}'
}
