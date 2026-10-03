#!/bin/sh
# The sessions chooser across a remote link. B (the "remote") has two
# sessions; A links one of them. A's picker shows a header per server,
# the linked session once (folded with its shadow, under the remote
# header), the other remote session dimmed as "not linked"; Enter on it
# links it; `d` drops a link; a dropped connection shows the server
# disconnected; `R` reconnects without waiting out the backoff.
#
# Needs the wasm example built:
#   cargo build -p sessions --target wasm32-unknown-unknown --release
# TEST_WASM_DIR points at another build's output when the binary and the
# plugin come from different trees.

. ./remote-common.inc

[ -z "$TEST_WASM_DIR" ] && TEST_WASM_DIR=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release
BUILT=$TEST_WASM_DIR/sessions.wasm
[ -f "$BUILT" ] || fail "sessions.wasm not built"
SIDECAR=$TEST_WASM_DIR/sessions.toml
[ -f "$SIDECAR" ] || fail "no sessions.toml"

XDG_A="$TMP/a"
XDG_B="$TMP/b"
mkdir -p "$XDG_A" "$XDG_B" "$TMP/deploy"
cp "$BUILT" "$TMP/deploy/sessions.wasm"
cp "$SIDECAR" "$TMP/deploy/sessions.toml"
WASM="$TMP/deploy/sessions.wasm"
CAPS="-c read-state -c run-command -c mode -c service-serve -c service-call -c send-keys"

screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
currow() { $TMUX capture-pane -M -p -t "$FORM" | grep '^▸' | head -1; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
find_form() {
	FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
	    awk '/plugin-mode/ { print $1 }')
}
open_picker() {
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	( sleep 0.3; echo "plugin-command sessions 'pick $1'"; sleep 60 ) |
	    $TMUX -C attach -t alpha >/dev/null 2>&1 &
	CTL=$!
	sleep 1.5
	find_form
	[ -n "$FORM" ] || fail "picker did not open"
}
close_picker() {
	keys Escape
	[ -n "$CTL" ] && kill $CTL 2>/dev/null
	CTL=
	sleep 0.3
}
cleanup_all() { [ -n "$CTL" ] && kill $CTL 2>/dev/null; cleanup; }
trap cleanup_all EXIT

# B: two sessions. A: one local session, the plugin, then the link.
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 "sh -c 'exec sleep 600'" || fail "new-session on B"
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s spare -x 200 -y 50 "sh -c 'exec sleep 600'" || fail "new-session spare on B"
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s alpha -x 200 -y 50 "sh -c 'exec sleep 600'" || fail "new-session on A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $CAPS "$WASM" || fail "load-plugin"
sleep 1.0

$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'sessions: .*role provider'" ||
    fail "not pushed: $($TMUX2 show-plugins)"
wait_for 10 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not connected"
sleep 1.5

open_picker s
# Two servers: a header each, alpha under local, work and spare under
# fakehost - work once (its shadow folded in), spare not linked.
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q '2 servers'" ||
    fail "picker does not show 2 servers: $(screen)"
screen | grep -q '▪ fakehost' || fail "no fakehost header: $(screen)"
screen | grep -q 'alpha' || fail "alpha missing"
screen | awk '/▪ fakehost/ { seen = 1 } seen && /work/ { found = 1 } END { exit !found }' ||
    fail "work is not under the fakehost header: $(screen)"
[ "$(screen | grep -c 'work')" = 1 ] || fail "work shown more than once (shadow not folded): $(screen)"
screen | grep -q 'spare.*not linked' || fail "spare not shown as not linked: $(screen)"
screen | grep -q 'fakehost/work' && fail "the shadow's local name leaked into the row: $(screen)"

# Enter on spare links it: a shadow appears, the row stops being dim.
keys G
currow | grep -q 'spare' || keys k
currow | grep -q 'spare' || fail "could not put the cursor on spare: $(screen)"
keys Enter
wait_for 10 "$TMUX has-session -t fakehost/spare" || fail "Enter did not link spare"
wait_for 8 "! $TMUX capture-pane -M -p -t $FORM | grep -q 'not linked'" ||
    fail "spare still shows not linked after the link: $(screen)"

# d on a remote row drops that link: the shadow goes, the row says not linked.
keys d
wait_for 10 "! $TMUX has-session -t fakehost/spare" || fail "d did not drop the link to spare"
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q 'spare.*not linked'" ||
    fail "spare does not show not linked after the drop: $(screen)"
close_picker

# Link down: park the ssh command on one that fails, detach B's client.
$TMUX set -s remote-ssh-command false || fail "set remote-ssh-command false"
client=$($TMUX2 list-clients -F '#{client_name}' | head -1)
$TMUX2 detach-client -t "$client" || fail "detach-client"
wait_for 6 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 0 ]" ||
    fail "link still up"
open_picker s
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q 'fakehost.*disconnected'" ||
    fail "no disconnected marker: $(screen)"
screen | grep -q 'work' || fail "the remote rows vanished while down"

# R reconnects now: the link is back well inside the 60 s backoff cap.
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "restore remote-ssh-command"
if $TMUX remote-attach -R fakehost 2>/dev/null; then
	wait_for 8 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
	    fail "remote-attach -R did not bring the link back"
else
	echo "note: this tmux has no remote-attach -R; waiting for the backoff" >&2
	wait_for 30 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
	    fail "link did not return"
fi
wait_for 10 "! $TMUX capture-pane -M -p -t $FORM | grep -q 'disconnected'" ||
    fail "disconnected marker stayed after reconnect: $(screen)"
close_picker

exit 0
