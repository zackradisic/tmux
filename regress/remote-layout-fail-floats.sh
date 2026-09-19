#!/bin/sh
# A remote layout that layout_parse() rejects must not break the shadow
# window's z-order. remote_link_apply_layout() takes the local floating
# panes out of the window, lets layout_parse() rebuild the list of tiled
# panes and puts the floats back on top. layout_parse() only rebuilds the
# z-order when it succeeds, so after a rejected layout the floats were put
# back while still linked in, the list pointed at itself, and the next
# %layout-change spun the server forever in collect_floats() with memory
# growing until the machine ran out.
#
# B's control-mode stream passes through a filter that, once, chops the
# closing bracket off a %layout-change so that layout_construct() fails.

. ./remote-common.inc

# kill-server never returns from a spinning server, and neither does the
# SIGTERM handler, which runs from the event loop: kill A by pid.
cleanup()
{
	[ -n "$SERVER_PID" ] && kill -9 "$SERVER_PID" 2>/dev/null
	$TMUX2 kill-server 2>/dev/null
	rm -rf "$TMP"
}

# alive: a client gets an answer within 5 seconds.
alive()
{
	rm -f "$TMP/alive"
	( $TMUX display-message -p ok >"$TMP/alive" 2>/dev/null ) &
	if wait_for 5 "grep -q ok '$TMP/alive'"; then
		return 0
	fi
	return 1
}

FILTER=$TMP/filter.sh
cat >"$FILTER" <<FILTER_EOF
#!/bin/sh
$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t "\$1" | while IFS= read -r line; do
	case "\$line" in
	"%layout-change "*)
		if [ -e "$TMP/corrupt" ]; then
			rm -f "$TMP/corrupt"
			line=\$(printf '%s\n' "\$line" | awk '{ sub(/[]}]\$/, "", \$3); print }')
		fi
		;;
	esac
	printf '%s\n' "\$line"
done
FILTER_EOF
chmod +x "$FILTER"

$TMUX2 new-session -d -s work -x 80 -y 24 || fail "new-session on B"
$TMUX new-session -d -s local -x 80 -y 24 || fail "new-session on A"
SERVER_PID=$($TMUX display-message -p '#{pid}')
$TMUX set -s remote-ssh-command "$FILTER #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 6 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_connected}')\" = 1 ]" || fail "link not connected"

# A local floating pane on the shadow window.
fid=$($TMUX new-pane -dPF '#{pane_id}' -t fakehost/work -x 20 -y 6 -X 5 -Y 2 \
    'sleep 300') || fail "new-pane"

# The next %layout-change arrives mangled and layout_parse() rejects it.
touch "$TMP/corrupt"
$TMUX2 split-window -d -t work || fail "split-window 1 on B"
wait_for 6 "[ ! -e '$TMP/corrupt' ]" || fail "filter saw no %layout-change"
alive || fail "server spun after a rejected layout"

# A good layout follows: the window converges and the float is still there.
$TMUX2 split-window -d -t work || fail "split-window 2 on B"
alive || fail "server spun on the layout after a rejected one"
$TMUX2 split-window -d -t work || fail "split-window 3 on B"
alive || fail "server spun on the second layout after a rejected one"

want=$(geometry "$TMUX2" work)
wait_for 6 "[ \"\$($TMUX list-panes -t fakehost/work -F \
    '#{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}' | \
    grep -v '^$fid ' | cut -d' ' -f2-)\" = \"$want\" ]" ||
    fail "geometry did not converge: $($TMUX list-panes -t fakehost/work -F \
    '#{pane_id} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}')"
$TMUX display-message -p -t "$fid" '#{pane_id}' | grep -q "^$fid\$" ||
    fail "floating pane gone"

exit 0
