#!/bin/sh
# First-run import for the resurrect plugin. A pre-SQLite state.bin
# (magic TMUXRES2, u32 meta length, JSON tree, raw pane blobs) sits in
# the data dir when the plugin loads. It must become snapshot #1 with
# reason `import` and its original timestamp, the file must be renamed
# to state.bin.imported, and `restore` must rebuild the session from the
# imported chunks. A second load must not import again. Needs the wasm
# example built:
#   cargo build -p resurrect --target wasm32-unknown-unknown --release

PATH=/bin:/usr/bin
TERM=screen

[ -z "$TEST_TMUX" ] && TEST_TMUX=$(readlink -f ../tmux)
TMUX="$TEST_TMUX -Lresurrect-import-test"
WASM=$(dirname "$TEST_TMUX")/plugin-host/target/wasm32-unknown-unknown/release/resurrect.wasm
CAPS="-c capture-pane -c run-command -c fs-read -c fs-write -c db"
XDG_DATA_HOME=$(mktemp -d)
export XDG_DATA_HOME
DATA=$XDG_DATA_HOME/tmux/plugins/resurrect
DB=$DATA/store.db
trap '$TMUX kill-server 2>/dev/null; rm -rf "$XDG_DATA_HOME"' EXIT

fail() {
	echo "FAIL: $*" >&2
	$TMUX show-messages 2>/dev/null | tail -5 >&2
	$TMUX kill-server 2>/dev/null
	exit 1
}
q() {
	python3 -c "import sqlite3, sys
c = sqlite3.connect('file:$DB?mode=ro', uri=True)
r = c.execute(sys.argv[1]).fetchone()
print('' if r is None or r[0] is None else r[0])" "$1"
}

[ -f "$WASM" ] || fail "resurrect.wasm not built"
command -v python3 >/dev/null || fail "python3 is needed"

# --- the fixture: one session, one window, one pane, saved a while ago --
mkdir -p "$DATA"
python3 - "$DATA/state.bin" <<'EOF'
import json, struct, sys
blob = b"MARK-OLD\r\n\x1b[32mgreen\x1b[0m\r\n"
meta = {
    "version": 2,
    "saved_at_ms": 1700000000000,
    "sessions": [{
        "name": "legacy",
        "current_window_index": 0,
        "windows": [{
            "index": 0, "name": "old", "auto_rename": False,
            "width": 80, "height": 24,
            "layout": "b25d,80x24,0,0,0",
            "active_pane": 0,
            "panes": [{
                "id": 0, "floating": False, "cwd": "/tmp",
                "command": "sleep", "blob": [0, len(blob)],
            }],
        }],
    }],
}
m = json.dumps(meta).encode()
with open(sys.argv[1], "wb") as f:
    f.write(b"TMUXRES2" + struct.pack("<I", len(m)) + m + blob)
EOF
[ -f "$DATA/state.bin" ] || fail "fixture not written"

$TMUX kill-server 2>/dev/null
sleep 0.5
$TMUX -f/dev/null new-session -d -s bootstrap "sleep 600" || fail "bootstrap"
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin"
sleep 1.5

# --- the import ------------------------------------------------------------
[ -f "$DB" ] || fail "store.db was not created"
[ -e "$DATA/state.bin" ] && fail "state.bin was not renamed"
[ -f "$DATA/state.bin.imported" ] || fail "state.bin.imported is missing"
[ "$(q 'SELECT count(*) FROM snapshot')" = 1 ] ||
    fail "expected one imported row, got $(q 'SELECT count(*) FROM snapshot')"
[ "$(q 'SELECT reason FROM snapshot')" = import ] ||
    fail "reason is $(q 'SELECT reason FROM snapshot'), not import"
[ "$(q 'SELECT saved_at_ms FROM snapshot')" = 1700000000000 ] ||
    fail "the original timestamp was not kept"
[ "$(q 'SELECT session_names FROM snapshot')" = legacy ] ||
    fail "session names not recorded"
[ "$(q 'SELECT count(*) FROM pane_blob')" = 1 ] ||
    fail "expected one pane_blob row, got $(q 'SELECT count(*) FROM pane_blob')"
[ "$(q 'SELECT raw_len FROM pane_blob')" = 26 ] ||
    fail "raw_len is $(q 'SELECT raw_len FROM pane_blob'), not 26"
$TMUX show-messages | grep -q 'imported 1 old snapshot' ||
    fail "the import was not announced"

# --- restore from the imported row ----------------------------------------
$TMUX plugin-command resurrect restore
ok=
for _ in 1 2 3 4 5 6 7 8 9 10; do
	sleep 0.5
	$TMUX has-session -t legacy 2>/dev/null && ok=1 && break
done
[ -n "$ok" ] || fail "legacy session did not come back"
sleep 1
$TMUX capture-pane -p -t legacy:0.0 | grep -q MARK-OLD ||
    fail "imported pane text was not replayed"
name=$($TMUX display -p -t legacy:0 '#{window_name}')
[ "$name" = old ] || fail "window name not restored (got $name)"

# --- a second load imports nothing more -----------------------------------
$TMUX unload-plugin resurrect || fail "unload-plugin"
sleep 0.3
$TMUX load-plugin $CAPS "$WASM" || fail "load-plugin (second)"
sleep 1
[ "$(q 'SELECT count(*) FROM snapshot')" = 1 ] ||
    fail "a second load imported again ($(q 'SELECT count(*) FROM snapshot') rows)"

$TMUX kill-server 2>/dev/null
echo OK
exit 0
