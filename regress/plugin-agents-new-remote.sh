#!/bin/sh
# Starting an agent on a linked server from this one. `--session host/name`
# (a mirrored session) puts the agent on that server without `@host`, the
# folder being a path there; the forwarded new-window answers before the
# shadow window exists, so the launch waits for it and reports its ids,
# and `--focus` makes it the mirrored session's current window here too.
# The form from a remote row does the same on Enter and switches the
# pressing client to the new window, not to whatever the session showed.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release
. ./remote-common.inc
. ./fake-bin.inc
# The shell this runs from may be an agent's: its pane must not look like one.
unset AI_AGENT CLAUDECODE CLAUDE_CODE_SESSION_ID TMUX_PANE
BUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
[ -f "$BUILT" ] || fail "agents.wasm not built"
SIDECAR=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml
[ -f "$SIDECAR" ] || fail "no agents.toml"
XDG_A="$TMP/a"
XDG_B="$TMP/b"
BIN="$TMP/bin"
mkdir -p "$XDG_A" "$XDG_B" "$BIN" "$TMP/there" "$TMP/deploy"
fake_bin "$BIN/codex"
cp "$BUILT" "$TMP/deploy/agents.wasm"
cp "$SIDECAR" "$TMP/deploy/agents.toml"
WASM="$TMP/deploy/agents.wasm"

screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
keys() { $TMUX send-keys -t "$FORM" "$@"; sleep 0.4; }
# The verb's answer, once it is in.
result() {
	i=0
	while [ "$i" -lt 30 ]; do
		r=$($TMUX show-options -pqv -t "$PC" @agents_new)
		[ -n "$r" ] && [ "$r" != pending ] && { echo "$r"; return 0; }
		sleep 0.2; i=$((i + 1))
	done
	echo "$r"
}
new() { $TMUX plugin-command -t "$PC" agents "new $*" || fail "plugin-command new $*"; result; }

# B: a codex agent in session "work", in a folder B can list.
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 -c "$TMP/there" \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session on B"
# A: a plain pane to call the verb from.
XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s alpha -x 200 -y 50 || fail "new-session on A"
PC=$($TMUX list-panes -t alpha -F '#{pane_id}')
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null -C attach -t #{remote_session}" ||
    fail "set remote-ssh-command"
$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode -c db \
    -c env-read -c pane-fds -c fs-read -c fs-list -c run-process -c service-serve \
    -c service-call "$WASM" || fail "load-plugin"
sleep 1.0
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "$TMUX2 show-plugins | grep -q 'agents: .*role provider'" ||
    fail "not pushed: $($TMUX2 show-plugins)"
wait_for 10 "$TMUX has-session -t fakehost/work" || fail "no shadow session"
sleep 1.5

# The verb: a mirrored session, no @host. The window appears on B, and
# the answer names the shadow pane and window.
R=$(new --session fakehost/work --folder "$TMP/there" --command "'sleep 300'" --name probe)
case "$R" in
%[0-9]*\ @[0-9]*\ fakehost/work) ;;
*) fail "new on a mirrored session answered '$R'" ;;
esac
$TMUX2 list-windows -t work -F '#{window_name}' | grep -qx probe ||
    fail "no probe window on B: $($TMUX2 list-windows -t work -F '#{window_name}' | tr '\n' ' ')"
W=$(echo "$R" | awk '{ print $2 }')
[ -n "$($TMUX display-message -p -t "$W" '#{window_remote_id}')" ] ||
    fail "$W is not a shadow window"
# Made in the background: the mirrored session still shows its old window.
[ "$($TMUX display-message -p -t fakehost/work '#{window_name}')" != probe ] ||
    fail "a background launch became the current window"

# --focus: the mirrored session's current window follows.
R=$(new --session fakehost/work --folder "$TMP/there" --command "'sleep 300'" --name probe2 --focus)
case "$R" in
%[0-9]*\ @[0-9]*\ fakehost/work) ;;
*) fail "new --focus answered '$R'" ;;
esac
[ "$($TMUX display-message -p -t fakehost/work '#{window_name}')" = probe2 ] ||
    fail "--focus did not select the new shadow window: $($TMUX display-message -p -t fakehost/work '#{window_name}')"

# A folder that is not on B is B's business, not this disk's: no local
# "no folder" refusal.
R=$(new --session fakehost/work --folder "$TMP/there" --command "'sleep 300'" --name probe3)
case "$R" in
%[0-9]*\ @[0-9]*\ fakehost/work) ;;
*) fail "third launch answered '$R'" ;;
esac

# The form from the remote row: n, the command swapped for something B
# can run, Enter. The client lands on the new window in the mirrored
# session.
( sleep 0.3; echo 'plugin-command agents pick'; sleep 60 ) |
    $TMUX -C attach -t alpha >/dev/null 2>&1 &
CTL=$!
sleep 2.0
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"
wait_for 8 "$TMUX capture-pane -M -p -t $FORM | grep -q codex" || fail "no codex row: $(screen)"
i=0
until screen | grep -q '▸.*codex'; do
	keys Up; i=$((i + 1))
	[ "$i" -lt 8 ] || fail "could not put the cursor on codex: $(screen)"
done
keys n
sleep 1.0
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode} #{pane_title}' |
    awk '/plugin-mode/ && !/pick/ { print $1 }' | tail -1)
wait_for 6 "$TMUX capture-pane -M -p -t $FORM | grep -q 'on fakehost'" ||
    fail "the form is not fakehost's: $(screen)"
screen | grep -q 'fakehost/work' || fail "the session field is not the mirrored session: $(screen)"
# The path is clipped from the left when long; its tail is what counts.
screen | grep -q 'folder .*/there *$' || fail "the folder is not the remote pane's: $(screen)"
# The cursor starts on name; the command is the next field.
keys C-j; keys C-u
$TMUX send-keys -t "$FORM" -l 'sleep 300'; sleep 0.3
keys Enter
sleep 2.5
CSESS=$($TMUX list-clients -F '#{client_session}' | head -1)
[ "$CSESS" = "fakehost/work" ] ||
    fail "client is on $CSESS, not fakehost/work: $(screen) --- $($TMUX plugin-log agents | tail -6)"
CWIN=$($TMUX display-message -p -t fakehost/work '#{window_name}')
[ "$CWIN" = there ] || fail "client did not land on the new window: on '$CWIN'"
$TMUX2 list-windows -t work -F '#{window_name}' | grep -qx there || fail "no 'there' window on B"
$TMUX list-panes -a -F '#{pane_mode}' | grep -q plugin-mode && fail "form or picker still open"

echo PASS
