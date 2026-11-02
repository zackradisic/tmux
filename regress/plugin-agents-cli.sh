#!/bin/sh
# The verbs that answer on the caller's terminal, for an agent reading
# the roster from its own pane: `ls` lists the live agents (id, state,
# project, name, directory), `--project` narrows, `--json` is a line per
# agent; `show` prints one agent's card; `transcript` prints its stored
# conversation (prompts, replies, tool lines; never a tool result);
# `search` finds the agent that talked about a word; `whoami` names the
# agent in the -t pane. Everything is read from the store at once, so a
# script gets its answer in one call.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
unset AI_AGENT OPENCODE CLAUDECODE CLAUDE_CODE_SESSION_ID TMUX TMUX_PANE

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-cli-test"
[ -z "$WASM" ] && WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d /tmp/agcli.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
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

trap '$TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
fail() { echo "FAIL: $*" >&2; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"; exit 1; }
agents() { $TMUX plugin-command agents "$*"; }
agents_t() { p=$1; shift; $TMUX plugin-command -t "$p" agents "$*"; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 'sleep 600' || fail "new-session"
$TMUX new-window -d -t alpha:1 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-window"
$TMUX new-window -d -t alpha:2 "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-window 2"
sleep 0.5
$TMUX set-option -w -t alpha:2 @project glm

sess() {
	# A Claude session file binding the pane in window $1 to id $2 named $3.
	PANE=$($TMUX list-panes -t "alpha:$1" -F '#{pane_id}' | head -1)
	WIN=$($TMUX list-panes -t "alpha:$1" -F '#{window_id}' | head -1)
	PID=$($TMUX list-panes -t "alpha:$1" -F '#{pane_pid}' | head -1)
	now=$(date +%s)000
	cat >"$HOME/.claude/sessions/$PID.json" <<EOF
{"pid":424242,"sessionId":"$2","cwd":"$HOME","startedAt":$((now - 3600000)),
 "tmux":"alpha:$WIN.${PANE}","name":"$3","status":"busy","updatedAt":$now}
EOF
	echo "$PANE"
}
SID1=25a936a2-cf9b-407d-9e4e-89e04a9636e7
SID2=35a936a2-cf9b-407d-9e4e-89e04a963602
P1=$(sess 1 $SID1 hyperyaml-91)
P2=$(sess 2 $SID2 glm-drafter)

# The transcript of the first agent.
SLUG=$(printf '%s' "$HOME" | sed 's/[^A-Za-z0-9]/-/g')
mkdir -p "$HOME/.claude/projects/$SLUG"
cat >"$HOME/.claude/projects/$SLUG/$SID1.jsonl" <<'EOF'
{"type":"user","message":{"role":"user","content":"please measure the dflash2 acceptance length on the bench box"},"timestamp":"2026-09-16T08:20:43.045Z","version":"2.1.273","sessionId":"x","cwd":"/work/bench-box","gitBranch":"feature/dflash"}
{"type":"assistant","message":{"role":"assistant","model":"claude-test-1","content":[{"type":"text","text":"Running the drafter benchmark now."},{"type":"tool_use","name":"Edit","input":{"file_path":"/x/bench/run.py","old_string":"k=4","new_string":"k=8\nverbose=True"}}]},"timestamp":"2026-09-16T08:20:50.000Z"}
{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"a very long tool result that must never be stored"}]},"timestamp":"2026-09-16T08:20:51.000Z"}
{"type":"assistant","message":{"role":"assistant","model":"claude-test-1","content":[{"type":"text","text":"Acceptance length is 3.9 at k=16."}]},"timestamp":"2026-09-16T08:21:50.000Z"}
EOF

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0
# A report from each pane: the rows exist, resolve to their ids, and the
# first agent's transcript is read.
agents_t "$P1" waiting || fail "report 1"
agents_t "$P2" working "training the drafter" || fail "report 2"
sleep 2

