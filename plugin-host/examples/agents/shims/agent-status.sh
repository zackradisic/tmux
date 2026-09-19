#!/bin/sh
# Report an agent status to the tmux `agents` plugin.
#
#   agent-status.sh <working|needs_input|waiting|done> [text...]
#
# The pane travels in the command scope (-t $TMUX_PANE), so no id is
# needed. Runs from any hook that inherits TMUX and TMUX_PANE - a coding
# agent's process does, and so do its hook subprocesses.
#
# The trailing text is optional. On a `needs_input` report it is the
# reason the agent wants you, and the roster shows it on the row, so the
# "needs input" band says what each agent is blocked on without a jump.
# When the caller passes none, this takes it from the hook payload: the
# Claude `Notification` hook puts its own words in `message` ("Claude
# needs your permission to use Bash").
[ -n "$TMUX" ] || exit 0
[ -n "$TMUX_PANE" ] || exit 0
status=$1
[ -n "$status" ] || exit 0
shift
sock=${TMUX%%,*}

text=$*
# Only for `needs_input`: no other hook carries a message, and PreToolUse
# runs on every single tool call - not a place to read and parse stdin.
if [ -z "$text" ] && [ "$status" = needs_input ] && [ ! -t 0 ]; then
	# `sed` keeps the shim dependency-free (no jq / python needed), the
	# same way the codex shim reads its payload.
	text=$(sed -n \
	    's/.*"message"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
	    head -1)
	# Undo the JSON escapes that survive that: a message is one line on
	# a row, so an escaped newline or tab becomes a space.
	text=$(printf '%s' "$text" | sed -e 's/\\[nt]/ /g' -e 's/\\"/"/g')
fi
# One line: the roster puts this in a row, and a newline would tear the
# list apart. (The plugin flattens it too - this keeps the wire tidy.)
text=$(printf '%s' "$text" | tr '\n\r\t' '   ')

tmux -S "$sock" plugin-command -t "$TMUX_PANE" agents "$status $text" 2>/dev/null
exit 0
