#!/bin/sh
# The conversation search. A Claude pane's transcript (the jsonl under
# ~/.claude/projects) is read when a turn ends and when the agent ends,
# condensed to prompts, replies and one line per tool call, stored, and
# searched from the picker's search box - for live agents and for ones
# that are gone. Check:
#
#   a `waiting` report ingests the transcript: the turns land in the
#     store (a prompt, a reply, a condensed Edit);
#   the search box finds the agent by a word only its conversation holds,
#     and the row shows the matching line;
#   Tab shows the conversation in the preview in place of the live pane,
#     with its Markdown rendered; l gives it the keyboard, n steps to the
#     next match;
#   after the agent's pane is killed, the same search still finds it with
#     history off, and its preview shows the conversation.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-transcript-test"
[ -z "$WASM" ] && WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

# A private HOME: the session file, the transcript and the scoped fs-read
# prefixes all live here.
HOME=$(mktemp -d)
export HOME
XDG_DATA_HOME="$HOME/.local/share"
export XDG_DATA_HOME
mkdir -p "$HOME/.claude/sessions"

DEPLOY=$(mktemp -d)
cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane", "run-command", "mode", "db", "env-read",
            "pane-fds", "fs-read", "fs-list"]
[caps.env-read]
names = ["AI_AGENT", "OPENCODE"]
[caps.fs-read]
paths = ["~/.claude/sessions", "~/.claude/projects"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT

cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	echo "--- turns:" >&2
	turns >&2
	cleanup
	exit 1
}
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
# SHOW=1 prints the picker at each checkpoint, for eyeballing a change.
shot() { [ -n "$SHOW" ] && { echo "--- $*"; screen; }; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
turns() {
	sqlite3 "$XDG_DATA_HOME/tmux/plugins/agents/store.db" \
	    "SELECT seq || '|' || kind || '|' || text FROM turns ORDER BY seq" 2>/dev/null
}
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach -t alpha:0 >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
}
close_picker() {
	keys Escape
	kill $CTL 2>/dev/null
	CTL=
	sleep 0.5
}

[ -f "$WASM" ] || fail "agents.wasm not built"
command -v sqlite3 >/dev/null || fail "sqlite3 not found"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Window 0 is a plain shell, where the picker opens; the agent lives in
# window 1, so killing its pane later leaves the picker's window alone.
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 'sleep 600' || fail "new-session"
$TMUX new-window -d -t alpha:1 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-window"
sleep 0.5

