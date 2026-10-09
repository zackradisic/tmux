#!/bin/sh
# gen-stock-notes.sh - regenerate the palette's table of tmux's own
# binding notes from key-bindings.c (the prefix table only).
set -eu
cd "$(dirname "$0")/.."
out=plugin-host/examples/palette/src/stock.rs
{
    echo '//! The notes tmux puts on its own prefix-table bindings (key-bindings.c),'
    echo "//! so the palette can tell a stock binding from one of the user's. Generated"
    echo '//! by scripts/gen-stock-notes.sh; do not edit.'
    echo
    echo 'pub const STOCK_NOTES: &[&str] = &['
    awk '/static const char \*const defaults/,/^\t};/' key-bindings.c |
        grep "bind -N" |
        sed -E "s/.*bind -N '([^']*)'.*/\1/" | sort -u |
        sed 's/\\/\\\\/g; s/"/\\"/g; s/.*/    "&",/'
    echo '];'
} > "$out"
echo "wrote $out"
