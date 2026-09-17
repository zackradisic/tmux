#!/bin/sh
# A linked remote session survives resurrect as a link, not as a copy.
#
# The remote server holds the real windows, panes and processes, so the
# snapshot stores only the link (host and remote session) and restore
# runs remote-attach; the sync brings the rest back live. Asserts the
# shadow contributes no panes and no pane_blob rows to the snapshot,
# that it comes back connected and mirroring B, and that an unreachable
# host degrades to a disconnected session instead of failing the
# restore. Needs the wasm example built:
#   cargo build -p resurrect --target wasm32-unknown-unknown --release

# Own data dir: never touch the user's real saves. Set before the
# prelude so the servers it starts inherit it.
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DATA=$XDG_DATA_HOME/tmux/plugins/resurrect
DB=$DATA/store.db

. ./remote-common.inc

WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/resurrect.wasm
CAPS="-c capture-pane -c run-command -c fs-read -c fs-write -c db"
SSH_CMD="$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}"

cleanup()
{
	$TMUX kill-server 2>/dev/null
	$TMUX2 kill-server 2>/dev/null
	rm -rf "$TMP" "$XDG_DATA_HOME"
}

[ -f "$WASM" ] || fail "resurrect.wasm not built"
command -v python3 >/dev/null || fail "python3 is needed to read store.db"

# One scalar from the store (empty for NULL / no row). The store is a
# WAL database, and a mode=ro connection cannot create the -shm file it
# needs to read one, so open it read-write - $DB is a throwaway copy
# under our own XDG_DATA_HOME, never the user's real store.
q()
{
	python3 -c "import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
r = c.execute(sys.argv[2]).fetchone()
print('' if r is None or r[0] is None else r[0])" "$DB" "$1"
}

# A field of the newest snapshot's session tree, by session name.
tree()
{
	python3 -c "import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
t = json.loads(c.execute('SELECT meta FROM snapshot ORDER BY id DESC').fetchone()[0])
s = next(s for s in t['sessions'] if s['name'] == sys.argv[2])
print(eval(sys.argv[3], {'s': s}))" "$DB" "$1" "$2"
}

# --- the scene: B has "work" with three windows, A links to it -------------
link_setup
$TMUX2 new-window -t work -n two || fail "new-window two on B"
$TMUX2 new-window -t work -n three || fail "new-window three on B"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{session_windows}')\" = 3 ]" || fail "shadow did not mirror B"

# The new format the plugin reads the link back out of.
rsess=$($TMUX display-message -p -t fakehost/work '#{remote_session}')
[ "$rsess" = work ] || fail "#{remote_session} is '$rsess', not work"

# --- save + kill -----------------------------------------------------------
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin (save side)"
sleep 0.5
$TMUX plugin-command resurrect kill

for _ in 1 2 3 4 5 6 7 8 9 10; do
	$TMUX ls >/dev/null 2>&1 || break
	sleep 0.5
done
$TMUX ls >/dev/null 2>&1 && fail "server A still alive after kill"
$TMUX2 has-session -t work || fail "server B died with A"
[ -f "$DB" ] || fail "store.db was not written"

# Both sessions are in the tree, but only the local one carries panes:
# the shadow's belong to B and are not ours to save.
[ "$(q 'SELECT sessions FROM snapshot')" = 2 ] ||
    fail "snapshot counts $(q 'SELECT sessions FROM snapshot') sessions, not 2"
[ "$(q 'SELECT panes FROM snapshot')" = 1 ] ||
    fail "snapshot counts $(q 'SELECT panes FROM snapshot') panes, not 1"
[ "$(q 'SELECT count(*) FROM pane_blob')" = 1 ] ||
    fail "expected one pane_blob row, got $(q 'SELECT count(*) FROM pane_blob')"
[ "$(q 'SELECT version FROM snapshot')" = 4 ] ||
    fail "snapshot version is $(q 'SELECT version FROM snapshot'), not 4"

[ "$(tree fakehost/work "s['remote']['host']")" = fakehost ] ||
    fail "saved link lost its host"
[ "$(tree fakehost/work "s['remote']['session']")" = work ] ||
    fail "saved link lost its remote session"
[ "$(tree fakehost/work "len(s['windows'])")" = 0 ] ||
    fail "the shadow session saved windows of its own"
[ "$(tree local "'remote' in s")" = False ] ||
    fail "a local session was saved as a link"

# --- restore ---------------------------------------------------------------
# remote-ssh-command lives in the config in real use; a fresh server
# started with -f/dev/null needs it set again before the link can dial.
$TMUX -f/dev/null new-session -d -s bootstrap "sleep 600" ||
    fail "bootstrap session"
$TMUX set -s remote-ssh-command "$SSH_CMD" || fail "set remote-ssh-command"
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin (restore side)"
sleep 0.5
$TMUX plugin-command resurrect restore

wait_for 10 "$TMUX has-session -t fakehost/work" ||
    fail "the shadow session did not come back"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_connected}')\" = 1 ]" || fail "restored link never connected"

# It is a live link, not a rebuilt copy: B's windows are there, with
# remote ids, and a window made on B afterwards shows up.
names=$($TMUX list-windows -t fakehost/work -F '#{window_name}' | tr '\n' ' ')
[ "$names" = "bash two three " ] || [ "$names" = "sh two three " ] ||
    case "$names" in
    *"two three "*) ;;
    *) fail "restored windows are '$names', not B's" ;;
    esac
ids=$($TMUX list-windows -t fakehost/work -F '#{window_remote_id}' | tr '\n' ' ')
case "$ids" in
*@*) ;;
*) fail "restored windows have no remote ids ('$ids')" ;;
esac
$TMUX2 new-window -t work -n four || fail "new-window four on B"
wait_for 6 "$TMUX list-windows -t fakehost/work -F '#{window_name}' | grep -qx four" ||
    fail "the restored link does not track B"

# The local session came back the ordinary way.
$TMUX has-session -t local || fail "the local session did not come back"

# --- an unreachable host ---------------------------------------------------
# The link must degrade to a disconnected session carrying the reason,
# and must not take the rest of the restore down with it.
$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s bootstrap "sleep 600" ||
    fail "bootstrap session (unreachable pass)"
$TMUX set -s remote-ssh-command "sh -c 'echo no route to host >&2; exit 255'" ||
    fail "set failing remote-ssh-command"
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin (unreachable pass)"
sleep 0.5
$TMUX plugin-command resurrect restore

wait_for 10 "$TMUX has-session -t local" ||
    fail "a dead remote stopped the local session restoring"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work \
    '#{remote_state}')\" = disconnected ]" ||
    fail "an unreachable link is not reported disconnected"
err=$($TMUX display-message -p -t fakehost/work '#{remote_error}')
case "$err" in
*"no route to host"*) ;;
*) fail "the disconnected link lost its reason (got '$err')" ;;
esac

$TMUX kill-server 2>/dev/null
$TMUX2 kill-server 2>/dev/null
echo OK
exit 0
