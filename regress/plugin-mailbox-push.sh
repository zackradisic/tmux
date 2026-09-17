#!/bin/sh
# Push: a message to a Claude agent wakes it. A stub Claude in a pane on A
# writes its session file (naming its pane and an inbox socket), then
# listens on that socket. A message to its id lands on the socket as one
# queued user turn and is marked read. Then B (the remote) messages the
# same agent by <id>@<server>: once A allows the pair, the push crosses
# the link the same way. A message to a plain box is left for `inbox`.
#
# Needs the wasm examples built:
#   cargo build -p agents -p mailbox --target wasm32-unknown-unknown --release
# and an nc that speaks unix sockets (-U).

. ./remote-common.inc

nc -h 2>&1 | grep -q -- '-U' || { echo "SKIP: nc without -U"; exit 0; }

AGBUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm
MBBUILT=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/mailbox.wasm
AGSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/agents/agents.toml
MBSIDE=$(dirname "$TEST_TMUX")/plugin-host/examples/mailbox/mailbox.toml
[ -f "$AGBUILT" ] || fail "no agents.wasm"
[ -f "$MBBUILT" ] || fail "no mailbox.wasm"

XDG_A="$TMP/a"; XDG_B="$TMP/b"; BIN="$TMP/bin"; HOME_A="$TMP/home"
mkdir -p "$XDG_A" "$XDG_B" "$BIN" "$TMP/dep" "$HOME_A/.claude/sessions" "$TMP/cc-socks"
cp /bin/sh "$BIN/codex"
cp "$AGBUILT" "$TMP/dep/agents.wasm"; cp "$AGSIDE" "$TMP/dep/agents.toml"
cp "$MBBUILT" "$TMP/dep/mailbox.wasm"; cp "$MBSIDE" "$TMP/dep/mailbox.toml"
AG="$TMP/dep/agents.wasm"; MB="$TMP/dep/mailbox.wasm"
HOST=$(hostname)
SOCK="$TMP/cc-socks/4242.sock"; LOG="$TMP/received.log"; : > "$LOG"

# The stub Claude: name its own pane in a session file the way Claude
# does, then listen on the inbox socket and log every line it gets.
cat > "$BIN/claude-stub" <<EOF
#!/bin/sh
pane=\$(printf %s "\$TMUX_PANE" | tr -d '%')
win=\$($TEST_TMUX -LtestA$$ -f/dev/null display-message -p -t "\$TMUX_PANE" '#{window_id}' | tr -d '@')
printf '{"pid":4242,"sessionId":"push-test","status":"idle","tmux":"local:@%s.%%%s","messagingSocketPath":"%s"}\n' \
    "\$win" "\$pane" "$SOCK" > "$HOME_A/.claude/sessions/4242.json"
exec nc -l -k -U "$SOCK" >> "$LOG"
EOF
chmod +x "$BIN/claude-stub"

AGCAPS="-c capture-pane -c run-command -c mode -c db -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve -c service-call -c claude-notify"
MBCAPS="-c db -c service-serve -c service-call -c write-options -c display-message"

# B: the remote, with a codex agent so it has a roster.
XDG_DATA_HOME="$XDG_B" $TMUX2 new-session -d -s work -x 200 -y 50 \
    "sh -c 'exec $BIN/codex -c \"read -r _\"'" || fail "new-session B"
# A: the stub Claude. HOME points the plugin's ~/.claude/sessions at ours.
HOME="$HOME_A" XDG_DATA_HOME="$XDG_A" $TMUX new-session -d -s local -x 200 -y 50 \
    -e AI_AGENT=claude "$BIN/claude-stub" || fail "new-session A"
$TMUX set -s remote-ssh-command \
    "$TEST_TMUX -LtestB$$ -f/dev/null #{remote_command}" || fail "ssh-command"
wait_for 5 "[ -S $SOCK ]" || fail "stub did not listen"
grep -q '"tmux":"local:@[0-9]*\.%[0-9]*"' "$HOME_A/.claude/sessions/4242.json" ||
    fail "stub session file: $(cat "$HOME_A/.claude/sessions/4242.json")"

XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server $MBCAPS "$MB" || fail "mb A"
XDG_DATA_HOME="$XDG_A" $TMUX load-plugin -s server -o trust_env=1 $AGCAPS "$AG" || fail "ag A"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $MBCAPS "$MB" || fail "mb B"
XDG_DATA_HOME="$XDG_B" $TMUX2 load-plugin -s server $AGCAPS "$AG" || fail "ag B"
sleep 1.5

AID="claude:push-test"

# A local message to the Claude agent: pushed to its socket as one user
# turn, sender named, and marked read.
wait_for 20 "$TMUX plugin-command agents 'message $AID hello push' 2>/dev/null; \
    grep -q 'hello push' $LOG" || fail "push did not land: $(cat "$LOG"; $TMUX show-messages | tail -3)"
grep -q '"type":"user"' "$LOG" || fail "not a user turn: $(cat "$LOG")"
grep -q '"role":"user"' "$LOG" || fail "no role: $(cat "$LOG")"
grep -q 'via the tmux2 mailbox' "$LOG" || fail "no sender line: $(cat "$LOG")"
wait_for 5 "$TMUX plugin-command mailbox list; sleep 0.4; $TMUX show-messages | grep -q 'mailbox: no unread'" ||
    fail "pushed message not marked read: $($TMUX show-messages | grep mailbox: | tail -2)"

# A plain box is nobody's session: it stays for inbox, and nothing hits the
# socket.
: > "$LOG"
$TMUX plugin-command mailbox 'send plainbox hello box' || fail "send plainbox"
sleep 1
[ ! -s "$LOG" ] || fail "plain box reached the socket: $(cat "$LOG")"
$TMUX plugin-command mailbox 'inbox plainbox' >/dev/null 2>&1
$TMUX show-options -s -v @mailbox_plainbox | grep -q 'hello box' || fail "plain box not stored"

# Across the link: B messages A's Claude by <id>@<server>. Denied until A
# allows the pair, then pushed into the same socket with a qualified
# sender.
$TMUX remote-attach -t work fakehost || fail "remote-attach"
wait_for 10 "[ \"\$($TMUX display-message -p -t fakehost/work '#{remote_connected}')\" = 1 ]" ||
    fail "link not up"
sleep 2
: > "$LOG"
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command agents "message $AID@$HOST from B denied" || fail "B message 1"
sleep 1
[ ! -s "$LOG" ] || fail "B->A pushed while denied: $(cat "$LOG")"
$TMUX plugin-peers allow fakehost mailbox || fail "allow"
XDG_DATA_HOME=$XDG_B $TMUX2 plugin-command agents "message $AID@$HOST from B allowed" || fail "B message 2"
wait_for 10 "grep -q 'from B allowed' $LOG" || fail "B->A push did not land: $(cat "$LOG")"
grep -q '@fakehost' "$LOG" || fail "sender not qualified with the server: $(cat "$LOG")"
exit 0
