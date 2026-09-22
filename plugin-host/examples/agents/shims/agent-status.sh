#!/bin/sh
# Report an agent status to the tmux `agents` plugin.
#
#   agent-status.sh <working|needs_input|waiting|done> [text...]
#   agent-status.sh pretool          # from a PreToolUse hook: decides itself
#
# The pane travels in the command scope (-t $TMUX_PANE), so no id is
# needed. Runs from any hook that inherits TMUX and TMUX_PANE - a coding
# agent's process does, and so do its hook subprocesses.
#
# `pretool` reads the PreToolUse payload: a tool that opens a dialog for
# the user - Claude's AskUserQuestion, ExitPlanMode - is `needs_input`
# with the question as the reason, and every other tool is `working`.
# That is the one exact "it is asking me something" signal Claude gives
# while a turn is still running; the Stop hook covers the end of it.
#
# The trailing text is optional. On a `needs_input` report it is the
# reason the agent wants you, and the roster shows it on the row, so the
# "needs input" band says what each agent is blocked on without a jump.
# When the caller passes none, this takes it from the hook payload: the
# question of an AskUserQuestion, or the `message` of a Notification
# ("Claude needs your permission to use Bash").
[ -n "$TMUX" ] || exit 0
[ -n "$TMUX_PANE" ] || exit 0
status=$1
[ -n "$status" ] || exit 0
shift
sock=${TMUX%%,*}

# A hook's PATH does not always have the fork on it - it inherits the
# agent's, which is whatever the shell had when the agent started. Name
# the binary if `tmux` here is the wrong one, or nothing:
#   TMUXBIN=$HOME/.local/share/tmux2/bin/tmux
TMUXBIN=${TMUXBIN:-tmux}

text=$*
# The payload is read only when something below needs it: `pretool`
# always (the tool name is in it), a bare `needs_input` for its message.
# A plain `working` from PreToolUse never read stdin; `pretool` does, but
# one `sed` over a few KB is well under a millisecond.
payload=
if [ ! -t 0 ]; then
	case $status in
	pretool) payload=$(cat) ;;
	needs_input) [ -z "$text" ] && payload=$(cat) ;;
	esac
fi
# The first string value of a JSON key, on one line. `sed` keeps the shim
# dependency-free (no jq / python needed), the same way the codex shim
# reads its payload. Good enough for a tool name or one question; a
# quote inside the text cuts it short, which is a shorter note, not a
# wrong one.
field() {
	printf '%s\n' "$payload" |
	    sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p" |
	    head -1
}
if [ "$status" = pretool ]; then
	case $(field tool_name) in
	AskUserQuestion)
		status=needs_input
		[ -n "$text" ] || text=$(field question)
		;;
	ExitPlanMode)
		status=needs_input
		[ -n "$text" ] || text="plan ready for review"
		;;
	*) status=working ;;
	esac
elif [ -z "$text" ] && [ "$status" = needs_input ] && [ -n "$payload" ]; then
	text=$(field message)
fi
# Undo the JSON escapes that survive that: a message is one line on a
# row, so an escaped newline or tab becomes a space.
text=$(printf '%s' "$text" | sed -e 's/\\[nt]/ /g' -e 's/\\"/"/g')
# One line: the roster puts this in a row, and a newline would tear the
# list apart. (The plugin flattens it too - this keeps the wire tidy.)
text=$(printf '%s' "$text" | tr '\n\r\t' '   ')

"$TMUXBIN" -S "$sock" plugin-command -t "$TMUX_PANE" agents "$status $text" 2>/dev/null
exit 0