WIN=$($TMUX list-panes -t alpha:1 -F '#{window_id}' | head -1)
PANE=$($TMUX list-panes -t alpha:1 -F '#{pane_id}' | head -1)
NUM=${PANE#%}
PID=$($TMUX list-panes -t alpha:1 -F '#{pane_pid}' | head -1)
[ -n "$NUM" ] && [ -n "$PID" ] || fail "no pane"

SID=25a936a2-cf9b-407d-9e4e-89e04a9636e7
now=$(date +%s)000
cat >"$HOME/.claude/sessions/$PID.json" <<EOF
{"pid":424242,"sessionId":"$SID","cwd":"$HOME","startedAt":$((now - 3600000)),
 "tmux":"alpha:$WIN.%$NUM","name":"hyperyaml-91","status":"busy","updatedAt":$now}
EOF

# The transcript Claude would write for that session: under the project
# directory named for the cwd (every non-alphanumeric character a dash).
SLUG=$(printf '%s' "$HOME" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$HOME/.claude/projects/$SLUG"
T="$HOME/.claude/projects/$SLUG/$SID.jsonl"
cat >"$T" <<'EOF'
{"type":"user","message":{"role":"user","content":"please measure the dflash2 acceptance length on the bench box"},"timestamp":"2026-09-16T08:20:43.045Z","version":"2.1.273","sessionId":"x"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Running the drafter benchmark now.\n\n## Plan\n\n- warm up the **acceptance** counter\n- read `run.py`\n\n```python\nk = 8\n```"},{"type":"tool_use","name":"Edit","input":{"file_path":"/x/bench/run.py","old_string":"k=4","new_string":"k=8\nverbose=True"}}]},"timestamp":"2026-09-16T08:20:50.000Z"}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"a very long tool result that must never be stored"}]},"timestamp":"2026-09-16T08:20:51.000Z"}
EOF

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0

# Open the picker: enrich-at-render reads the session file, binds the
# durable id and records the transcript path.
open_picker
screen | grep -q 'hyperyaml-91' || fail "resolved name missing"

# The turn ends: a `waiting` report ingests the transcript.
$TMUX plugin-command -t "$PANE" agents waiting || fail "report"
sleep 1.5
n=$(sqlite3 "$XDG_DATA_HOME/tmux/plugins/agents/store.db" "SELECT COUNT(*) FROM turns" 2>/dev/null)
[ "$n" = "3" ] || fail "expected 3 turns, got $n"
turns | grep -q '^0|user|please measure the dflash2' || fail "the prompt is not turn 0"
turns | grep -q '^1|assistant|Running the drafter benchmark' || fail "the reply is not turn 1"
turns | grep -q '^2|tool|Edit bench/run.py +2 −1$' || fail "the edit was not condensed"
turns | grep -q 'never be stored' && fail "a tool result was stored"

# Search by a word only the conversation holds: the row stays, with the
# matching line on it.
keys /
keys -l dflash2
sleep 0.8
shot "search: dflash2"
screen | grep -q 'hyperyaml-91' || fail "conversation search did not keep the row"
screen | grep -q 'dflash2 acceptance length' || fail "the matching line is not on the row"
# A word from nowhere empties the list.
keys C-u
keys -l zyxwvut
sleep 0.5
screen | grep -q 'hyperyaml-91' && fail "a miss kept the row"
keys C-u
keys -l acceptance
keys Escape
sleep 0.5

# Tab: the conversation in place of the live pane.
keys Tab
sleep 1.0
shot "Tab: conversation of a live agent"
screen | grep -q 'conversation · 3 turns' || fail "Tab did not show the conversation"
screen | grep -q 'please measure the' || fail "the prompt is not in the preview"
screen | grep -q 'Edit bench/run.py' || fail "the tool line is not in the preview"
# Markdown: the heading without its hashes, bullets, a fenced block.
screen | grep -q '│Plan' || fail "the heading was not rendered"
screen | grep -q '• warm up the acceptance counter' || fail "the list was not rendered"
screen | grep -q '┌─ python' || fail "the code fence was not rendered"
screen | grep -q '│ k = 8' || fail "the code line was not rendered"
# The query has two matches (the prompt and the bullet); the preview
# opened on the first. l: the conversation takes the keyboard, and n
# steps between them.
screen | grep -q 'match 1/2' || fail "the preview did not open on the first match"
keys l
screen | grep -q 'Esc back to list' || fail "the footer does not say how to leave"
keys n
screen | grep -q 'match 2/2' || fail "n did not step to the second match"
keys n
screen | grep -q 'match 1/2' || fail "n did not wrap to the first match"
keys Escape
screen | grep -q 'Esc back to list' && fail "Esc did not give the keyboard back"
close_picker

# The agent dies. Its pane goes, the row ends, the rest of the transcript
# (nothing new here) is read, and the index is snapshotted.
$TMUX kill-pane -t "$PANE" || fail "kill-pane"
sleep 1.5
ended=$(sqlite3 "$XDG_DATA_HOME/tmux/plugins/agents/store.db" \
    "SELECT COUNT(*) FROM agents WHERE ended_ms IS NOT NULL" 2>/dev/null)
[ "$ended" = "1" ] || fail "the row did not end (ended=$ended)"
snap=$(sqlite3 "$XDG_DATA_HOME/tmux/plugins/agents/store.db" \
    "SELECT COUNT(*) FROM search_index" 2>/dev/null)
[ "$snap" = "1" ] || fail "no index snapshot after the agent ended"

# History is off, so the finished row is not in the list - until a query
# finds it in its conversation.
open_picker
screen | grep -q 'hyperyaml-91' && fail "a finished row showed with history off"
keys /
keys -l acceptance
sleep 1.0
screen | grep -q 'hyperyaml-91' || fail "the killed agent was not found by its conversation"
screen | grep -q 'done' || fail "the found row is not in the done band"
keys Escape
sleep 1.0
shot "killed agent found, preview is its conversation"
# No pane to blit: the preview is the conversation, opened on the hit.
screen | grep -q 'conversation · 3 turns' || fail "a finished agent's preview is not its conversation"
screen | grep -q 'Running the drafter benchmark' || fail "the reply is not in the preview"

cleanup
exit 0
