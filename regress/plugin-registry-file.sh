#!/bin/sh
# The release registry without a network: a manifest whose [registry]
# points at file:// urls. A sync fetches the registry plugins and a url
# entry into the content-addressed cache, writes the lock, and loads them
# when the downloads land. A second sync moves nothing. update-plugins -n
# reports a new release without applying it; update-plugins moves the
# lock and live-reloads. A url entry with a wrong hash never loads. An
# index built for another ABI is refused. The daily check tells the user
# about a new release when the lock's last check is old.
#
# Needs the wasm examples built (release and wasm-release profiles):
#   cargo build -p ticker -p services_probe -p hello-raw \
#       --target wasm32-unknown-unknown --release
#   cargo build -p ticker --target wasm32-unknown-unknown --profile wasm-release
# and curl on PATH. plugin-index is built here when missing.

PATH=/bin:/usr/bin:/usr/local/bin:$HOME/.cargo/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
ROOT=$(dirname "$TEST_TMUX")
TMUX="$TEST_TMUX -Lregistry$$ -f/dev/null"
TMP=$(mktemp -d)
export XDG_DATA_HOME="$TMP/data"

cleanup()
{
	$TMUX kill-server 2>/dev/null
	rm -rf "$TMP"
}
trap cleanup EXIT

fail()
{
	echo "FAIL: $*" >&2
	echo "--- plugin-log" >&2
	$TMUX plugin-log 2>/dev/null | tail -20 >&2
	exit 1
}

wait_for()
{
	timeout=$1
	shift
	i=0
	while [ "$i" -lt "$((timeout * 5))" ]; do
		if sh -c "$*" >/dev/null 2>&1; then
			return 0
		fi
		sleep 0.2
		i=$((i + 1))
	done
	return 1
}

command -v curl >/dev/null || fail "curl not on PATH"

REL="$ROOT/plugin-host/target/wasm32-unknown-unknown/release"
STRIPPED="$ROOT/plugin-host/target/wasm32-unknown-unknown/wasm-release"
for f in "$REL/ticker.wasm" "$REL/services_probe.wasm" "$REL/hello_raw.wasm" \
    "$STRIPPED/ticker.wasm"; do
	[ -f "$f" ] || fail "no $f"
done
INDEX="$ROOT/plugin-host/target/release/plugin-index"
[ -x "$INDEX" ] || INDEX="$ROOT/plugin-host/target/debug/plugin-index"
if [ ! -x "$INDEX" ]; then
	(cd "$ROOT/plugin-host" && cargo build -q -p plugin-index) ||
	    fail "cannot build plugin-index"
	INDEX="$ROOT/plugin-host/target/debug/plugin-index"
fi

# Two releases: rel1 with the release-profile ticker, rel2 with the
# stripped one (different bytes, version 0.2.0).
mkdir -p "$TMP/rel1" "$TMP/rel2" "$TMP/url"
cp "$REL/ticker.wasm" "$REL/services_probe.wasm" "$TMP/rel1/"
cp "$STRIPPED/ticker.wasm" "$REL/services_probe.wasm" "$TMP/rel2/"
printf '[caps]\nrequests = ["service-serve", "service-call", "write-options"]\n' \
    > "$TMP/rel1/services_probe.toml"
cp "$TMP/rel1/services_probe.toml" "$TMP/rel2/"
"$INDEX" --dir "$TMP/rel1" --tag rel1 --version ticker=0.1.0 \
    --version services_probe=0.1.0 >/dev/null || fail "index rel1"
"$INDEX" --dir "$TMP/rel2" --tag rel2 --version ticker=0.2.0 \
    --version services_probe=0.1.0 >/dev/null || fail "index rel2"
cp "$REL/hello_raw.wasm" "$TMP/url/"
"$INDEX" --dir "$TMP/url" --tag url --version hello_raw=0.1.0 >/dev/null ||
    fail "index url"
HELLO_HASH=$(sed -n 's/^blake3 = "\(.*\)"/\1/p' "$TMP/url/index.toml")
[ -n "$HELLO_HASH" ] || fail "no hash for hello_raw"

