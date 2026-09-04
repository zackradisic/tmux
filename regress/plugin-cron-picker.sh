#!/bin/sh
# The cron picker (`plugin-command cron pick`): two jobs, open the float
# from an attached control client and drive it with send-keys, reading
# the mode screen back with capture-pane -M:
#
#   the title counts the jobs; e toggles enabled (and the DB column); l
#   shows the last run's output; Enter starts a run (the row shows the
#   running glyph); d asks y/n and y deletes the job; Esc closes; the
#   "next" cell counts down while the picker is open. Needs the wasm
#   example built:
#     cargo build -p cron --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lcron-picker-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/cron.wasm
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DB=$XDG_DATA_HOME/tmux/plugins/cron/store.db
FIFO=$XDG_DATA_HOME/ctl
trap 'cleanup' EXIT

cleanup() {
	exec 3>&- 2>/dev/null
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	$TMUX kill-server 2>/dev/null
	rm -rf "$XDG_DATA_HOME"
}
fail() {
	echo "FAIL: $*" >&2
	echo "--- screen:" >&2
	screen >&2
	cleanup
	trap - EXIT
	exit 1
}
q() {
	python3 -c "import sqlite3, sys
c = sqlite3.connect('file:$DB?mode=ro', uri=True)
r = c.execute(sys.argv[1]).fetchone()
print('' if r is None or r[0] is None else r[0])" "$1"
}
cron() { $TMUX plugin-command cron "$1"; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.5; }
rows() { screen | grep -c '#[0-9]'; }
find_form() {
	FORM=
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
		    awk '/plugin-mode/ { print $1 }')
		[ -n "$FORM" ] && return 0
		sleep 0.3
	done
	return 1
}

[ -f "$WASM" ] || fail "cron.wasm not built"
command -v python3 >/dev/null || fail "python3 needed"

$TMUX kill-server 2>/dev/null
sleep 0.5

$TMUX -f/dev/null new-session -d -s alpha -x 140 -y 40 "sleep 600" ||
    fail "new-session"
$TMUX load-plugin -c db -c run-process -c run-command -c mode -o tz=utc \
    "$WASM" || fail "load-plugin"
sleep 0.5
cron "add -n first every 1h -- shell echo hello from first"
cron "add -n second every 1h -- shell sleep 3"
cron "run first"
sleep 1.5
[ "$(q "select count(*) from runs where job_id=1 and state='ok'")" = 1 ] ||
    fail "first did not run"

# A control client gives the picker a current window to open on; its
# stdin is a fifo so more commands can follow later.
mkfifo "$FIFO"
$TMUX -C attach < "$FIFO" >/dev/null 2>&1 &
CTL=$!
exec 3>"$FIFO"
sleep 0.5
echo 'plugin-command cron pick' >&3
find_form || fail "picker did not open"
sleep 0.3

screen | grep -q 'cron jobs (2)' || fail "title does not count the jobs"
[ "$(rows)" = 2 ] || fail "expected two job rows, got $(rows)"
screen | grep -q 'first' || fail "row for first missing"
screen | grep -q 'run ·' || fail "footer missing"
screen | grep -q 'ok [0-9]*s ago' || fail "first's last run not shown"

# e toggles the highlighted job (row 1 = first).
keys e
screen | grep -q '○' || fail "toggle did not show the disabled glyph"
[ "$(q "select enabled from jobs where id=1")" = 0 ] || fail "toggle did not disable in the DB"
screen | grep -q 'disabled' || fail "next cell does not say disabled"
keys e
[ "$(q "select enabled from jobs where id=1")" = 1 ] || fail "toggle did not re-enable"

# l shows the last run's output.
keys l
screen | grep -q 'last run: ok' || fail "detail header missing"
screen | grep -q 'hello from first' || fail "detail output missing"
keys l
screen | grep -q 'last run:' && fail "detail did not close"

# Enter on the second row starts a run; the row shows the running glyph.
keys j
keys Enter
screen | grep -q '▶' || fail "running glyph missing after Enter"
screen | grep -q 'running #2 "second" now' || fail "run was not reported"
[ "$(q "select count(*) from runs where job_id=2 and state='running'")" = 1 ] ||
    fail "no running row for second"
sleep 3.5
[ "$(q "select count(*) from runs where job_id=2 and state='ok'")" = 1 ] ||
    fail "second did not finish"

# d asks; n keeps, y deletes.
keys d
screen | grep -q 'delete .*y/n' || fail "d did not prompt"
keys n
screen | grep -q 'y/n' && fail "n did not cancel the prompt"
[ "$(q 'select count(*) from jobs')" = 2 ] || fail "n deleted something"
keys d
keys y
sleep 0.5
[ "$(q 'select count(*) from jobs')" = 1 ] || fail "y did not delete the job"
screen | grep -q 'deleted #2' || fail "delete not reported"
screen | grep -q 'cron jobs (1)' || fail "title did not update after delete"

# Esc closes the float.
keys Escape
sleep 0.5
$TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode &&
    fail "picker did not close on Escape"

# Reopen: the next cell counts down while the picker is open.
echo 'plugin-command cron pick' >&3
find_form || fail "picker did not reopen"
sleep 0.3
before=$(screen | grep 'first')
echo "$before" | grep -q 'next in' || fail "no countdown cell: $before"
sleep 1.3
after=$(screen | grep 'first')
[ "$before" != "$after" ] || fail "next cell did not tick: $before"

cleanup
trap - EXIT
echo OK
exit 0
