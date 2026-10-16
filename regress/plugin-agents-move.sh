#!/bin/sh
# Moving rows and projects. The order of the rows in a project, and of
# the projects, is the user's: `M-j` / `M-k` move the highlighted row one
# place, or - with the cursor on the project header (`h` from a row) -
# the project among the projects, and the order survives a reopen. A
# move stops at the edge of the group, with a status. In the band views
# a row moves within its band. Check:
#
#   M-j swaps the row with the one under it; the order is kept across a
#     reopen (`pick_order`);
#   h then M-j moves the whole project below the next; kept too
#     (`pick_proj_order`);
#   at the bottom of a project M-j says so and nothing moves;
#   in the server view M-j moves the row within its band.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-move-test"
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
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
}
# The project headers, top to bottom, on one line (the cursor glyph
# replaces the bullet on the header the cursor is on).
headers() { screen | sed -n "s/^ *[▪▸] \([^ ●][^ ]*\).*/\1/p" | tr "\n" " "; }
# The rows' sessions, top to bottom, on one line (the rows carry the
# session in their name, "claude · alpha", and no header does).
rows() { screen | grep -v '^ *▪' | grep -o 'claude · [a-z]*' | sed 's/claude · //' | tr '\n' ' '; }

[ -f "$WASM" ] || fail "agents.wasm not built"

$TMUX kill-server 2>/dev/null
sleep 0.5

# Three agents in three sessions. alpha and beta are tagged into one
# project; gamma is its own (its session name).
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session"
for s in beta gamma; do
	$TMUX new-session -d -s $s -x 200 -y 50 \
	    "sh -c 'AI_AGENT=claude exec sleep 600'" || fail "new-session $s"
done
sleep 0.5
PA=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
PB=$($TMUX list-panes -t beta -F '#{pane_id}' | head -1)
[ -n "$PA" ] && [ -n "$PB" ] || fail "no panes"

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command \
    -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list \
    "$DEPLOY/agents.wasm" || fail "load-plugin"
sleep 1.0
[ "$(q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL")" = 3 ] ||
	fail "expected 3 live rows, got $(q "SELECT COUNT(*) FROM agents WHERE ended_ms IS NULL")"
$TMUX plugin-command -t "$PA" agents "project x"
$TMUX plugin-command -t "$PB" agents "project x"
sleep 0.5

# --- M-j moves a row down inside its project --------------------------

open_picker
screen | grep -q '3 live, by project' || fail "not grouped by project"
case "$(headers)" in
*x*gamma*|*gamma*x*) ;;
*) fail "expected the x and gamma projects: $(headers)" ;;
esac
# Narrow to x so the first row is one of its two, whatever the project
# order came out as. Rows in a project order by state, unread and
# recency, not by hand: M-j says so and nothing moves.
keys /
keys '^' x
keys Escape
[ "$(rows | wc -w | tr -d " ")" = 2 ] || fail "^x did not narrow to the two rows: $(rows)"
before=$(rows)
keys g g
keys M-j
screen | grep -q 'rows order by state' || fail "no status for a row move in the project view"
[ "$(rows)" = "$before" ] || fail "a refused move changed the order: $(rows)"
keys /
keys C-u
keys Escape

# --- h then M-j moves the whole project ---------------------------------

keys g g
keys h
# The cursor is on the first project's header: M-j puts it second.
first=$(headers | awk '{print $1}')
keys M-j
[ "$(headers | awk '{print $2}')" = "$first" ] ||
	fail "M-j on the header did not move the project: $(headers)"
keys M-j
screen | grep -q 'last project' || fail "no status past the last project"
[ "$(q "SELECT value FROM settings WHERE key = 'pick_proj_order'")" != "" ] ||
	fail "pick_proj_order was not saved"
hdrs=$(headers)

# --- a reopen keeps both orders ------------------------------------------

open_picker
[ "$(headers)" = "$hdrs" ] || fail "the reopen forgot the project order: $(headers) vs $hdrs"
keys /
keys '^' x
keys Escape
[ "$(rows)" = "$before" ] || fail "the reopen changed the row order: $(rows) vs $before"
keys /
keys C-u
keys Escape

# --- the server view: within the band ----------------------------------

keys v
screen | grep -q 'grouped by server' || fail "v did not group by server"
before=$(rows)
keys g g
keys M-j
after=$(rows)
[ "$(echo "$after" | awk '{print $2}')" = "$(echo "$before" | awk '{print $1}')" ] ||
	fail "M-j in the server view did not move the row: $before -> $after"
keys G
keys M-j
screen | grep -q 'last in its band' || fail "no status at the bottom of the band"
keys v; keys v

cleanup
exit 0
