#!/usr/bin/env bash
set -euo pipefail

# Install media-convert (CLI + GUI) to ~/.local/bin by default.
# Override with: INSTALL_ROOT=/some/prefix ./install.sh
# (binaries land in $INSTALL_ROOT/bin)

INSTALL_ROOT="${INSTALL_ROOT:-$HOME/.local}"
BIN_DIR="$INSTALL_ROOT/bin"

cd "$(dirname "$0")"

echo "==> Building and installing to $BIN_DIR"
cargo install --path . --root "$INSTALL_ROOT" --locked --force

echo
echo "==> Installed:"
for b in media-convert media-convert-gui; do
    if [[ -x "$BIN_DIR/$b" ]]; then
        printf '    %s\n' "$BIN_DIR/$b"
    fi
done

case ":$PATH:" in
    *":$BIN_DIR:"*)
        echo
        echo "==> $BIN_DIR is on your PATH. You can run:"
        echo "      media-convert --help"
        echo "      media-convert-gui"
        ;;
    *)
        echo
        echo "==> WARNING: $BIN_DIR is not on your PATH."
        echo "    Add it to your shell rc (e.g. ~/.bashrc or ~/.zshrc):"
        echo
        echo "      export PATH=\"$BIN_DIR:\$PATH\""
        ;;
esac
