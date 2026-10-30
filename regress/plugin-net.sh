#!/bin/sh
# The `net` capability through the net-probe example: a plugin fetches a
# URL from a local `python3 -m http.server` and answers on the caller's
# terminal at once (reply), then lands the status and first body line in
# the `@net_probe` server option; a plugin whose sidecar allowlist names
# another host is refused before anything is sent; a plugin without the
# capability is denied. The websocket half is covered by the host unit
# test (`cargo test -p plugin-host --lib net::`): python3's standard
# library has no websocket server to run it against here.
#
# Needs the wasm example built:
#   cargo build -p net-probe --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin:/usr/local/bin:/opt/homebrew/bin
TERM=screen
unset TMUX TMUX_PANE AI_AGENT

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lnet-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/net_probe.wasm

HOME=$(mktemp -d /tmp/net.XXXXXX); HOME=$(cd "$HOME" && pwd -P); export HOME
XDG_DATA_HOME="$HOME/.local/share"; export XDG_DATA_HOME
WWW="$HOME/www"; mkdir -p "$WWW"
printf 'hello net\nsecond line\n' >"$WWW/index.html"
DEPLOY=$(mktemp -d)
mkdir -p "$DEPLOY/open" "$DEPLOY/narrow"
cp "$WASM" "$DEPLOY/open/net-probe.wasm"
cp "$WASM" "$DEPLOY/narrow/net-probe.wasm"
cat >"$DEPLOY/narrow/net-probe.toml" <<'TOML'
[caps]
requests = ["net", "write-options"]
[caps.net]
hosts = ["example.invalid", "*.slack.com"]
TOML

HTTPD=
WATCHDOG=
cleanup() {
	$TMUX kill-server 2>/dev/null
	[ -n "$WATCHDOG" ] && kill "$WATCHDOG" 2>/dev/null
	[ -n "$HTTPD" ] && kill "$HTTPD" 2>/dev/null
	wait 2>/dev/null
	rm -rf "$HOME" "$DEPLOY"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; cleanup; exit 1; }
# The result lands in a server option; wait for it to leave "pending".
result() {
	i=0
	while [ $i -lt 60 ]; do
		r=$($TMUX show-options -sqv "$1")
		[ -n "$r" ] && [ "$r" != pending ] && { echo "$r"; return; }
		sleep 0.25; i=$((i + 1))
	done
	echo "timeout"
}

[ -f "$WASM" ] || fail "net-probe.wasm not built"
command -v python3 >/dev/null || fail "python3 not found"

# A local web server on a free port.
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
( cd "$WWW" && exec python3 -m http.server --bind 127.0.0.1 "$PORT" >"$HOME/httpd.log" 2>&1 ) &
HTTPD=$!
i=0
while [ $i -lt 40 ]; do
	python3 -c "import socket; socket.create_connection(('127.0.0.1', $PORT), 1).close()" 2>/dev/null && break
	sleep 0.25; i=$((i + 1))
done
[ $i -lt 40 ] || fail "http.server did not come up"

# Watchdog: nothing below may hang.
( sleep 90; echo "FAIL: watchdog" >&2; $TMUX kill-server 2>/dev/null; kill $$ 2>/dev/null ) &
WATCHDOG=$!

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s main -x 120 -y 40 'sleep 600' || fail "new-session"
$TMUX load-plugin -n probe -s server -c net -c write-options "$DEPLOY/open/net-probe.wasm" || fail "load-plugin probe"
$TMUX load-plugin -n narrow -s server -c net -c write-options "$DEPLOY/narrow/net-probe.wasm" || fail "load-plugin narrow"
$TMUX load-plugin -n nocap -s server -c write-options "$DEPLOY/open/net-probe.wasm" || fail "load-plugin nocap"
sleep 1

# --- a GET, answered on the terminal and then in the option ---------------
URL="http://127.0.0.1:$PORT/index.html"
OUT=$($TMUX plugin-command probe "get $URL")
case "$OUT" in *"net-probe: GET $URL"*) ;; *) fail "reply did not reach the caller: $OUT" ;; esac
R=$(result @net_probe)
[ "$R" = "200 hello net" ] || fail "unexpected GET result: $R"
grep -q "GET /index.html" "$HOME/httpd.log" || fail "the server saw no request"

# --- a 404 is a result, not an error ---------------------------------------
$TMUX set-option -s @net_probe pending
$TMUX plugin-command probe "get http://127.0.0.1:$PORT/missing" >/dev/null
R=$(result @net_probe)
case "$R" in 404*) ;; *) fail "a 404 was not reported as a status: $R" ;; esac

# --- a sidecar allowlist refuses other hosts before anything is sent -------
$TMUX set-option -s @net_probe pending
: >"$HOME/httpd.log"
$TMUX plugin-command narrow "get $URL" >/dev/null
R=$(result @net_probe)
case "$R" in "error: E_CAP_DENIED"*) ;; *) fail "the allowlist did not refuse 127.0.0.1: $R" ;; esac
sleep 0.5
grep -q "GET" "$HOME/httpd.log" && fail "a refused request still reached the server"

# --- no capability: denied ------------------------------------------------
$TMUX set-option -s @net_probe pending
$TMUX plugin-command nocap "get $URL" >/dev/null
R=$(result @net_probe)
case "$R" in "error: E_CAP_DENIED"*) ;; *) fail "a plugin without net was not denied: $R" ;; esac

# --- a bad scheme is refused at the call ----------------------------------
$TMUX set-option -s @net_probe pending
$TMUX plugin-command probe "get ftp://127.0.0.1/x" >/dev/null
R=$(result @net_probe)
case "$R" in "error: E_BAD_REQUEST"*) ;; *) fail "ftp:// was not refused: $R" ;; esac

# --- a refused connection is a transport error with a message ---------------
FREE=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
$TMUX set-option -s @net_probe pending
$TMUX plugin-command probe "get http://127.0.0.1:$FREE/" >/dev/null
R=$(result @net_probe)
case "$R" in "error: E_HOST "*) ;; *) fail "a refused connection was not an E_HOST error: $R" ;; esac

# --- the websocket half: a handshake against a plain HTTP server fails
# cleanly (the echo round trip itself is the host unit test) -----------------
$TMUX set-option -s @net_probe pending
$TMUX plugin-command probe "ws ws://127.0.0.1:$PORT/" >/dev/null
R=$(result @net_probe)
case "$R" in "error: E_HOST "*) ;; *) fail "a failed websocket handshake was not reported: $R" ;; esac

$TMUX show-plugins >/dev/null || fail "show-plugins"
$TMUX unload-plugin probe || fail "unload-plugin"

echo "PASS"
exit 0