printf '{"tag_name": "rel1"}\n' > "$TMP/latest.json"
cat > "$TMP/plugins.toml" <<EOM
[registry]
api_url = "file://$TMP/latest.json"
base_url = "file://$TMP/{tag}"

[plugins.ticker]
scope = "server"

[plugins.services_probe]
caps = ["service-serve", "service-call", "write-options"]

[plugins.hello]
url = "file://$TMP/url/hello_raw.wasm"
hash = "blake3:$HELLO_HASH"
EOM

$TMUX new-session -d -s main 'sleep 600' || fail "new-session"

# 1. First sync: nothing is cached, everything fetches.
out=$($TMUX sync-plugins "$TMP/plugins.toml") || fail "sync 1: $out"
case "$out" in
*"1 fetching"*"resolving release latest"*) ;;
*) fail "sync 1 report: $out" ;;
esac
wait_for 20 "$TMUX show-plugins | grep -q '^ticker: version 0.1.0, .*running'" ||
    fail "ticker not loaded: $($TMUX show-plugins)"
wait_for 20 "$TMUX show-plugins | grep -q '^services_probe: version 0.1.0, .*running'" ||
    fail "services_probe not loaded: $($TMUX show-plugins)"
wait_for 20 "$TMUX show-plugins | grep -q '^hello: .*running'" ||
    fail "hello not loaded: $($TMUX show-plugins)"
$TMUX show-plugins | grep -q '^ticker: .*managed, path .*/plugin-cache/cas/' ||
    fail "ticker not from the cache: $($TMUX show-plugins)"
grep -q '^tag = "rel1"' "$TMP/plugins.lock" || fail "lock: $(cat "$TMP/plugins.lock")"
grep -q '^\[plugins.ticker\]' "$TMP/plugins.lock" || fail "lock lacks ticker"
n=$(ls "$XDG_DATA_HOME/tmux/plugin-cache/cas/"*.wasm | wc -l)
[ "$n" -eq 3 ] || fail "expected 3 cached modules, got $n"
# The sidecar came along and its requests count.
$TMUX show-plugins -v | grep -q 'service-serve' || fail "sidecar caps missing"

# 2. Second sync: all cached, nothing moves.
out=$($TMUX sync-plugins "$TMP/plugins.toml") || fail "sync 2: $out"
case "$out" in
*"0 loaded, 0 updated, 3 unchanged, 0 unloaded"*) ;;
*) fail "sync 2 report: $out" ;;
esac
case "$out" in *fetching*|*resolving*) fail "sync 2 fetched: $out" ;; esac

# 3. Check only: the registry still says rel1.
out=$($TMUX update-plugins -n "$TMP/plugins.toml") || fail "update -n: $out"
case "$out" in
*"release rel1"*"up to date"*) ;;
*) fail "update -n report: $out" ;;
esac

# 4. A new release appears. -n reports and changes nothing.
printf '{"tag_name": "rel2"}\n' > "$TMP/latest.json"
out=$($TMUX update-plugins -n "$TMP/plugins.toml") || fail "update -n rel2: $out"
case "$out" in
*"release rel2"*"lock had rel1"*"services_probe: 0.1.0 unchanged"*"ticker: 0.1.0 -> 0.2.0"*) ;;
*) fail "update -n rel2 report: $out" ;;
esac
$TMUX show-plugins | grep -q '^ticker: version 0.1.0' || fail "-n changed ticker"
grep -q '^tag = "rel1"' "$TMP/plugins.lock" || fail "-n moved the lock"

