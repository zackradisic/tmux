#!/bin/sh
# A bracketed paste from the terminal while typing into the preview. The
# terminal sends \e[200~, the text, \e[201~; tmux turns the markers into
# the PasteStart/PasteEnd keys and hands the text over one character at a
# time. The plugin mode must not show the markers to the plugin as keys
# (they used to drop the preview focus) and must deliver the text as one
# paste, into the focused agent's pane. A paste while the list has the
# keyboard still goes to the search box. Needs a real client with a tty,
# so a python pty drives `tmux attach`.
#
# Needs the wasm example built:
#   cargo build -p agents --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
SOCK=agents-paste-test
TMUX="$TEST_TMUX -L$SOCK"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm

HOME=$(mktemp -d); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
DEPLOY=$(mktemp -d); cp "$WASM" "$DEPLOY/agents.wasm"
cat >"$DEPLOY/agents.toml" <<'TOML'
[caps]
requests = ["capture-pane","run-command","mode","db","env-read","pane-fds","send-keys"]
[caps.env-read]
names = ["AI_AGENT","OPENCODE"]
TOML
FIFO="$HOME/keys"
OUT="$HOME/out"

cleanup() {
	[ -n "$PTY" ] && { kill $PTY 2>/dev/null; wait $PTY 2>/dev/null; }
	$TMUX kill-server 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; echo "--- screen:" >&2; screen >&2; cleanup; exit 1; }
screen() { $TMUX capture-pane -M -p -t "$FORM" | sed '/^ *$/d'; }
typing() { $TMUX capture-pane -M -p -t "$FORM" | grep -q 'typing into'; }
# The search line, as drawn.
search() { $TMUX capture-pane -M -p -t "$FORM" | sed -n 2p; }
# What the agent's pane shows, as one line.
pane() { $TMUX capture-pane -p -t "$A0" | tr -d '\n'; }
# Bytes typed at the real client's terminal.
type_raw() { printf "$1" >"$FIFO"; sleep 1; }

[ -f "$WASM" ] || fail "agents.wasm not built"
command -v python3 >/dev/null || { echo "SKIP: no python3"; exit 0; }

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s alpha -x 200 -y 50 \
    "sh -c 'AI_AGENT=claude exec cat'" || fail "new-session"
sleep 0.5
A0=$($TMUX list-panes -t alpha -F '#{pane_id}')

$TMUX load-plugin -s server -o trust_env=1 -c capture-pane -c run-command -c mode \
    -c db -c env-read -c pane-fds -c send-keys "$DEPLOY/agents.wasm" \
    || fail "load-plugin"
sleep 1.5

# A client with a tty: keys written to the fifo reach it, its output is
# drained to a file so it never blocks.
mkfifo "$FIFO"
cat >"$HOME/client.py" <<'PY'
import os, pty, sys, fcntl, termios, struct, select
tmux, sock, out, fifo = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.environ['TERM'] = 'xterm-256color'
    os.execv(tmux, [tmux, '-L', sock, 'attach'])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 50, 200, 0, 0))
ff = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
o = open(out, 'ab')
while True:
    r, _, _ = select.select([fd, ff], [], [], 0.2)
    if fd in r:
        try:
            d = os.read(fd, 65536)
        except OSError:
            break
        if not d:
            break
        o.write(d); o.flush()
    if ff in r:
        d = os.read(ff, 65536)
        if d:
            os.write(fd, d)
PY
python3 -I "$HOME/client.py" "$TEST_TMUX" "$SOCK" "$OUT" "$FIFO" &
PTY=$!
sleep 1.5
[ -n "$($TMUX list-clients)" ] || fail "the pty client did not attach"

# Open the picker from that client: prefix, :, the command.
type_raw '\002:'
type_raw 'plugin-command agents pick\r'
sleep 1
FORM=$($TMUX list-panes -a -F '#{pane_id} #{pane_mode}' |
    awk '/plugin-mode/ { print $1 }')
[ -n "$FORM" ] || fail "picker did not open"
i=0
while [ "$i" -lt 15 ]; do
	$TMUX capture-pane -M -p -t "$FORM" | grep -q '▸' && break
	sleep 0.4; i=$((i + 1))
done
screen | grep -q '1 live' || fail "expected 1 live agent"

# l focuses the preview; a bracketed paste then lands in the agent's pane
# as one piece, the focus stays, the search box is untouched.
$TMUX send-keys -t "$FORM" l; sleep 0.5
typing || fail "l did not focus the preview"
SEARCH=$(search)
type_raw '\033[200~hello paste\033[201~'
typing || fail "the paste dropped the preview focus"
[ "$(pane)" = "hello paste" ] || fail "the pasted text did not reach the pane once: '$(pane)'"
[ "$(search)" = "$SEARCH" ] || fail "the paste touched the search line: '$(search)'"

# Keys typed after the paste still go to the pane.
type_raw 'X'
[ "$(pane)" = "hello pasteX" ] || fail "typing after the paste went astray: '$(pane)'"

# paste-buffer at the picker takes the same road.
$TMUX set-buffer 'from buffer'; $TMUX paste-buffer -t "$FORM"; sleep 0.8
[ "$(pane)" = "hello pasteXfrom buffer" ] || fail "paste-buffer missed the pane: '$(pane)'"
typing || fail "paste-buffer dropped the preview focus"

# With the keyboard back on the list, a paste goes to the search box.
$TMUX send-keys -t "$FORM" C-]; sleep 0.5
typing && fail "C-] did not take the keyboard back"
type_raw '\033[200~srch\033[201~'
search | grep -q 'srch' || fail "an unfocused paste did not reach the search box: '$(search)'"
[ "$(pane)" = "hello pasteXfrom buffer" ] || fail "an unfocused paste reached the pane: '$(pane)'"

echo PASS
