#!/bin/sh
# ASAN pass for the plugin ABI's C borrow contract: build tmux with
# AddressSanitizer against the DEBUG plugin-host staticlib (debug builds
# replace every borrowed pointer handed to C with a copy freed when the
# import call ends, so any C code stashing one past the call is a
# heap-use-after-free ASAN reports deterministically), then run the
# resurrect roundtrip and check for reports.
#
# handle_segv=0 is required: wasmtime traps wasm out-of-bounds through
# its own SIGSEGV handler (guard pages); ASAN must not steal the signal.
# detect_leaks=0: tmux frees little at exit by design; leaks are not
# what this pass is for.
#
# Run from the repo root. Rebuilds ./tmux with ASAN, restores the normal
# binary afterwards.

set -e
cd "$(dirname "$0")/.."

echo "== building debug staticlib (tripwire active)"
(cd plugin-host && cargo build -p plugin-host >/dev/null)

echo "== building ASAN tmux"
[ -f tmux ] && cp tmux tmux-preasan.bak
touch ./*.c
make -j"$(nproc)" \
    CFLAGS="-std=gnu99 -fsanitize=address -O1 -g -fno-omit-frame-pointer" \
    LDFLAGS="-fsanitize=address" \
    PLUGIN_HOST_LIB=plugin-host/target/debug/libplugin_host.a >/dev/null
cp tmux tmux-asan

LOGS=$(mktemp -d)
export ASAN_OPTIONS="handle_segv=0:handle_sigbus=0:handle_sigfpe=0:detect_leaks=0:log_path=$LOGS/asan"

echo "== resurrect roundtrip under ASAN"
TEST_TMUX=$PWD/tmux-asan sh regress/plugin-resurrect.sh

echo "== restoring normal build"
touch ./*.c
make -j"$(nproc)" >/dev/null 2>&1 || { [ -f tmux-preasan.bak ] && cp tmux-preasan.bak tmux; }
rm -f tmux-preasan.bak

if ls "$LOGS"/asan.* >/dev/null 2>&1; then
	echo "ASAN REPORTS FOUND:"
	cat "$LOGS"/asan.*
	exit 1
fi
rm -rf "$LOGS"
echo "ASAN pass clean"
