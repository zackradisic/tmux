#!/bin/sh
# A restart (handoff) makes the remote links again before it has put the
# window and pane id counters back, so each link's placeholder window and
# pane took ids that restored windows already held. Two windows as @3: the
# second never enters the id tree, window_find_by_id answers with the
# other one, and a dead link then renames and writes its error into
# somebody else's window. The scene is laid out so that the last window
# restored has a lower id than an earlier one (sessions restore in name
# order, windows inside them in index order), which is where the counter
# was left low.
PATH=/bin:/usr/bin
TERM=screen
[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
[ -z "$TEST_SOCKET" ] && TEST_SOCKET=handoff-ids-test
TMUX="$TEST_TMUX -L$TEST_SOCKET -f/dev/null"
fail() {
	echo "FAIL: $*" >&2
	$TMUX kill-server 2>/dev/null
	exit 1
}
$TMUX kill-server 2>/dev/null
sleep 0.5

# alpha: @0 and @3; beta: @1 and @2. Restored in that order, the counter
# ends at 3 while @3 exists.
$TMUX new-session -d -s alpha -x 80 -y 20 "sleep 600" || fail "new-session"
$TMUX new-session -d -s beta -x 80 -y 20 "sleep 601" || fail "new-session beta"
$TMUX new-window -d -t beta "sleep 602"
$TMUX new-window -d -t alpha "sleep 603"
[ "$($TMUX list-windows -t alpha -F '#{window_id}' | tr '\n' ' ')" = "@0 @3 " ] ||
    fail "scene: alpha windows are $($TMUX list-windows -t alpha -F '#{window_id}' | tr '\n' ' ')"

# A link to a host that does not exist: the session stays on its
# placeholder window, and the restart makes the link again.
$TMUX remote-attach nosuchhost.invalid 2>/dev/null
i=0
while [ $i -lt 20 ]; do
	$TMUX has-session -t 'nosuchhost.invalid/' 2>/dev/null && break
	$TMUX list-sessions -F '#{session_name}' 2>/dev/null | grep -q '^nosuchhost' && break
	sleep 0.5
	i=$((i + 1))
done
LINK=$($TMUX list-sessions -F '#{session_name}' | grep '^nosuchhost' | head -1)
[ -n "$LINK" ] || fail "no link session: $($TMUX list-sessions -F '#{session_name}' | tr '\n' ' ')"

# --- restart ---------------------------------------------------------------
$TMUX restart-server || fail "restart-server"
sleep 3
$TMUX has-session -t alpha 2>/dev/null || fail "server did not come back"
i=0
while [ $i -lt 20 ]; do
	$TMUX list-sessions -F '#{session_name}' 2>/dev/null | grep -q '^nosuchhost' && break
	sleep 0.5
	i=$((i + 1))
done

dups=$($TMUX list-windows -a -F '#{window_id}' | sort | uniq -d | tr '\n' ' ')
[ -z "$dups" ] || fail "duplicate window ids after the restart: $dups ($($TMUX list-windows -a -F '#{session_name}:#{window_index}=#{window_id}' | tr '\n' ' '))"
dups=$($TMUX list-panes -a -F '#{pane_id}' | sort | uniq -d | tr '\n' ' ')
[ -z "$dups" ] || fail "duplicate pane ids after the restart: $dups"
dups=$($TMUX list-sessions -F '#{session_id}' | sort | uniq -d | tr '\n' ' ')
[ -z "$dups" ] || fail "duplicate session ids after the restart: $dups"

# The windows that were there keep their ids, and the placeholder is
# its own window: renaming it must not touch alpha's.
[ "$($TMUX list-windows -t alpha -F '#{window_id}' | tr '\n' ' ')" = "@0 @3 " ] ||
    fail "alpha's windows changed: $($TMUX list-windows -t alpha -F '#{window_id}' | tr '\n' ' ')"
ph=$($TMUX list-windows -t "$LINK" -F '#{window_id}' | head -1)
[ -n "$ph" ] || fail "the link has no placeholder window"
$TMUX rename-window -t "$ph" placeholder-renamed 2>/dev/null
$TMUX list-windows -t alpha -F '#{window_name}' | grep -q placeholder-renamed &&
    fail "renaming the placeholder renamed one of alpha's windows"

$TMUX kill-server 2>/dev/null
echo PASS
