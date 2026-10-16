#!/bin/sh
# Projects, part two: the project prompt completes from the projects on
# the roster; `R` on a project header (`h` from a row) renames the
# project, every row in it going to the new name by the thing that put
# it there (tag, `@project` option, session); the new-agent form has a
# project field prefilled from the row and the new window gets it as
# `@project`. Check:
#
#   `p` opens with a dropdown of every project; typing narrows it, Enter
#     takes the highlighted one into the box, Enter again tags the row;
#   `h` then `R` renames: a tagged row is retagged, a window option is
#     set on the window, a session-named row gets the session option;
#     the header follows and so does the cursor;
#   `n` from a row: the form's project field holds the row's project,
#     and the new window carries it as `@project`.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-rename-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read",
            "pane-fds","fs-read","fs-list","run-process"]
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
# sqlite3, not python3: the system python3 needs an Xcode SDK that is
# not always there.
q() { db=$(find "$XDG_DATA_HOME" -name store.db | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }' | head -1)
	[ -n "$FORM" ] || fail "picker did not open"
}
# The project headers, top to bottom, on one line (the cursor glyph
# replaces the bullet on the header the cursor is on).
headers() { screen | sed -n "s/^ *[▪▸] \([^ ●][^ ]*\).*/\1/p" | tr "\n" " "; }
narrow() { keys /; keys '^' "$@"; keys Escape; }
widen() { keys /; keys C-u; keys Escape; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Four agents: alpha (tagged tmux2), beta:0 (nothing: the session name),
# beta:1 (the window option glm), gamma (nothing: the session name).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 -c "$HOME" \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
$TMUX new-session -d -s beta -x 200 -y 50 -c "$HOME" \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session beta"
$TMUX new-window -d -t beta -c "$HOME" "sh -c 'AI_AGENT=claude exec sleep 600'" ||
	fail "new-window"
$TMUX new-session -d -s gamma -x 200 -y 50 -c "$HOME" \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session gamma"
sleep 0.5
PA=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
PB=$($TMUX list-panes -t beta:0 -F '#{pane_id}' | head -1)
PC=$($TMUX list-panes -t beta:1 -F '#{pane_id}' | head -1)
[ -n "$PA" ] && [ -n "$PB" ] && [ -n "$PC" ] || fail "no panes"

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list -c run-process \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0
[ "$(q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL")" = 4 ] ||
	fail "expected 4 live rows, got $(q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL")"
$TMUX plugin-command -t "$PC" agents "project -w glm"
$TMUX plugin-command -t "$PA" agents "project tmux2"
sleep 0.5

# --- the project prompt completes -----------------------------------------

open_picker
case "$(headers)" in
*tmux2*) ;; *) fail "no tmux2 header: $(headers)" ;;
esac
# On beta's row (the only one in its group): p offers every project.
narrow b e t a
keys g g
keys p
screen | grep -q ' glm  1' || fail "the dropdown does not offer glm"
screen | grep -q ' tmux2  1' || fail "the dropdown does not offer tmux2"
# "gl" leaves glm; Enter takes it into the box, Enter tags the row.
keys g l
screen | grep -q ' tmux2  1' && fail "gl kept tmux2 in the dropdown"
keys Enter
screen | grep -q 'project *glm' || fail "Enter did not take glm into the box"
keys Enter
sleep 0.8
screen | grep -q 'tagged glm' || fail "the second Enter did not tag the row"
[ "$(q "SELECT project FROM agents WHERE pane = ${PB#%}")" = glm ] ||
	fail "beta's agent is not tagged glm: $(q "SELECT project FROM agents WHERE pane = ${PB#%}")"
widen

# --- R on the header renames the project ----------------------------------

# glm holds beta:0 (tag) and beta:1 (window option) now.
narrow g l m
keys g g
keys h
keys R
screen | grep -q 'project name *glm' || fail "R on the header did not open the rename prompt"
keys C-u
keys s p e e d
keys Enter
sleep 1.0
# The ^glm in the box followed the rename, the status says what moved,
# and the cursor is on the new header.
screen | grep -q '\^speed' || fail "the filter token did not follow the rename"
screen | grep -q 'glm → speed (2 rows)' || fail "no rename status"
screen | grep -q '^ *▸ speed' || fail "the cursor is not on the speed header"
widen
case "$(headers)" in
*speed*) ;; *) fail "no speed header after the rename: $(headers)" ;;
esac
case "$(headers)" in
*glm*) fail "the glm header outlived the rename: $(headers)" ;; *) ;;
esac
[ "$(q "SELECT project FROM agents WHERE pane = ${PB#%}")" = speed ] ||
	fail "the tag was not renamed: $(q "SELECT project FROM agents WHERE pane = ${PB#%}")"
[ "$($TMUX display-message -p -t "$PC" '#{@project}')" = speed ] ||
	fail "the window option was not renamed: $($TMUX display-message -p -t "$PC" '#{@project}')"

# A project named by its session: the rename puts @project on the session.
narrow g a m m a
keys g g
keys h
keys R
keys C-u
keys i n f r a
keys Enter
sleep 1.0
widen
[ "$($TMUX display-message -p -t gamma '#{@project}')" = infra ] ||
	fail "the session option was not set: $($TMUX display-message -p -t gamma '#{@project}')"
case "$(headers)" in
*infra*) ;; *) fail "no infra header: $(headers)" ;;
esac

# --- the new-agent form carries the project --------------------------------

narrow s p e e d
keys g g
keys n
sleep 1.0
NEWFORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }' | grep -v "^$FORM\$" | head -1)
[ -n "$NEWFORM" ] || fail "the new-agent form did not open"
PICKER=$FORM; FORM=$NEWFORM
screen | grep -q 'project *speed' || fail "the form's project field is not speed"
# Down to the command field, a command that stays alive, Enter.
keys Down
screen | grep -q 'command' || fail "no command field"
keys C-u
$TMUX send-keys -t "$FORM" -l -- 'sleep 300'; sleep 0.5
keys Enter
# The sandbox may not see the folder: the form then asks for a second
# Enter to create it (an existing folder is left as it is).
screen | grep -q 'does not exist' && keys Enter
sleep 1.5
[ "$($TMUX display-message -p -t 'beta:' '#{@project}')" = speed ] ||
	fail "the new window did not get @project: $($TMUX display-message -p -t 'beta:' '#{@project}')"
[ "$($TMUX list-windows -t beta | wc -l | tr -d ' ')" = 3 ] ||
	fail "no third window in beta"

cleanup
exit 0
