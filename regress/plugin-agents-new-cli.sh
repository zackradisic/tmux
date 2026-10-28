#!/bin/sh
# `plugin-command -t $TMUX_PANE agents "new <flags>"`: an agent (or a
# script) starts another agent with no form. Any flag that says what to
# start or where makes it direct; the caller pane gives the defaults -
# its folder, its session, its `@project` - and the result lands in the
# caller pane's `@agents_new` option ("%pane @window session", or
# "error: <why>"), since a plugin command cannot print to its caller.
# Check:
#
#   --command in the caller's session, rooted in its folder, tagged with
#     its window's @project, made in the background (the caller's window
#     stays current), the ids reported;
#   --folder, --session, --name and a quoted --project 'Ooga booga' all
#     take; --new-session makes a session of its own carrying the tag;
#   --resume <id> adds the harness's resume flag, and a roster row behind
#     the id lends its folder, name and tag; --fork adds --fork-session;
#   --split puts a pane beside the caller, tagged on the pane;
#   a missing session is an error, reported in @agents_new;
#   bare `new @host --profile dir` still means the form (not asserted
#     here beyond "no window was made").
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
unset AI_AGENT OPENCODE

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-new-cli-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d /tmp/agnc.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
WORK="$HOME/work"
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","send-keys","run-process","fs-list","fs-read-any"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

AGENT="sh -c 'AI_AGENT=claude exec cat'"

trap '$TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
fail() { echo "FAIL: $*" >&2; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"; exit 1; }
q() { db=$(find "$XDG_DATA_HOME" -name store.db | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
fmt() { $TMUX display-message -p -t "$1" "$2"; }
# The verb answers through the caller pane's option; wait for it.
result() {
	i=0
	while [ $i -lt 40 ]; do
		r=$($TMUX show-options -pqv -t "$PC" @agents_new)
		[ -n "$r" ] && [ "$r" != pending ] && { echo "$r"; return; }
		sleep 0.25; i=$((i + 1))
	done
	echo "timeout"
}
new() { $TMUX plugin-command -t "$PC" agents "new $*" || fail "plugin-command new $*"; result; }

[ -f "$WASM" ] || fail "agents.wasm not built"
mkdir -p "$WORK/proj-a" "$WORK/proj-r" "$WORK/other"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 -c "$WORK/proj-a" 'sleep 600' \
    || fail "new-session"
$TMUX new-session -d -s beta -x 200 -y 50 -c "$WORK" 'sleep 600' || fail "new-session beta"
PC=$($TMUX list-panes -t alpha:0 -F '#{pane_id}')
$TMUX set-option -w -t alpha:0 @project glm
$TMUX load-plugin -s server -o trust_env=1 -o "launch=fake = $AGENT" \
    -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c send-keys -c run-process -c fs-list -c fs-read-any \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.5

# --- the defaults come from the caller ------------------------------------
R=$(new --command fake --name w1)
case "$R" in %*\ @*\ alpha) ;; *) fail "unexpected result for w1: $R" ;; esac
P1=${R%% *}
$TMUX list-windows -t alpha -F '#{window_name}' | grep -qx w1 || fail "no window w1 in alpha"
[ "$(fmt "$P1" '#{pane_current_path}')" = "$WORK/proj-a" ] || fail "w1 is not rooted in the caller's folder"
[ "$(fmt "$P1" '#{pane_current_command}')" = cat ] || fail "w1 is not running the launcher's line"
[ "$(fmt "$P1" '#{@project}')" = glm ] || fail "w1 did not take the caller's @project: $(fmt "$P1" '#{@project}')"
[ "$(fmt alpha: '#{window_index}')" = 0 ] || fail "the new window took the focus"
# (The roster's detection of the new pane is the provider's business and
# is not asserted here: on macOS the env-marked fake agent is not seen,
# the same as in plugin-agents-new.sh.)

# --- every flag ------------------------------------------------------------
R=$(new --command fake --folder "$WORK/other" --session beta --name w2 --project "'Ooga booga'")
case "$R" in %*\ @*\ beta) ;; *) fail "unexpected result for w2: $R" ;; esac
P2=${R%% *}
[ "$(fmt "$P2" '#{window_name}')" = w2 ] || fail "w2 is not named"
[ "$(fmt "$P2" '#{pane_current_path}')" = "$WORK/other" ] || fail "w2 is not rooted in --folder"
[ "$(fmt "$P2" '#{@project}')" = "Ooga booga" ] || fail "w2 did not take the quoted project: $(fmt "$P2" '#{@project}')"

