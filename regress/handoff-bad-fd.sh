#!/bin/sh
# A pane record in the handoff state file names a descriptor that is not
# open (or not the saved tty). The server must not put it into the event
# loop, where select() would fail on it for good and the server would hang
# at full CPU: the pane comes back dead, with a line in show-messages, and
# the server answers as before.

PATH=/bin:/usr/bin
TERM=screen
unset TMUX

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Ltest$$ -f/dev/null"
$TMUX kill-server 2>/dev/null

TMP=$(mktemp -d)
cleanup()
{
	$TMUX kill-server 2>/dev/null
	[ -n "$SERVER" ] && kill "$SERVER" 2>/dev/null
	rm -rf "$TMP"
}
trap cleanup EXIT

fail()
{
	echo "FAIL: $*" >&2
	exit 1
}

# A layout string with the right checksum for one 80x24 pane %0, from a
# throwaway server.
$TMUX new-session -d -s tmp -x 80 -y 24 || fail "throwaway server"
layout=$($TMUX display-message -p -t tmp '#{window_layout}')
$TMUX kill-server
sleep 0.3

# Descriptor 200 is not open in the new server; the tty does not exist.
STATE="$TMP/state"
printf 'version\t1\n' >"$STATE"
printf 'session\t0\talpha\t/tmp\t1787870086\n' >>"$STATE"
printf 'window\t0\t0\tsleep\t80\t24\t0\t0\t0\n' >>"$STATE"
printf 'layout\t%s\n' "$layout" >>"$STATE"
printf 'wactive\t0\n' >>"$STATE"
printf 'pane\t0\t200\t12345\t/dev/pts/999\t/tmp\t/bin/sh\t0\t0\t0\t0\t0\t80\t24\t2\tsleep\t600\n' >>"$STATE"
printf 'tiledend\n' >>"$STATE"
printf 'windowend\n' >>"$STATE"
printf 'sessionend\n' >>"$STATE"

# -Z is the exec'd server image itself: it runs in the foreground and
# restores from the file, so start it in the background and talk to it as
# a client.
$TMUX -Z "$STATE" >"$TMP/out" 2>&1 &
SERVER=$!
i=0
while [ "$i" -lt 30 ]; do
	$TMUX list-sessions >"$TMP/ls" 2>/dev/null && break
	kill -0 "$SERVER" 2>/dev/null || fail "server exited: $(cat "$TMP/out")"
	sleep 0.2
	i=$((i + 1))
done
grep -q '^alpha:' "$TMP/ls" || fail "session missing: $(cat "$TMP/ls") $(cat "$TMP/out")"

# The pane is there, dead, and the server still answers after a while.
[ "$($TMUX display-message -p -t alpha:0.0 '#{pane_dead}')" = 1 ] ||
    fail "pane not dead: $($TMUX list-panes -a -F '#{pane_id} #{pane_dead}')"
$TMUX show-messages | grep -q 'restore: pane %0: descriptor 200' ||
    fail "no message: $($TMUX show-messages)"
sleep 2
[ "$($TMUX display-message -p 'ok')" = ok ] || fail "server stopped answering"
exit 0
