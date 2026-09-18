#!/bin/sh
# build-plugins.sh - build the wasm plugins and put their capability
# sidecars next to them, the way the release tarball does.
#
#   sh scripts/build-plugins.sh              # every plugin
#   sh scripts/build-plugins.sh agents mailbox
#
# Why this exists: the host finds a plugin's sidecar by path, as
# <stem>.toml next to the .wasm (caps.rs, load_manifest). The release
# workflow copies plugin-host/examples/*/<name>.toml into the bundle, but
# `cargo build` does not - it only writes the .wasm. A dev-tree plugin
# loaded straight from target/ therefore has no sidecar, and:
#
#   - serve_remote is readable ONLY from the sidecar, so a linked server
#     cannot call the plugin at all (mailbox stops accepting `deliver`);
#   - a STALE sidecar copied by hand is worse than none, because
#     effective caps are requests & grants - a cap missing from the
#     sidecar is intersected away with no error.
#
# So: build and copy together, always, and the two cannot drift.
set -eu

cd "$(dirname "$0")/.."
out=plugin-host/target/wasm32-unknown-unknown/release

if [ $# -gt 0 ]; then
    pkgs=""
    for p in "$@"; do pkgs="$pkgs -p $p"; done
else
    pkgs="-p notify-toast -p resurrect -p session_creator -p git-status \
          -p cron -p agents -p mailbox"
fi

# shellcheck disable=SC2086
(cd plugin-host && cargo build --release --target wasm32-unknown-unknown $pkgs)

for f in plugin-host/examples/*/*.toml; do
    case "$f" in *Cargo.toml) continue ;; esac
    cp "$f" "$out/"
    echo "sidecar $(basename "$f")"
done

ls -la "$out"/*.wasm "$out"/*.toml