R=$(new --command fake --new-session --name solo --project x)
case "$R" in %*\ @*\ solo) ;; *) fail "unexpected result for solo: $R" ;; esac
$TMUX list-sessions -F '#{session_name}' | grep -qx solo || fail "session solo was not made"
[ "$(fmt solo: '#{pane_current_path}')" = "$WORK/proj-a" ] || fail "solo is not rooted in the caller's folder"
[ "$($TMUX show-options -qv -t solo @project)" = x ] || fail "solo did not carry the tag on the session"

# --- resume: the flag, and the row behind the id ----------------------------
R=$(new --command fake --resume claude:1b2c3d4e-0000-4000-8000-000000000001 --name r1)
P3=${R%% *}
fmt "$P3" '#{pane_start_command}' | grep -q -- "--resume 1b2c3d4e-0000-4000-8000-000000000001" ||
    fail "r1 did not get --resume: $(fmt "$P3" '#{pane_start_command}')"
fmt "$P3" '#{pane_start_command}' | grep -q -- "--fork-session" && fail "r1 was forked without --fork"

now=$(date +%s)000
q "INSERT INTO agents (id, kind, status, life, session, cwd, user_name, project, first_seen_ms, last_status_ms, ended_ms)
   VALUES ('claude:2b2c3d4e-0000-4000-8000-000000000002', 'claude', 'done', 'active', 'alpha',
           '$WORK/proj-r', 'row name', 'rowtag', $now, $now, $now)" || fail "insert row"
R=$(new --command fake --resume 2b2c3d4e-0000-4000-8000-000000000002 --fork)
P4=${R%% *}
[ "$(fmt "$P4" '#{window_name}')" = "row name" ] || fail "the row's name was not used: $(fmt "$P4" '#{window_name}')"
[ "$(fmt "$P4" '#{pane_current_path}')" = "$WORK/proj-r" ] || fail "the row's folder was not used"
fmt "$P4" '#{pane_start_command}' | grep -q -- "--resume 2b2c3d4e-0000-4000-8000-000000000002 --fork-session" ||
    fail "the fork did not get both flags: $(fmt "$P4" '#{pane_start_command}')"
# The caller's own project wins over the row's tag; from a pane with no
# project, the row's tag is the default.
[ "$(fmt "$P4" '#{@project}')" = glm ] || fail "the caller's project did not win: $(fmt "$P4" '#{@project}')"
$TMUX set-option -w -u -t alpha:0 @project
R=$(new --command fake --resume 2b2c3d4e-0000-4000-8000-000000000002 --name r3)
P5=${R%% *}
[ "$(fmt "$P5" '#{@project}')" = rowtag ] || fail "the row's tag was not the default: $(fmt "$P5" '#{@project}')"
$TMUX set-option -w -t alpha:0 @project glm

# --- split -------------------------------------------------------------------
R=$(new --command fake --split --project beside)
case "$R" in %*\ @*\ alpha) ;; *) fail "unexpected result for the split: $R" ;; esac
P6=${R%% *}
[ "$(fmt "$P6" '#{window_id}')" = "$(fmt "$PC" '#{window_id}')" ] || fail "the split is not in the caller's window"
[ "$($TMUX show-options -pqv -t "$P6" @project)" = beside ] || fail "the split pane was not tagged"
[ "$(fmt "$PC" '#{pane_active}')" = 1 ] || fail "the split took the focus"

# --- errors ------------------------------------------------------------------
R=$(new --command fake --session nope)
[ "$R" = "error: new: no session 'nope'" ] || fail "a missing session was not reported: $R"
R=$(new --command fake --folder "$WORK/missing")
case "$R" in "error: new: no folder"*) ;; *) fail "a missing folder was not reported: $R" ;; esac
R=$(new --project x --bogus)
case "$R" in "error: new: unknown flag --bogus") ;; *) fail "an unknown flag was not reported: $R" ;; esac

# --- no launch flag: the form, not a window ----------------------------------
before=$($TMUX list-windows -a | wc -l)
$TMUX plugin-command -t "$PC" agents "new --profile $WORK/other"
sleep 1
after=$($TMUX list-windows -a | wc -l)
[ "$before" = "$after" ] || fail "a bare new made a window"

echo "PASS"
exit 0
