#!/bin/sh
# opencode keeps every session in one SQLite database and runs its plugins
# in a shared background service, so no shim can name the pane. The roster
# reads the database itself: a pane running opencode is matched to the
# session created in its directory after the row appeared, the row moves
# to `opencode:<sid>`, its status follows the last message (`idle` closes
# a turn), and its conversation - prompts, replies, condensed tool calls -
# is read into the store and shown in the preview, live and after the
# pane is gone. A fake database stands in for opencode's.
#
# Needs the wasm example built, and sqlite3 on PATH:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
SID=ses_test0001abcdefghij

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-opencode-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

command -v sqlite3 >/dev/null 2>&1 || { echo "SKIP: no sqlite3" >&2; exit 0; }

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
. ./fake-bin.inc
FAKE=$(mktemp -d); fake_bin "$FAKE/opencode"
WORK="$HOME/work"; mkdir -p "$WORK"
# opencode records the real path of its directory.
REAL=$(cd "$WORK" && pwd -P)
OCDB="$HOME/.local/share/opencode/opencode.db"; mkdir -p "$(dirname "$OCDB")"

DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<TOML
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","fs-read","fs-list","run-process"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE","_"]
TOML

trap 'kill $CTL 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$FAKE" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$FAKE" "$DEPLOY"
}
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
q() { db=$(find "$XDG_DATA_HOME" -name store.db | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
oc() { sqlite3 "$OCDB" "$1"; }
now_ms() { date +%s000; }
screen() { [ -n "$FORM" ] && $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
	    $TMUX -C attach >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
	[ -n "$FORM" ] || fail "picker did not open"
	i=0
	while [ "$i" -lt 15 ]; do
		$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸\|(no agents)' && return 0
		sleep 0.4; i=$((i + 1))
	done
	fail "picker did not render"
}
close_picker() { $TMUX send-keys -t "$FORM" q; sleep 0.5; FORM=; }
# wait_for <tries> <command>: poll until the command succeeds.
wait_for() {
	n=$1; shift
	i=0
	while [ "$i" -lt "$n" ]; do
		"$@" && return 0
		sleep 1; i=$((i + 1))
	done
	return 1
}
row_is() { [ "$(q "SELECT id FROM agents WHERE pane=${PANE#%}")" = "$1" ]; }
status_is() { [ "$(q "SELECT status FROM agents WHERE id='opencode:$SID'")" = "$1" ]; }
turns_are() { [ "$(q "SELECT count(*) FROM turns WHERE id='opencode:$SID'")" = "$1" ]; }
screen_has() { screen | grep -q "$1"; }

[ -f "$WASM" ] || fail "agents.wasm not built"

# The slice of opencode's schema the roster reads.
oc "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, \
      slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT, version TEXT NOT NULL, \
      time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER); \
    CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, \
      seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 -c "$WORK" \
    "$FAKE/opencode -c 'sleep 600; :'" || fail "new-session"
sleep 0.5
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
T0=$(now_ms)

$TMUX load-plugin -s server -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list -c run-process "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

# Detected as opencode, provisional: the database has no session yet.
wait_for 5 row_is "prov-opencode-${PANE#%}" ||
    fail "pane not detected as provisional opencode (got '$(q "SELECT id FROM agents WHERE pane=${PANE#%}")')"

# A session created after the row, in the pane's directory, with a turn:
# a prompt, a reply with two tool calls and a line of text, then idle.
T1=$((T0 + 1500))
# (Times stay in the past: activity is dated by them, and a later prompt
# at the real clock must read as newer.)
oc "INSERT INTO session_v2 VALUES ('$SID','proj1',NULL,'brisk-otter','$REAL','Probe session','2.1.2',$T1,$((T1 + 40)),NULL); \
    INSERT INTO session_message VALUES ('m1','$SID','user',1,$((T1 + 10)),$((T1 + 10)), \
      '{\"time\":{\"created\":$((T1 + 10))},\"text\":\"hello there please make a file\",\"files\":[],\"agents\":[]}'); \
    INSERT INTO session_message VALUES ('m2','$SID','assistant',2,$((T1 + 20)),$((T1 + 30)), \
      '{\"time\":{\"created\":$((T1 + 20)),\"completed\":$((T1 + 30))},\"agent\":\"build\",\"model\":{\"id\":\"flint-40b-free\",\"providerID\":\"opencode\"},\"content\":[ \
        {\"type\":\"reasoning\",\"text\":\"thinking hard\"}, \
        {\"type\":\"tool\",\"id\":\"c1\",\"name\":\"write\",\"state\":{\"status\":\"completed\",\"input\":{\"content\":\"hi there\\\\n\",\"path\":\"$REAL/hello.txt\"},\"output\":\"Created file\"},\"time\":{\"created\":$((T1 + 100))}}, \
        {\"type\":\"tool\",\"id\":\"c2\",\"name\":\"shell\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"wc -c hello.txt\",\"workdir\":\"$REAL\"},\"output\":\"9\"},\"time\":{\"created\":$((T1 + 200))}}, \
        {\"type\":\"text\",\"text\":\"All done, the file is written.\"}]}'); \
    INSERT INTO session_message VALUES ('m3','$SID','idle',3,$((T1 + 40)),$((T1 + 40)),'{\"time\":{\"created\":$((T1 + 40))},\"outcome\":\"succeeded\"}');"
[ "$(oc "SELECT count(*) FROM session_message")" = 3 ] || fail "fake database not written"

# A render binds the row to the session and reads the conversation.
open_picker
wait_for 10 row_is "opencode:$SID" ||
    fail "row did not bind to opencode:$SID (got '$(q "SELECT id FROM agents WHERE pane=${PANE#%}")')"
wait_for 5 status_is waiting || fail "status after idle is not waiting (got '$(q "SELECT status FROM agents WHERE id='opencode:$SID'")')"
wait_for 10 turns_are 4 || fail "expected 4 turns (got '$(q "SELECT count(*) FROM turns WHERE id='opencode:$SID'")'): $(q "SELECT kind||' '||text FROM turns WHERE id='opencode:$SID'")"
[ "$(q "SELECT text FROM turns WHERE id='opencode:$SID' AND kind='tool' ORDER BY seq")" = "Write work/hello.txt +1
Shell wc -c hello.txt" ] || fail "tool lines: $(q "SELECT text FROM turns WHERE id='opencode:$SID' AND kind='tool' ORDER BY seq")"
[ "$(q "SELECT model FROM agents WHERE id='opencode:$SID'")" = "flint-40b-free" ] || fail "model not kept"
[ "$(q "SELECT transcript_cursor FROM agents WHERE id='opencode:$SID'")" = 3 ] || fail "cursor is not the last seq"

# The conversation in the preview (Tab), live.
$TMUX send-keys -t "$FORM" Tab
wait_for 5 screen_has "hello there please" || fail "prompt not in the preview"
screen_has "Write work/hello.txt +1" || fail "tool line not in the preview"
screen_has "All done, the file" || fail "reply not in the preview"
close_picker

# A new prompt moves the row to working; its idle moves it back, and the
# turn is read from the cursor on - nothing twice.
T2=$(now_ms)
oc "INSERT INTO session_message VALUES ('m4','$SID','user',4,$T2,$T2,'{\"time\":{\"created\":$T2},\"text\":\"and now a second prompt\"}'); \
    UPDATE session_v2 SET time_updated=$T2 WHERE id='$SID';"
open_picker
wait_for 12 status_is working || fail "a new prompt did not make the row working (got '$(q "SELECT status FROM agents WHERE id='opencode:$SID'")')"
wait_for 10 turns_are 5 || fail "second prompt not read (turns: $(q "SELECT count(*) FROM turns WHERE id='opencode:$SID'"))"
# The reply streams: its message is updated in place, tool parts first,
# text last. Until `time.completed` is on it nothing of it is read, and
# the cursor waits before it.
T3=$(now_ms)
oc "INSERT INTO session_message VALUES ('m5','$SID','assistant',5,$T3,$T3, \
      '{\"time\":{\"created\":$T3},\"model\":{\"id\":\"flint-40b-free\"},\"content\":[ \
        {\"type\":\"tool\",\"id\":\"c3\",\"name\":\"read\",\"state\":{\"status\":\"completed\",\"input\":{\"filePath\":\"$REAL/hello.txt\"}},\"time\":{\"created\":$T3}}]}'); \
    UPDATE session_v2 SET time_updated=$T3 WHERE id='$SID';"
sleep 7
turns_are 5 || fail "a streaming reply was read early (turns: $(q "SELECT count(*) FROM turns WHERE id='opencode:$SID'"))"
[ "$(q "SELECT transcript_cursor FROM agents WHERE id='opencode:$SID'")" = 4 ] || fail "the cursor passed a streaming reply"
T4=$(now_ms)
oc "UPDATE session_message SET time_updated=$T4, data=json_set(data, '$.time.completed', $T4, \
      '$.content[1]', json('{\"type\":\"text\",\"text\":\"Read it back, all good.\"}')) WHERE id='m5'; \
    INSERT INTO session_message VALUES ('m6','$SID','idle',6,$T4,$T4,'{\"time\":{\"created\":$T4},\"outcome\":\"succeeded\"}'); \
    UPDATE session_v2 SET time_updated=$T4 WHERE id='$SID';"
wait_for 12 status_is waiting || fail "idle did not make the row waiting"
wait_for 10 turns_are 7 || fail "the completed reply was not read (turns: $(q "SELECT count(*) FROM turns WHERE id='opencode:$SID'"))"
[ "$(q "SELECT text FROM turns WHERE id='opencode:$SID' AND seq=5")" = "Read it back, all good." ] || fail "reply text: $(q "SELECT seq||' '||kind||' '||text FROM turns WHERE id='opencode:$SID' AND seq>=5")"
[ "$(q "SELECT text FROM turns WHERE id='opencode:$SID' AND seq=6")" = "Read work/hello.txt" ] || fail "read line: $(q "SELECT text FROM turns WHERE id='opencode:$SID' AND seq=6")"
[ "$(q "SELECT transcript_cursor FROM agents WHERE id='opencode:$SID'")" = 6 ] || fail "cursor after the turn"
close_picker

# The pane dies: the row ends, and the ended row still shows its
# conversation in the preview. (Another session first: the server would
# leave with its last pane.)
$TMUX -f/dev/null new-session -d -s beta -x 200 -y 50 || fail "new-session beta"
$TMUX kill-pane -t "$PANE"
row_ended() { [ -n "$(q "SELECT ended_ms FROM agents WHERE id='opencode:$SID' AND ended_ms IS NOT NULL")" ]; }
wait_for 10 row_ended || fail "row did not end with its pane"
open_picker
# Ended rows are history (`.`); the ended row's preview is its conversation.
$TMUX send-keys -t "$FORM" .
wait_for 5 screen_has "and now a second prompt" || fail "ended row's conversation not in the preview"
screen_has "Shell wc -c hello.txt" || fail "ended row's tool line not in the preview"

cleanup
exit 0