# 5. Apply: the lock moves, ticker reloads live.
out=$($TMUX update-plugins "$TMP/plugins.toml") || fail "update: $out"
case "$out" in
*"ticker: 0.1.0 -> 0.2.0"*"1 fetching"*) ;;
*) fail "update report: $out" ;;
esac
# The module changes (the stripped build has another hash); its crate
# version stays 0.1.0, since that comes from the wasm, not the index.
REL2_HASH=$(sed -n '/^\[plugins.ticker\]/,/^$/s/^blake3 = "\(.*\)"/\1/p' "$TMP/rel2/index.toml")
[ -n "$REL2_HASH" ] || fail "no rel2 ticker hash"
wait_for 20 "$TMUX show-plugins | grep -q '^ticker: .*running.*/cas/$REL2_HASH.wasm'" ||
    fail "ticker not updated: $($TMUX show-plugins)"
grep -q '^tag = "rel2"' "$TMP/plugins.lock" || fail "lock not moved"
grep -q '^version = "0.2.0"' "$TMP/plugins.lock" || fail "lock version: $(cat "$TMP/plugins.lock")"

# 6. A url entry with the wrong hash never loads.
cat > "$TMP/bad.toml" <<EOM
[plugins.badhash]
url = "file://$TMP/url/hello_raw.wasm"
hash = "blake3:$(printf '0%.0s' $(seq 1 64))"
EOM
out=$($TMUX sync-plugins "$TMP/bad.toml") || fail "sync bad: $out"
case "$out" in *"1 fetching"*) ;; *) fail "sync bad report: $out" ;; esac
wait_for 20 "$TMUX plugin-log | grep -q 'badhash.*hash mismatch'" ||
    fail "no hash mismatch in the log"
$TMUX show-plugins | grep -q '^badhash' && fail "badhash loaded"
# That sync's manifest names none of the first one's plugins, so it
# swept them: managed plugins follow the last manifest synced.
$TMUX show-plugins | grep -q '^ticker' && fail "sweep did not unload ticker"

# 7. An index for another ABI is refused.
sed -i 's/^abi = .*/abi = 999/' "$TMP/rel2/index.toml"
out=$($TMUX update-plugins -n "$TMP/plugins.toml" 2>&1) && fail "abi 999 accepted: $out"
case "$out" in *"needs ABI 999"*) ;; *) fail "abi error text: $out" ;; esac
sed -i "s/^abi = .*/abi = $(sed -n 's/^abi = //p' "$TMP/rel1/index.toml")/" "$TMP/rel2/index.toml"

# 8. The daily check: an old lock stamp and a new release make a message.
$TMUX sync-plugins "$TMP/plugins.toml" >/dev/null || fail "sync 3"
# rel3 carries the release-profile ticker again: other bytes than rel2.
mkdir -p "$TMP/rel3"
cp "$REL/ticker.wasm" "$REL/services_probe.wasm" "$TMP/rel1/services_probe.toml" \
    "$TMP/rel3/"
"$INDEX" --dir "$TMP/rel3" --tag rel3 --version ticker=0.3.0 \
    --version services_probe=0.1.0 >/dev/null || fail "index rel3"
printf '{"tag_name": "rel3"}\n' > "$TMP/latest.json"
sed -i 's/^checked = .*/checked = "2020-01-01T00:00:00Z"/' "$TMP/plugins.lock"
$TMUX sync-plugins "$TMP/plugins.toml" >/dev/null || fail "sync 4"
wait_for 20 "$TMUX plugin-log | grep -q '1 plugin update in release rel3'" ||
    fail "no update message"
wait_for 5 "grep -q '^checked = \"20[2-9][0-9]-' '$TMP/plugins.lock' && ! grep -q 'checked = \"2020-01-01' '$TMP/plugins.lock'" ||
    fail "checked stamp not renewed: $(grep checked "$TMP/plugins.lock")"
grep -q '^tag = "rel2"' "$TMP/plugins.lock" || fail "the check moved the lock"

# With the option off, no check runs.
$TMUX set -s plugin-update-check off
sed -i 's/^checked = .*/checked = "2020-01-01T00:00:00Z"/' "$TMP/plugins.lock"
$TMUX sync-plugins "$TMP/plugins.toml" >/dev/null || fail "sync 5"
sleep 1
grep -q 'checked = "2020-01-01' "$TMP/plugins.lock" || fail "check ran with the option off"

echo OK
exit 0
