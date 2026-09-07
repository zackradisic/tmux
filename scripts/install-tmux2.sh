#!/bin/sh
# install-tmux2.sh - install or update the tmux2 fork from a GitHub release.
#
#   sh install-tmux2.sh            # latest release
#   sh install-tmux2.sh <tag>      # a specific release tag
#
# Lays out, without touching a stock tmux on the host:
#   $TMUX2_HOME/bin/tmux        the fork binary   (default ~/.local/share/tmux2)
#   $TMUX2_HOME/plugins/        the bundled wasm plugins and their sidecars
#   ~/.local/bin/tmux2          wrapper: runs the fork on its own socket
#                               (-L $TMUX2_SOCKET, default "tmux2") with
#                               $TMUX2_HOME/bin first in PATH, so `tmux`
#                               inside a tmux2 session is the fork too.
#
# Point ~/.tmux/plugins.toml at $TMUX2_HOME/plugins/<name>.wasm.
set -eu

REPO="${TMUX2_REPO:-zackradisic/tmux}"
HOME_DIR="${TMUX2_HOME:-$HOME/.local/share/tmux2}"
BIN_DIR="${TMUX2_BIN_DIR:-$HOME/.local/bin}"
TAG="${1:-}"

os=$(uname -s); arch=$(uname -m)
case "$os-$arch" in
    Linux-x86_64)  platform=linux-x86_64 ;;
    Linux-aarch64) platform=linux-aarch64 ;;
    Darwin-arm64)  platform=darwin-arm64 ;;
    *) echo "install-tmux2: no release build for $os $arch" >&2; exit 1 ;;
esac

if [ -z "$TAG" ]; then
    TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
    [ -n "$TAG" ] || { echo "install-tmux2: cannot resolve latest release" >&2; exit 1; }
fi
base="https://github.com/$REPO/releases/download/$TAG"

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
echo "tmux2 $TAG ($platform) -> $HOME_DIR"
curl -fsSL -o "$tmp/bin.tar.gz"     "$base/tmux2-$platform.tar.gz"
curl -fsSL -o "$tmp/plugins.tar.gz" "$base/tmux2-plugins.tar.gz"

mkdir -p "$HOME_DIR" "$BIN_DIR"
# Unpack beside the live files, then swap: a running server keeps its
# old inode, and restart-server picks up the new binary at the same path.
rm -rf "$HOME_DIR/bin.new" "$HOME_DIR/plugins.new"
tar -C "$tmp" -xzf "$tmp/bin.tar.gz";     mv "$tmp/bin"     "$HOME_DIR/bin.new"
tar -C "$tmp" -xzf "$tmp/plugins.tar.gz"; mv "$tmp/plugins" "$HOME_DIR/plugins.new"
rm -rf "$HOME_DIR/bin.old" "$HOME_DIR/plugins.old"
[ -d "$HOME_DIR/bin" ]     && mv "$HOME_DIR/bin"     "$HOME_DIR/bin.old"
[ -d "$HOME_DIR/plugins" ] && mv "$HOME_DIR/plugins" "$HOME_DIR/plugins.old"
mv "$HOME_DIR/bin.new" "$HOME_DIR/bin"; mv "$HOME_DIR/plugins.new" "$HOME_DIR/plugins"
rm -rf "$HOME_DIR/bin.old" "$HOME_DIR/plugins.old"
printf '%s\n' "$TAG" > "$HOME_DIR/VERSION"

wrapper="$BIN_DIR/tmux2"
if [ ! -e "$wrapper" ]; then
    cat > "$wrapper" <<EOF
#!/bin/sh
# tmux2 - the tmux fork with wasm plugins, on its own socket so a stock
# tmux on this host is untouched. Installed by install-tmux2.sh.
TMUX2_HOME="\${TMUX2_HOME:-$HOME_DIR}"
export PATH="\$TMUX2_HOME/bin:\$PATH"
exec "\$TMUX2_HOME/bin/tmux" -L "\${TMUX2_SOCKET:-tmux2}" "\$@"
EOF
    chmod +x "$wrapper"
    echo "created $wrapper"
fi

"$HOME_DIR/bin/tmux" -V
ls "$HOME_DIR/plugins"
case ":$PATH:" in *":$BIN_DIR:"*) ;; *) echo "note: add $BIN_DIR to PATH" ;; esac
