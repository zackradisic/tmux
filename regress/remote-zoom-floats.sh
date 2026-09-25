#!/bin/sh
# A zoomed remote window with floating panes must not break its shadow.
#
# Zoomed, every pane's cell is parked in saved_layout_cell, and the
# window_layout format dumps the saved tree. The dump used to take the
# float list from the live cells, so a zoomed window's layout carried its
# float leaves inline with no <...> part: the link could not strip them
# and mirrored them as tiled panes. Killing those mirrors on unzoom then
# went through a z-order they were never in, and the TAILQ_REMOVE spliced
# a local float (a toast) out of the list; the sync dropped it from the
# pane list and never put it back, leaving a live pane in no list with
# the window's active pointer still on it. The next key in that window
# hit fatalx("invalid current find state").
#
# Asserts: the zoomed layout string keeps its float suffix; the shadow
# never mirrors a float; the toast survives zoom and unzoom in the pane
# list; exactly one pane is active; no pane claims the window without
# being listed in it.

. ./remote-common.inc

link_setup
WID=$($TMUX display -p -t fakehost/work:0 '#{window_id}')
MAIN=$($TMUX list-panes -t fakehost/work:0 -F '#{pane_id}' | head -1)

# A control client on the shadow session, as the plugin's client would be.
( sleep 0.3; echo "resize-pane -Z -t $MAIN"; sleep 1.5;
  echo "resize-pane -Z -t $MAIN"; sleep 1.5;
  echo "new-pane -d -T toast2 -X 40 -Y 0 -t $WID -x 30 -y 4 'sleep 60'";
  sleep 60 ) | $TMUX -C attach -t fakehost/work >/dev/null 2>&1 &
CTL=$!

# Six off-screen floats on the remote, like agent teammates.
for xy in "50 8" "50 13" "40 8" "60 13" "60 13" "40 13"; do
	set -- $xy
	$TMUX2 new-pane -d -t work:0 -x 20 -y 3 -X $1 -Y $2 'sleep 60' ||
	    fail "new-pane on B"
done
sleep 0.8

# A local toast in the shadow window.
$TMUX new-pane -d -T toast -X 40 -Y 0 -t $WID -x 30 -y 4 'sleep 60' ||
    fail "toast"
TOAST=$($TMUX list-panes -t $WID -F '#{pane_id} #{pane_title}' |
    awk '$2 == "toast" { print $1 }')
[ -n "$TOAST" ] || fail "no toast pane"

# Shadow panes: id/remote id/active/floating.
panes()
{
	$TMUX list-panes -t $WID -F '#{pane_id}/#{pane_remote_id}/#{pane_active}/#{pane_floating_flag}' | tr '\n' ' '
}

# The invariants, named for the step.
check()
{
	n=$($TMUX list-panes -t $WID -F '#{pane_remote_id}' | grep -c .)
	[ "$n" -eq 1 ] || fail "$1: $n remote panes mirrored: $(panes)"
	a=$($TMUX list-panes -t $WID -F '#{pane_active}' | awk '{ a += $1 } END { print a + 0 }')
	[ "$a" -eq 1 ] || fail "$1: $a active panes: $(panes)"
	$TMUX list-panes -t $WID -F '#{pane_id}' | grep -qx "$TOAST" ||
	    fail "$1: toast $TOAST not listed: $(panes)"
	w=$($TMUX display -p -t $TOAST '#{window_id}')
	[ "$w" = "$WID" ] || fail "$1: toast in $w, not $WID"
}
check start

# Zoom on the shadow's main pane goes to the remote.
wait_for 6 "[ \"\$($TMUX2 display -p -t work:0 '#{window_zoomed_flag}')\" = 1 ]" ||
    fail "remote did not zoom"
layout=$($TMUX2 display -p -t work:0 '#{window_layout}')
case $layout in
*'<'*'>') ;;
*) fail "zoomed layout lost its floats: $layout" ;;
esac
sleep 0.5
check zoomed

wait_for 6 "[ \"\$($TMUX2 display -p -t work:0 '#{window_zoomed_flag}')\" = 0 ]" ||
    fail "remote did not unzoom"
sleep 0.5
check unzoomed

# Toast churn in the shadow while the remote resizes.
wait_for 6 "$TMUX list-panes -t $WID -F '#{pane_title}' | grep -qx toast2" ||
    fail "second toast"
$TMUX kill-pane -t $TOAST
TOAST=$($TMUX list-panes -t $WID -F '#{pane_id} #{pane_title}' |
    awk '$2 == "toast2" { print $1 }')
$TMUX2 resize-window -t work -x 100 -y 30
sleep 0.8
check resized

# No pane may claim the window without being listed in it.
listed=$($TMUX list-panes -t $WID -F '#{pane_id}' | tr '\n' ' ')
for n in $(seq 0 40); do
	r=$($TMUX display -p -t %$n '#{pane_id} #{window_id}' 2>/dev/null) ||
	    continue
	set -- $r
	[ "$2" = "$WID" ] || continue
	case " $listed " in
	*" $1 "*) ;;
	*) fail "pane $1 claims $WID but is not listed: $listed" ;;
	esac
done

kill $CTL 2>/dev/null
exit 0
