#!/bin/sh
# The cron plugin, end to end on its SQLite store: add jobs, watch a 2s
# job tick, kill the server mid-run, come back and check the catch-up
# policies (once/each/skip) and the interrupted row, retries with
# backoff, restart-server, rm/disable, and run retention. Needs the wasm
# example built:
#   cargo build -p cron --target wasm32-unknown-unknown --release
#
# Every wait is a bounded poll. DB assertions go through python's sqlite3
# module (there is no sqlite3 CLI on every box).

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lcron-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/cron.wasm
# Own data dir: the Makefile runs env -i, so HOME is unset and the plugin
# host needs XDG_DATA_HOME.
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DB=$XDG_DATA_HOME/tmux/plugins/cron/store.db
TICKS=$XDG_DATA_HOME/ticks
trap '$TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT

fail() {
	echo "FAIL: $*" >&2
	echo "--- messages:" >&2
	msgs >&2
	echo "--- runs:" >&2
	python3 -c "import sqlite3; c=sqlite3.connect('file:$DB?mode=ro', uri=True)
for r in c.execute('select id,job_id,attempt,reason,state,exit_code,error from runs'): print(r)" >&2 2>/dev/null
	$TMUX kill-server 2>/dev/null
	exit 1
}

# One scalar from the store (empty for NULL / no row).
q() {
	python3 -c "import sqlite3, sys
c = sqlite3.connect('file:$DB?mode=ro', uri=True)
r = c.execute(sys.argv[1]).fetchone()
print('' if r is None or r[0] is None else r[0])" "$1"
}
msgs() { $TMUX show-messages 2>/dev/null | sed -n 's/.*plugin cron: //p'; }
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }
cron() { $TMUX plugin-command cron "$1"; }
# waitq <tries> <sql> <op> <value>: poll the scalar until the test holds.
waitq() {
	n=$1
	while [ "$n" -gt 0 ]; do
		v=$(q "$2")
		[ -n "$v" ] && [ "$v" "$3" "$4" ] && return 0
		sleep 0.5
		n=$((n - 1))
	done
	return 1
}
load() {
	$TMUX load-plugin -c db -c run-process -c run-command -c mode \
	    -o tz=utc -o max_sleep=2s -o retry_backoff=1s "$@" "$WASM" ||
	    fail "load-plugin"
	sleep 0.5
}

[ -f "$WASM" ] || fail "cron.wasm not built"
command -v python3 >/dev/null || fail "python3 needed"

$TMUX kill-server 2>/dev/null
sleep 0.5

# --- 1. load creates the store -------------------------------------------
$TMUX -f/dev/null new-session -d -s main -x 120 -y 40 "sleep 600" ||
    fail "new-session"
load
[ -f "$DB" ] || fail "store.db was not created"
[ "$(q 'pragma user_version')" = 1 ] || fail "user_version not 1"

# --- 2. add a 2s job -------------------------------------------------------
cron "add -n ticks every 2s -- shell echo tick >> $TICKS"
sleep 0.5
msgs | grep -q 'added #1 "ticks"' || fail "add did not reply"
[ "$(q 'select count(*) from jobs')" = 1 ] || fail "job row missing"
[ "$(q "select catchup from jobs where id=1")" = once ] ||
    fail "default catchup is not once"

# --- 3. it ticks -----------------------------------------------------------
waitq 20 "select count(*) from runs where job_id=1 and state='ok'" -ge 2 ||
    fail "ticks did not run twice"
[ "$(wc -l < "$TICKS")" -ge 2 ] || fail "tick output missing"
[ "$(q "select exit_code from runs where job_id=1 and state='ok' limit 1")" = 0 ] ||
    fail "exit code not recorded"
[ -n "$(q "select duration_ms from runs where job_id=1 and state='ok' limit 1")" ] ||
    fail "duration not recorded"

# --- 4. grammar errors and ls ---------------------------------------------
cron "add every 2s"
cron "add every 500ms -- shell x"
cron "add 60 * * * * -- shell x"
cron "add every 2s -- python x"
cron "frob"
cron "ls"
sleep 0.5
msgs | grep -q 'needs `-- shell' || fail "missing -- not reported"
msgs | grep -q 'floor' || fail "1s floor not reported"
msgs | grep -q '0-59' || fail "bad cron field not reported"
msgs | grep -q 'shell or tmux' || fail "bad action kind not reported"
msgs | grep -q 'unknown verb "frob"' || fail "unknown verb not reported"
msgs | grep -q '#1 "ticks" (every 2s)' || fail "ls did not list #1"
[ "$(q 'select count(*) from jobs')" = 1 ] || fail "a bad add created a job"

# --- 5. a long run to interrupt ---------------------------------------------
cron "add -n slow every 1h -- shell sleep 30"
cron "run slow"
waitq 10 "select count(*) from runs where job_id=2 and state='running'" -ge 1 ||
    fail "slow did not start"
cron "run slow"
sleep 0.5
msgs | grep -q 'already running' || fail "double run not refused"

# --- 6. catch-up jobs, then kill mid-run -----------------------------------
cron "add -n each --catchup each every 2s -- shell true"
cron "add -n skip --catchup skip every 2s -- shell true"
sleep 0.5
[ "$(q 'select count(*) from jobs')" = 4 ] || fail "expected 4 jobs"
t_kill=$(now_ms)
$TMUX kill-server
for _ in 1 2 3 4 5 6 7 8 9 10; do
	$TMUX ls >/dev/null 2>&1 || break
	sleep 0.5
