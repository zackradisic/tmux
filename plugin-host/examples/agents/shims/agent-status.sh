#!/bin/sh
# Report an agent status to the tmux `agents` plugin.
#
#   agent-status.sh <working|needs_input|waiting|done> [task...]
#
# The pane travels in the command scope (-t $TMUX_PANE), so no id is
# needed. Runs from any hook that inherits TMUX and TMUX_PANE - a coding
# agent's process does, and so do its hook subprocesses.
[ -n "$TMUX" ] || exit 0
[ -n "$TMUX_PANE" ] || exit 0
status=$1
[ -n "$status" ] || exit 0
shift
sock=${TMUX%%,*}
# The plugin fork lives here; adjust if your tmux is elsewhere.
tmux -S "$sock" plugin-command -t "$TMUX_PANE" agents "$status $*" 2>/dev/null
exit 0
