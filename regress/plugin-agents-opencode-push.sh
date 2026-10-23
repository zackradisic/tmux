#!/bin/sh
# Push into opencode: a mailbox message to an opencode agent is posted to
# the service that runs its session, as one steered prompt, and marked
# read. opencode has no inbox socket; its service has an HTTP API and a
# registration file naming the url and password. A stub service stands
# in for it here and logs what it receives: the path names the session,
# the credentials are the registration's, the delivery is `steer`, and
# the prefix names the row's pane literally (opencode runs its tools in
# the service, where $TMUX_PANE is not the agent's pane). With the
# service gone the message stays unread for `inbox`. `f` on the row forks
# the session through the same service (`POST .../fork`) and the new
# window resumes the copy.
#
# Needs the wasm examples built, and sqlite3, python3 and curl on PATH:
#   cargo build -p agents -p mailbox --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen
SID=ses_test0002abcdefghij
PW='s3cret-pw_Xyz'

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lagents-opencode-push-test"
BUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release
AGSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml
MBSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/mailbox/mailbox.toml

for t in sqlite3 python3 curl; do
	command -v $t >/dev/null 2>&1 || { echo "SKIP: no $t" >&2; exit 0; }
done
[ -f "$BUILT/agents.wasm" ] || { echo "FAIL: agents.wasm not built" >&2; exit 1; }
[ -f "$BUILT/mailbox.wasm" ] || { echo "FAIL: mailbox.wasm not built" >&2; exit 1; }

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
. ./fake-bin.inc
FAKE=$(mktemp -d); fake_bin "$FAKE/opencode"
WORK="$HOME/work"; mkdir -p "$WORK"
REAL=$(cd "$WORK" && pwd -P)
OCDB="$HOME/.local/share/opencode/opencode.db"; mkdir -p "$(dirname "$OCDB")"
REG="$HOME/.local/state/opencode/service.json"; mkdir -p "$(dirname "$REG")"
LOG="$HOME/received.log"; : > "$LOG"

DEPLOY=$(mktemp -d)
cp "$BUILT/agents.wasm" "$DEPLOY/agents.wasm"; cp "$AGSIDE" "$DEPLOY/agents.toml"
cp "$BUILT/mailbox.wasm" "$DEPLOY/mailbox.wasm"; cp "$MBSIDE" "$DEPLOY/mailbox.toml"

# The stub service: logs every POST as one JSON line and answers 200.
cat > "$HOME/stub.py" <<'PY'
import http.server, json, sys
port_file, log = sys.argv[1], sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(n).decode("utf-8", "replace")
        with open(log, "a") as f:
            f.write(json.dumps({"path": self.path, "auth": self.headers.get("authorization"), "body": body}) + "\n")
        if self.path.endswith("/fork"):
            out = json.dumps({"data": {"id": "ses_forkedcopy0001", "title": "Push session (fork #1)"}}).encode()
        else:
            out = json.dumps({"data": {"id": "msg_stub", "delivery": "steer"}}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)
    def log_message(self, *a):
        pass
srv = http.server.HTTPServer(("127.0.0.1", 0), H)
with open(port_file, "w") as f:
    f.write(str(srv.server_address[1]))
srv.serve_forever()
PY