# --- ls ------------------------------------------------------------------------
# The first call may still show provisional rows (the session files are
# read on a render, a timer, or after a call like this one); the second
# has the durable ids.
agents ls >/dev/null
sleep 1.5
R=$(agents ls)
# (Its state is the session file's word, busy = working, which beats
# the report; the second agent's report carries a task.)
echo "$R" | grep -Eq "^claude:$SID1 +(working|waiting) +alpha +hyperyaml-91 +/work/bench-box" || fail "ls lacks the first agent with its project, name and cwd: $R"
echo "$R" | grep -q "^claude:$SID2 .* working .* glm .* glm-drafter" || fail "ls lacks the second with its project and name: $R"
echo "$R" | grep -q "hyperyaml-91" || fail "ls lacks the first agent's name: $R"
[ "$(echo "$R" | wc -l | tr -d ' ')" = 2 ] || fail "ls has $(echo "$R" | wc -l) lines, not 2: $R"
R=$(agents 'ls --project glm')
echo "$R" | grep -q "$SID2" || fail "ls --project lacks the glm agent: $R"
echo "$R" | grep -q "$SID1" && fail "ls --project glm lists the other: $R"
R=$(agents 'ls --project nothing')
[ "$R" = 'no agents in project "nothing"' ] || fail "ls of an empty project: $R"
R=$(agents 'ls --json')
echo "$R" | grep -q "\"id\":\"claude:$SID2\"" || fail "ls --json lacks the id: $R"
echo "$R" | python3 -c 'import json,sys
rows=[json.loads(l) for l in sys.stdin if l.strip()]
assert len(rows)==2, rows
g=[r for r in rows if r["project"]=="glm"][0]
assert g["state"]=="working" and g["task"]=="training the drafter" and g["server"]=="local", g' || fail "ls --json shape"
R=$(agents 'ls --bogus')
echo "$R" | grep -q "unknown flag" || fail "ls --bogus did not complain: $R"

# --- show ----------------------------------------------------------------------
R=$(agents "show claude:$SID1")
echo "$R" | grep -q "^name .*hyperyaml-91" || fail "show lacks the name: $R"
echo "$R" | grep -q "^where .*alpha:" || fail "show lacks where: $R"
echo "$R" | grep -Eq "^status .*(working|waiting)" || fail "show lacks the status: $R"
echo "$R" | grep -q "^cwd " || fail "show lacks the cwd: $R"
echo "$R" | grep -q "^transcript .*$SID1.jsonl" || fail "show lacks the transcript path: $R"
echo "$R" | grep -q "^turns .*1 prompts · 2 replies · 1 tool calls (1 edits" || fail "show lacks the turn counts: $R"
echo "$R" | grep -q "^resume .*claude --resume $SID1" || fail "show lacks the resume line: $R"
# The bare id works too.
R=$(agents "show $SID2")
echo "$R" | grep -q "^status .*working · training the drafter" || fail "show by bare id: $R"
R=$(agents "show claude:nope")
echo "$R" | grep -q "no agent" || fail "show of a stranger: $R"

# --- transcript ------------------------------------------------------------------
R=$(agents "transcript claude:$SID1")
echo "$R" | grep -q "^# hyperyaml-91 (claude:$SID1) · 4 of 4 turns" || fail "transcript heading: $R"
echo "$R" | grep -q "^## user" || fail "transcript lacks the prompt heading: $R"
echo "$R" | grep -q "please measure the dflash2 acceptance length" || fail "transcript lacks the prompt: $R"
echo "$R" | grep -q "^## agent" || fail "transcript lacks the reply heading: $R"
echo "$R" | grep -q "^· Edit .*run.py" || fail "transcript lacks the tool line: $R"
echo "$R" | grep -q "must never be stored" && fail "a tool result leaked into the transcript: $R"
echo "$R" | grep -q "Acceptance length is 3.9" || fail "transcript lacks the last reply: $R"
R=$(agents "transcript claude:$SID1 -n 1")
echo "$R" | grep -q "1 of 4 turns" || fail "-n 1 heading: $R"
echo "$R" | grep -q "Acceptance length is 3.9" || fail "-n 1 is not the last turn: $R"
echo "$R" | grep -q "please measure" && fail "-n 1 printed more than one turn: $R"
R=$(agents "transcript claude:$SID2")
echo "$R" | grep -q "no turns stored yet" || fail "transcript of an agent without one: $R"
R=$(agents "transcript")
echo "$R" | grep -q "transcript <agent-id>" || fail "transcript without an id: $R"

# --- search ----------------------------------------------------------------------
R=$(agents 'search dflash2 acceptance')
echo "$R" | grep -q "^claude:$SID1  hyperyaml-91  .*dflash2" || fail "search did not find the agent with its line: $R"
R=$(agents 'search zebrafish')
echo "$R" | grep -q "no conversation matches" || fail "search of nothing: $R"

# --- whoami --------------------------------------------------------------------
R=$(agents_t "$P2" whoami)
echo "$R" | grep -q "^id .*claude:$SID2" || fail "whoami lacks the id: $R"
echo "$R" | grep -q "^project .*glm  (@project option)" || fail "whoami lacks the project: $R"
R=$(agents_t "$($TMUX list-panes -t alpha:0 -F '#{pane_id}')" whoami)
echo "$R" | grep -q 'no agent in' || fail "whoami from a plain pane: $R"

# --- help ------------------------------------------------------------------------
agents help | grep -q 'transcript <agent-id>' || fail "help lacks transcript"
agents 'ls --help' | grep -q -- '--project <name>' || fail "ls --help"
agents 'transcript --help' | grep -q 'last 30 turns' || fail "transcript --help"

echo "PASS"
exit 0
