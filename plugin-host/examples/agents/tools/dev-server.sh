#!/bin/sh
# Try the agents plugin from this tree without touching the live server:
# a second tmux server (its own socket) running this tree's tmux, with
# this tree's agents.wasm loaded, on a COPY of the live store. The copy
# is migrated to this plugin's schema, so the live server's older plugin
# never sees it; the live -L tmux2 server and its db are left alone.
#
#   sh plugin-host/examples/agents/tools/dev-server.sh   # from the tree root
#   tmux -L search-dev attach                             # this tree's tmux
#   prefix a                                              # the picker
#
# Needs: ./tmux built here (configure --enable-plugins), and
#   cargo build -p agents --target wasm32-unknown-unknown --release
# DATA=... picks another data dir; delete it to start from a fresh copy.
set -e
ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
T="$ROOT/tmux"
SOCK=${SOCK:-search-dev}
WASM="$ROOT/plugin-host/target/wasm32-unknown-unknown/release/agents.wasm"
DATA=${DATA:-$HOME/.local/share/tmux2-search-dev}
[ -x "$T" ] || { echo "no tmux binary at $T (build the tree first)" >&2; exit 1; }
[ -f "$WASM" ] || { echo "no agents.wasm at $WASM (cargo build -p agents --target wasm32-unknown-unknown --release)" >&2; exit 1; }
mkdir -p "$DATA/tmux/plugins/agents"
if [ ! -f "$DATA/tmux/plugins/agents/store.db" ]; then
  LIVE=${LIVE_STORE:-$HOME/.local/share/tmux/plugins/agents/store.db}
  if [ -f "$LIVE" ]; then
    cp "$LIVE" "$DATA/tmux/plugins/agents/store.db"
    echo "copied the live store ($LIVE) to $DATA; the live one is untouched"
  else
    echo "no live store at $LIVE; starting empty (LIVE_STORE=... to point at one)"
  fi
fi
DEPLOY="$DATA/deploy"; mkdir -p "$DEPLOY"
cp "$WASM" "$DEPLOY/agents.wasm"
cp "$ROOT/plugin-host/examples/agents/agents.toml" "$DEPLOY/agents.toml"
if $T -L $SOCK kill-server 2>/dev/null; then
  # The server takes a moment to go; a new-session that races it dies.
  sleep 1
fi
XDG_DATA_HOME="$DATA" $T -L $SOCK -f /dev/null new-session -d -s dev -x 220 -y 50
XDG_DATA_HOME="$DATA" $T -L $SOCK load-plugin -s server -c capture-pane -c run-command -c mode -c db \
  -c env-read -c pane-fds -c fs-read -c fs-list -c service-serve -c service-call -c send-keys \
  -c run-process -c fs-read-any "$DEPLOY/agents.wasm"
$T -L $SOCK bind-key a run-shell "$T -L $SOCK plugin-command agents pick"
echo "server up: attach with   $T -L $SOCK attach"
echo "picker: prefix a   (or: $T -L $SOCK plugin-command agents pick)"
