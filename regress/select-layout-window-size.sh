#!/bin/sh

# select-layout resizes the window to the layout it applies, whatever the
# shape of the layout. A single-pane layout used to be skipped, so a window
# restored at a different size kept its own size with a layout that did not
# fill it, and nothing resized the layout again until the client changed size.

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -LtestL$$ -f/dev/null"
$TMUX kill-server 2>/dev/null

fail()
{
	echo "$*" >&2
	$TMUX kill-server 2>/dev/null
	exit 1
}

must_equal()
{
	[ "$1" = "$2" ] || fail "$3: got '$1', expected '$2'"
}

$TMUX new-session -d -x 80 -y 24 || exit 1

# A single-pane layout of another size: the window follows it.
$TMUX select-layout 'b99d,202x53,0,0,0' || fail "select-layout single failed"
must_equal "$($TMUX display -p '#{window_width}x#{window_height}')" 202x53 \
	"window after single-pane layout"
must_equal "$($TMUX display -p '#{window_layout}')" 'b99d,202x53,0,0,0' \
	"layout after single-pane layout"

# A split layout of another size: the same. Build it at 281x64, shrink the
# window back, then apply it.
$TMUX resize-window -x 281 -y 64 || exit 1
$TMUX split-window -d -h || exit 1
split=$($TMUX display -p '#{window_layout}')
$TMUX resize-window -x 80 -y 24 || exit 1
$TMUX set -wu window-size || exit 1
must_equal "$($TMUX display -p '#{window_width}x#{window_height}')" 80x24 \
	"window before split layout"
$TMUX select-layout "$split" || fail "select-layout split failed"
must_equal "$($TMUX display -p '#{window_width}x#{window_height}')" 281x64 \
	"window after split layout"
must_equal "$($TMUX display -p '#{window_layout}')" "$split" \
	"layout after split layout"

# A later window resize fits the layout to the window again.
$TMUX resize-window -x 120 -y 30 || fail "resize-window failed"
must_equal "$($TMUX display -p '#{window_layout}' | cut -d, -f2)" 120x30 \
	"layout root after resize-window"

$TMUX kill-server 2>/dev/null
exit 0
