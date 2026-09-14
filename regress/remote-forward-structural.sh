#!/bin/sh
# Structural commands on a shadow object run on the remote, and both trees
# end up the same. A local command leaves the remote alone, and a command
# that mixes local and remote panes is an error.

. ./remote-common.inc

link_setup

# split-window on A: two panes with the same geometry on both servers.
$TMUX split-window -t fakehost/work:0 -h || fail "split-window"
wait_for 6 "[ \"\$($TMUX2 list-panes -t work:0 | wc -l)\" -eq 2 ]" || fail "B not split"
wait_for 6 "[ \"\$($TMUX list-panes -t fakehost/work:0 | wc -l)\" -eq 2 ]" || fail "A not split"
[ "$(geometry "$TMUX" fakehost/work:0)" = "$(geometry "$TMUX2" work:0)" ] ||
    fail "geometry differs after split"

# The new pane is active on both sides.
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work:0 '#{pane_remote_id}')\" = \
    \"\$($TMUX2 display-message -p -t work:0 '#{pane_id}')\" ]" || fail "active pane differs"

# rename-window on A renames on B and the shadow follows.
$TMUX rename-window -t fakehost/work:0 renamed || fail "rename-window"
wait_for 6 "[ \"\$($TMUX2 display-message -p -t work:0 '#{window_name}')\" = renamed ]" ||
    fail "B not renamed"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work:0 '#{window_name}')\" = renamed ]" ||
    fail "A not renamed"

# A split on B while a local floating pane sits over the shadow window.
$TMUX new-pane -d -t fakehost/work:0 -x 20 -y 5 -X 2 -Y 2 'sleep 100' || fail "new-pane"
$TMUX2 split-window -t work:0 -v || fail "split on B"
wait_for 6 "[ \"\$($TMUX2 list-panes -t work:0 | wc -l)\" -eq 3 ]" || fail "B not split (2)"
wait_for 6 "[ \"\$($TMUX list-panes -t fakehost/work:0 -F '#{pane_remote_id}' | grep -c '^%')\" -eq 3 ]" ||
    fail "A not split (2)"
[ "$($TMUX list-panes -t fakehost/work:0 -F '#{pane_floating_flag}' | grep -c 1)" -eq 1 ] ||
    fail "floating pane lost"
$TMUX list-panes -t fakehost/work:0 -F '#{pane_floating_flag} #{pane_left},#{pane_top} #{pane_width}x#{pane_height}' |
    grep -v '^1 ' | cut -d' ' -f2- >"$TMP/a"
geometry "$TMUX2" work:0 >"$TMP/b"
cmp -s "$TMP/a" "$TMP/b" || fail "geometry differs with a float: $(cat "$TMP/a" | tr '\n' ' ') vs $(cat "$TMP/b" | tr '\n' ' ')"
$TMUX kill-pane -t "$($TMUX list-panes -t fakehost/work:0 -F '#{pane_floating_flag} #{pane_id}' | grep '^1 ' | cut -d' ' -f2)"

# kill-pane on A kills on B.
victim=$($TMUX list-panes -t fakehost/work:0 -F '#{pane_id} #{pane_remote_id}' | tail -1)
$TMUX kill-pane -t "${victim% *}" || fail "kill-pane"
wait_for 6 "! $TMUX2 list-panes -t work:0 -F '#{pane_id}' | grep -qx '${victim#* }'" ||
    fail "B pane survived"
wait_for 6 "[ \"\$($TMUX list-panes -t fakehost/work:0 | wc -l)\" -eq 2 ]" || fail "A pane survived"
[ "$(geometry "$TMUX" fakehost/work:0)" = "$(geometry "$TMUX2" work:0)" ] ||
    fail "geometry differs after kill"

# A split on the local session leaves B alone.
before=$($TMUX2 list-panes -s -t work | wc -l)
$TMUX split-window -t local:0 || fail "local split"
sleep 0.5
[ "$($TMUX2 list-panes -s -t work | wc -l)" -eq "$before" ] || fail "B changed on a local split"

# Mixed local and remote panes are refused.
$TMUX swap-pane -s local:0.0 -t fakehost/work:0.0 2>"$TMP/err" && fail "mixed swap allowed"
grep -q "cannot mix" "$TMP/err" || fail "wrong error: $(cat "$TMP/err")"
$TMUX swap-pane -s fakehost/work:0.0 -t local:0.0 2>"$TMP/err" && fail "mixed swap allowed (2)"

# new-window on the shadow session lands on B with the same index.
$TMUX new-window -t fakehost/work -n fresh || fail "new-window"
wait_for 6 "$TMUX2 list-windows -t work -F '#{window_name}' | grep -qx fresh" || fail "B has no fresh"
wait_for 6 "$TMUX list-windows -t fakehost/work -F '#{window_name}' | grep -qx fresh" || fail "A has no fresh"
[ "$($TMUX display-message -p -t fakehost/work:fresh '#{window_index}')" = \
    "$($TMUX2 display-message -p -t work:fresh '#{window_index}')" ] || fail "index differs"

# kill-window on B removes the shadow.
$TMUX2 kill-window -t work:fresh || fail "kill-window on B"
wait_for 6 "! $TMUX list-windows -t fakehost/work -F '#{window_name}' | grep -qx fresh" ||
    fail "shadow window survived"
exit 0