trap 'kill $CTL $STUB 2>/dev/null; $TMUX kill-server 2>/dev/null; rm -rf "$HOME" "$FAKE" "$DEPLOY"' EXIT
cleanup() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	[ -n "$STUB" ] && kill $STUB 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$FAKE" "$DEPLOY"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- received:" >&2; cat "$LOG" >&2
	echo "--- mailbox store:" >&2
	mb=$(find "$XDG_DATA_HOME" -name store.db -path '*mailbox*' | head -1)
	[ -n "$mb" ] && sqlite3 "$mb" "SELECT id, box, sender, substr(body, 1, 60), read FROM messages" >&2
	echo "--- messages:" >&2; $TMUX show-messages 2>/dev/null | grep -v 'command:' | tail -5 >&2
	cleanup; exit 1
}
q() { db=$(find "$XDG_DATA_HOME" -name store.db -path '*agents*' | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
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
received() { grep -q "$1" "$LOG"; }
mb() { db=$(find "$XDG_DATA_HOME" -name store.db -path '*mailbox*' | head -1); [ -n "$db" ] && sqlite3 "$db" "$1"; }
read_is() { [ "$(mb "SELECT read FROM messages WHERE body = '$1'")" = "$2" ]; }

# The stub listens on a free port; the registration names it, as
# opencode's own would (url, pid, password, mode 0600).
python3 "$HOME/stub.py" "$HOME/port" "$LOG" &
STUB=$!
wait_for 10 test -s "$HOME/port" || fail "stub did not start"
PORT=$(cat "$HOME/port")
printf '{"id":"stub","version":"2.1.2","url":"http://127.0.0.1:%s","pid":%s,"password":"%s"}' "$PORT" "$STUB" "$PW" > "$REG"
chmod 600 "$REG"
AUTH="Basic $(printf 'opencode:%s' "$PW" | base64)"

oc "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, \
      slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT, version TEXT NOT NULL, \
      time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER); \
    CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, \
      seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);"

$TMUX kill-server 2>/dev/null
sleep 0.5
# The fake opencode first on the server's PATH: a fork's window runs
# `opencode -s <copy>` by name.
PATH="$FAKE:$PATH" $TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 -c "$WORK" \
    "$FAKE/opencode -c 'sleep 600; :'" || fail "new-session"
sleep 0.5
PANE=$($TMUX list-panes -t alpha -F '#{pane_id}' | head -1)
T0=$(now_ms)

$TMUX load-plugin -s server -c db -c service-serve -c service-call -c write-options -c display-message \
    "$DEPLOY/mailbox.wasm" || fail "load mailbox"
$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve -c service-call -c run-process \
    "$DEPLOY/agents.wasm" || fail "load agents"
sleep 1.5
wait_for 5 row_is "prov-opencode-${PANE#%}" ||
    fail "pane not detected as provisional opencode (got '$(q "SELECT id FROM agents WHERE pane=${PANE#%}")')"

# The session the pane runs, with one finished turn, so the row binds.
T1=$((T0 + 1500))
oc "INSERT INTO session_v2 VALUES ('$SID','proj1',NULL,'brisk-otter','$REAL','Push session','2.1.2',$T1,$((T1 + 40)),NULL); \
    INSERT INTO session_message VALUES ('m1','$SID','user',1,$((T1 + 10)),$((T1 + 10)), \
      '{\"time\":{\"created\":$((T1 + 10))},\"text\":\"hello there\",\"files\":[],\"agents\":[]}'); \
    INSERT INTO session_message VALUES ('m2','$SID','idle',2,$((T1 + 40)),$((T1 + 40)),'{\"time\":{\"created\":$((T1 + 40))},\"outcome\":\"succeeded\"}');"
open_picker
wait_for 10 row_is "opencode:$SID" ||
    fail "row did not bind to opencode:$SID (got '$(q "SELECT id FROM agents WHERE pane=${PANE#%}")')"
close_picker

# A message to the opencode agent: posted to its session as a steer,
# with the registration's credentials, the sender line and the row's
# pane in the prefix, and marked read.
$TMUX plugin-command agents "message opencode:$SID hello opencode push" || fail "message"
wait_for 10 received 'hello opencode push' || fail "push did not reach the service"
grep -q "\"path\": \"/api/session/$SID/prompt\"" "$LOG" || fail "wrong path"
grep -q "\"auth\": \"$AUTH\"" "$LOG" || fail "wrong credentials (want $AUTH)"
grep -q '\\"delivery\\": *\\"steer\\"\|\\"delivery\\":\\"steer\\"' "$LOG" || fail "not a steer"
grep -q 'via the tmux2 mailbox' "$LOG" || fail "no sender line"
grep -q "plugin-command -t $PANE agents" "$LOG" || fail "the prefix does not name the pane $PANE"
grep -q "skill -t $PANE show mailbox" "$LOG" || fail "the guide does not name the pane"
grep -q 'tmux2-mailbox' "$LOG" || fail "no source in the metadata"
wait_for 5 read_is 'hello opencode push' 1 || fail "pushed message not marked read"

# f on the row: the form as a fork (fork yes, the row's session id in
# resume, a name of its own); Enter asks the service for a copy and the
# new window resumes that copy, not the original.
: > "$LOG"
open_picker
$TMUX send-keys -t "$FORM" f
i=0
while [ "$i" -lt 20 ]; do
	NEWF=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' | awk '/plugin-mode/ { print $1 }' | grep -v "^$FORM\$" | head -1)
	[ -n "$NEWF" ] && break
	sleep 0.3; i=$((i + 1))
done
[ -n "$NEWF" ] || fail "f did not open the form on the opencode row ($($TMUX capture-pane -M -p -t "$FORM" | grep -i 'nothing to fork\|no session' | head -1))"
sleep 0.5
fscreen() { $TMUX capture-pane -M -p -t "$NEWF" | sed '/^ *$/d'; }
fscreen | grep -q 'fork *yes' || fail "fork is not on: $(fscreen | grep fork)"
fscreen | grep -q "resume *$SID" || fail "the form does not carry the session id: $(fscreen | grep resume)"
fscreen | grep -q 'name .* fork' || fail "the fork did not get a name of its own: $(fscreen | grep name)"
$TMUX send-keys -t "$NEWF" Enter
wait_for 10 received "/api/session/$SID/fork" || fail "Enter did not ask the service for a fork"
grep -q "\"auth\": \"$AUTH\"" "$LOG" || fail "the fork went without the registration's credentials"
started() { $TMUX list-panes -a -F '#{pane_start_command}' | grep -q 'opencode -s ses_forkedcopy0001'; }
wait_for 10 started || fail "no window resumes the copy: $($TMUX list-panes -a -F '#{pane_id} #{pane_start_command}')"
$TMUX list-panes -a -F '#{pane_start_command}' | grep -q "opencode -s $SID" && fail "a window resumed the original instead of the copy"
FORM=

# With the service gone the message is stored and left unread.
kill $STUB; wait $STUB 2>/dev/null; STUB=
: > "$LOG"
$TMUX plugin-command agents "message opencode:$SID while down" || fail "message 2"
sleep 2
[ ! -s "$LOG" ] || fail "a dead service received something"
read_is 'while down' 0 || fail "message not kept unread"

cleanup
exit 0
