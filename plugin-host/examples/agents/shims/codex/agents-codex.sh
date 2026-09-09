#!/bin/sh
# Codex hook -> the tmux `agents` plugin.
#
#   agents-codex.sh <working|needs_input|waiting|done>
#
# Codex passes a JSON object on stdin (Claude-compatible): it carries
# `session_id` (the durable id) and `transcript_path` (the rollout file the
# roster reads for the session nickname). This shim reports the durable id
# once (`identify`) and the turn status, both keyed by the pane. The pane
# travels in the command scope (-t $TMUX_PANE); no id is sent as a key.
[ -n "$TMUX" ] || exit 0
[ -n "$TMUX_PANE" ] || exit 0
status=$1
sock=${TMUX%%,*}

# Read the hook payload and pull two string fields out of the JSON. `sed`
# keeps the shim dependency-free (no jq / python needed).
payload=$(cat)
field() {
	printf '%s' "$payload" |
	    sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p" |
	    head -1
}
sid=$(field session_id)
transcript=$(field transcript_path)

# Bind the pane's row to the durable codex id, and hand over the transcript
# so the roster can read the session nickname from it.
if [ -n "$sid" ]; then
	tmux -S "$sock" plugin-command -t "$TMUX_PANE" agents \
	    "identify codex:$sid${transcript:+ $transcript}" 2>/dev/null
fi
# Report the turn status.
[ -n "$status" ] &&
    tmux -S "$sock" plugin-command -t "$TMUX_PANE" agents "$status" 2>/dev/null
exit 0
