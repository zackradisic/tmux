#!/bin/sh
# Projects. The picker groups by project unless told otherwise: a header
# per project, the rows under it by status, a machine column where the
# session column was. A row's project is, most specific first: the tag
# on the agent (`p` in the picker, or `project <name>` from its pane);
# the `@project` option its pane resolves to (pane, window, session -
# `project -w|-s <name>` from the pane sets those); the session's name;
# the directory; "other". Check:
#
#   the verb sets the window and session options, and the agent's tag;
#   the picker shows one header per project, the machine on every row,
#     and no band headers;
#   `^` narrows to a project; `v` cycles the grouping and the choice
#     survives a reopen; `p` tags the highlighted row from a prompt;
#   clearing the tag drops the row back to its option.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-project-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read",
            "pane-fds","fs-read","fs-list"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML

DUMP="$DEPLOY/dump.py"
cat >"$DUMP" <<'PY'
import sqlite3, sys, glob
dbs = glob.glob(sys.argv[1] + "/**/store.db", recursive=True)
if not dbs:
    print(""); sys.exit()
c = sqlite3.connect(dbs[0])
rows = list(c.execute(sys.argv[2]))
print("" if not rows or rows[0][0] is None else rows[0][0])
PY

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
q() { python3 "$DUMP" "$XDG_DATA_HOME" "$1"; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
}
# The project header lines: "▪ name".
headers() { screen | sed -n 's/^ *▪ \([^ ]*\).*/\1/p' | tr '\n' ' '; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Three agents: one in alpha, two in beta (one per window).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
$TMUX new-session -d -s beta -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session beta"
$TMUX new-window -d -t beta "sh -c 'AI_AGENT=claude exec sleep 600'" ||
	fail "new-window"
sleep 0.5
PA=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
PB=$($TMUX list-panes -t beta:0 -F '#{pane_id}' | head -1)
PC=$($TMUX list-panes -t beta:1 -F '#{pane_id}' | head -1)
[ -n "$PA" ] && [ -n "$PB" ] && [ -n "$PC" ] || fail "no panes"

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0
[ "$(q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL")" = 3 ] ||
	fail "expected 3 live rows, got $(q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL")"

# --- the verb: window and session options, and the agent's tag ---------

$TMUX plugin-command -t "$PC" agents "project -w glm"
sleep 0.5
[ "$($TMUX display-message -p -t "$PC" '#{@project}')" = glm ] ||
	fail "project -w did not set the window option"
$TMUX plugin-command -t "$PA" agents "project -s infra"
sleep 0.5
[ "$($TMUX display-message -p -t alpha '#{@project}')" = infra ] ||
	fail "project -s did not set the session option"
$TMUX plugin-command -t "$PA" agents "project tmux2"
sleep 0.5
[ "$(q "SELECT project FROM agents WHERE pane = ${PA#%}")" = tmux2 ] ||
	fail "project did not tag the agent: $(q "SELECT project FROM agents WHERE pane = ${PA#%}")"

# --- grouped by project ------------------------------------------------

open_picker
screen | grep -q '3 live, by project' || fail "header does not say by project"
# alpha's agent: the tag beats the session option. beta:1's: the window
# option. beta:0's: nothing set, so the session name.
for h in tmux2 glm beta; do
	screen | grep -q "^ *▪ $h" || fail "no project header $h: $(headers)"
done
screen | grep -q '^ *▪ infra' && fail "the tag did not beat the session option"
screen | grep -Eq '^ *(working|waiting|needs input)' && fail "a band header in project mode"
[ "$(screen | grep -c 'local  *claude')" = 3 ] ||
	fail "the machine column is not on every row"

# --- the ^ sigil narrows to a project ------------------------------------

keys /
keys '^' g l
screen | grep -q '^ *▪ glm' || fail "^gl lost the glm group"
screen | grep -q '^ *▪ beta' && fail "^gl kept the beta group"
screen | grep -q '^ *▪ tmux2' && fail "^gl kept the tmux2 group"
keys C-u
keys Escape

# --- v cycles the grouping and it sticks -------------------------------

keys v
screen | grep -q 'grouped by server' || fail "v did not group by server"
screen | grep -q '^ *working' || fail "server grouping shows no band"
screen | grep -q '^ *▪ ' && fail "a project header in server grouping"
keys v
screen | grep -q 'flat: bands only' || fail "v did not go flat"
screen | grep -q '3 live, flat' || fail "the header does not say flat"
keys v
screen | grep -q 'grouped by project' || fail "v did not come back to project"
keys v
[ "$(q "SELECT value FROM settings WHERE key = 'pick_group'")" = server ] ||
	fail "pick_group was not saved"
open_picker
screen | grep -q 'by project' && fail "the reopen forgot the grouping"
screen | grep -q '^ *working' || fail "the reopen is not grouped by server"
keys v; keys v
screen | grep -q '3 live, by project' || fail "not back to project"

# --- p tags the highlighted row from a prompt ----------------------------

keys g g
keys p
screen | grep -q 'project' || fail "no project prompt"
keys o p s Enter
sleep 0.6
screen | grep -q '^ *▪ ops' || fail "p did not tag the row: $(headers)"
screen | grep -q 'tagged ops' || fail "no status after tagging"
[ "$(q "SELECT COUNT(*) FROM agents WHERE project = 'ops'")" = 1 ] ||
	fail "the tag is not in the store"
# The info card says where the project came from.
keys i
screen | grep -q 'ops (tag)' || fail "the info card does not show the tag"
keys Escape

# --- clearing the tag: back to the option --------------------------------

$TMUX plugin-command -t "$PA" agents "project"
sleep 0.8
[ "$(q "SELECT project FROM agents WHERE pane = ${PA#%}")" = "" ] ||
	fail "project with no name did not clear the tag"
screen | grep -q '^ *▪ infra' || fail "alpha's agent did not fall back to the session option: $(headers)"
screen | grep -q '^ *▪ tmux2' && fail "the tmux2 group outlived its tag"

cleanup
exit 0