done
$TMUX ls >/dev/null 2>&1 && fail "server still alive after kill"
sleep 6

# --- 7. come back ------------------------------------------------------------
$TMUX -f/dev/null new-session -d -s main -x 120 -y 40 "sleep 600" ||
    fail "new-session (2)"
load
sleep 1
[ "$(q "select state from runs where job_id=2 order by id desc limit 1")" = interrupted ] ||
    fail "slow's run was not marked interrupted"
[ "$(q "select error from runs where job_id=2 order by id desc limit 1")" = \
    "server stopped during run" ] || fail "interrupted row has no error text"
waitq 10 "select count(*) from runs where job_id=1 and reason='catchup'" -ge 1 ||
    fail "ticks got no catch-up run"
sleep 1.5
[ "$(q "select count(*) from runs where job_id=1 and reason='catchup'")" = 1 ] ||
    fail "ticks (catchup once) ran catch-up more than once"
next=$(q "select next_run_ms from jobs where id=1")
[ "$next" -gt $((t_kill + 6000)) ] || fail "ticks next_run_ms not advanced past the outage"
[ "$next" -le $(($(now_ms) + 2000)) ] || fail "ticks next_run_ms too far ahead"
waitq 20 "select count(*) from runs where job_id=3 and reason='catchup' and state='ok'" -ge 3 ||
    fail "each did not replay its missed occurrences"
n_each=$(q "select count(*) from runs where job_id=3 and reason='catchup'")
[ "$n_each" -le 5 ] || fail "each replayed too many occurrences ($n_each)"
[ "$(q "select count(*) from runs where job_id=4 and reason='catchup'")" = 0 ] ||
    fail "skip ran a catch-up"
[ "$(q "select next_run_ms from jobs where id=4")" -gt "$t_kill" ] ||
    fail "skip did not advance next_run_ms"

# --- 8. retries with backoff -------------------------------------------------
cron "add -n boom every 1h -- shell exit 3"
cron "run boom"
waitq 10 "select count(*) from runs where job_id=5 and state='failed' and attempt=1" -ge 1 ||
    fail "boom did not fail"
[ "$(q "select exit_code from runs where job_id=5 and attempt=1")" = 3 ] ||
    fail "boom exit code not 3"
waitq 6 "select count(*) from runs where job_id=5 and attempt=2" -ge 1 ||
    fail "no retry row queued"
waitq 20 "select count(*) from runs where job_id=5 and state='failed' and attempt=3" -ge 1 ||
    fail "attempts 2 and 3 did not run"
sleep 2.5
[ "$(q "select count(*) from runs where job_id=5 and attempt=4")" = 0 ] ||
    fail "a fourth attempt was made"
[ "$(q "select count(*) from runs where job_id=5 and state='pending'")" = 0 ] ||
    fail "a retry is still pending after the last attempt"
msgs | grep -q '"boom" failed (exit 3); retry' || fail "failure not announced"

# --- 9. restart-server ---------------------------------------------------------
cron "run slow"
waitq 10 "select count(*) from runs where job_id=2 and state='running'" -ge 1 ||
    fail "slow did not start (2)"
ok_before=$(q "select count(*) from runs where job_id=1 and state='ok'")
pid=$($TMUX display -p '#{pid}')
$TMUX restart-server || fail "restart-server"
sleep 3
[ "$($TMUX display -p '#{pid}')" = "$pid" ] || fail "pid changed across restart-server"
$TMUX show-plugins | grep -q '^cron' || fail "cron not loaded after restart-server"
[ "$(q "select state from runs where job_id=2 order by id desc limit 1")" = interrupted ] ||
    fail "slow's run not interrupted by restart-server"
waitq 20 "select count(*) from runs where job_id=1 and state='ok'" -gt "$ok_before" ||
    fail "ticks stopped after restart-server"

# --- 10. rm and disable ---------------------------------------------------------
cron "rm ticks"
sleep 0.5
msgs | grep -q 'removed #1 "ticks"' || fail "rm did not reply"
[ "$(q "select count(*) from runs where job_id=1")" = 0 ] || fail "rm left runs behind"
[ "$(q "select count(*) from jobs where id=1")" = 0 ] || fail "rm left the job"
cron "disable slow"
sleep 0.5
[ "$(q "select enabled from jobs where id=2")" = 0 ] || fail "disable did not clear enabled"
[ -z "$(q "select next_run_ms from jobs where id=2")" ] || fail "disable left next_run_ms"
cron "enable slow"
sleep 0.5
[ "$(q "select enabled from jobs where id=2")" = 1 ] || fail "enable did not set enabled"
[ -n "$(q "select next_run_ms from jobs where id=2")" ] || fail "enable did not set next_run_ms"
cron "status"
sleep 0.5
msgs | grep -q 'jobs (.* enabled), .* running, .* retries pending, .* runs kept, tz +00:00' ||
    fail "status line missing"

# --- 11. retention ----------------------------------------------------------------
$TMUX unload-plugin cron
sleep 0.3
load -o keep_runs=3
cron "add -n fast every 1s -- shell true"
sleep 7
kept=$(q "select count(*) from runs where job_id=(select id from jobs where name='fast') and state='ok'")
[ "$kept" -ge 3 ] || fail "fast did not run enough ($kept)"
[ "$kept" -le 3 ] || fail "retention kept $kept runs, wanted 3"

$TMUX kill-server 2>/dev/null
echo OK
exit 0
