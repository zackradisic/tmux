#!/bin/sh
# restart-server keeps exactly the pty masters it writes into the state file,
# and nothing else. It used to keep whatever a walk of the window id tree
# found, which is a different list from the sessions the state file is
# written from: on Nov 10 2026 a destroyed duplicate window had emptied the
# tree, the masters of 163 of 175 panes got close-on-exec, and the exec hung
# up every process in them while the new image restored them all as dead.
# The same walk also carried the shadow panes of remote links across the
# exec, where nothing ever used or closed them again.
#
# The scene: local panes of every kind a user has (a plain program, an
# interactive shell, a full-screen program on the alternate screen, a script
# that logs SIGHUP, a pane made by respawn-pane), plus a live link to a
# second server. After two restarts every local process must be the same
# one, alive and not hung up, and the server must hold no more descriptors
# than it did before (no shadow pane leaked from the image it replaced).

PATH=/bin:/usr/bin:/usr/sbin:/usr/local/bin:/opt/homebrew/bin
TERM=screen
unset TMUX

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lkeep$$ -f/dev/null"
TMUX2="$TEST_TMUX -Lkeepr$$ -f/dev/null"
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

# wait_for <seconds> <shell command>: poll until the command exits 0.
wait_for()
{
	n=0
	while [ "$n" -lt "$(($1 * 5))" ]; do
		sh -c "$2" >/dev/null 2>&1 && return 0
		sleep 0.2
		n=$((n + 1))
	done
	return 1
}

# The server's open descriptors.
nfds()
{
	if [ -d /proc/$1/fd ]; then
		ls /proc/$1/fd | wc -l
	else
		lsof -p $1 -Ff 2>/dev/null | grep -c '^f[0-9]'
	fi
}

cat >"$TMP/hup.sh" <<EOF
#!/bin/sh
trap 'echo HUP >>$TMP/hup.log; exit 1' HUP
while :; do sleep 1; done
EOF
cat >"$TMP/alt.sh" <<'EOF'
#!/bin/sh
printf '\033[?1049h\033[Hfull screen'
exec sleep 600
EOF
chmod +x "$TMP/hup.sh" "$TMP/alt.sh"
SH=$(command -v zsh || echo /bin/sh)

# --- the scene -------------------------------------------------------------
$TMUX new-session -d -s alpha -x 80 -y 24 "sleep 600" || fail "new-session"
$TMUX new-window -d -t alpha "$SH -i"
$TMUX new-window -d -t alpha "$TMP/alt.sh"
$TMUX new-window -d -t alpha "$TMP/hup.sh"
$TMUX new-window -d -t alpha "sleep 601"
$TMUX respawn-pane -k -t alpha:4 "$SH -c 'echo respawned; exec $SH -i'" ||
    fail "respawn-pane"
$TMUX set -g remain-on-exit on

$TMUX2 new-session -d -s work -x 80 -y 24 "sleep 700" || fail "remote server"
$TMUX2 split-window -t work "sleep 701"
$TMUX2 new-window -t work "sleep 702"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -Lkeepr$$ -f/dev/null -C attach -t #{remote_session}"
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not connected"
wait_for 10 "[ \"\$($TMUX list-panes -s -t fakehost/work | wc -l)\" -eq 3 ]" ||
    fail "link has no panes"
sleep 1

SERVER=$($TMUX display -p '#{pid}')
local_panes()
{
	$TMUX list-panes -t alpha -s -F \
	    '#{pane_id} #{pane_pid} #{pane_tty} dead=#{pane_dead}'
}
local_panes >"$TMP/before"
[ "$(grep -c 'dead=0' "$TMP/before")" -eq 5 ] || fail "scene: $(cat "$TMP/before")"
fds_before=$(nfds $SERVER)

for round in 1 2; do
	$TMUX restart-server || fail "restart-server $round"
	sleep 2
	wait_for 10 "$TMUX has-session -t alpha" ||
	    fail "server did not come back ($round)"
	[ "$($TMUX display -p '#{pid}')" = "$SERVER" ] ||
	    fail "server pid changed ($round)"
	wait_for 10 "[ \"\$($TMUX display -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
	    fail "link did not come back ($round)"
	wait_for 10 "[ \"\$($TMUX list-panes -s -t fakehost/work | wc -l)\" -eq 3 ]" ||
	    fail "link panes did not come back ($round)"
	sleep 1

	local_panes >"$TMP/after"
	cmp -s "$TMP/before" "$TMP/after" ||
	    fail "local panes changed ($round): $(diff "$TMP/before" "$TMP/after" | tr '\n' ' ')"
	for pid in $(awk '{print $2}' "$TMP/before"); do
		kill -0 "$pid" 2>/dev/null || fail "pane process $pid died ($round)"
	done
	[ -s "$TMP/hup.log" ] && fail "a pane process was hung up ($round)"
	msgs=$($TMUX show-messages | grep 'restore:')
	[ -z "$msgs" ] || fail "restore errors ($round): $msgs"

	fds=$(nfds $SERVER)
	[ "$fds" -le "$fds_before" ] ||
	    fail "descriptors grew from $fds_before to $fds ($round)"
done

exit 0
