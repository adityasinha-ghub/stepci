#!/bin/sh
# Install a prebuilt stepci binary from the latest GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/adityasinha-ghub/stepci/main/install.sh | sh
#
# Override the install location with STEPCI_BIN_DIR (default: ~/.local/bin).
set -eu

REPO="adityasinha-ghub/stepci"
BIN_DIR="${STEPCI_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) echo "stepci: unsupported macOS arch '$arch' — build from source with cargo." >&2; exit 1 ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-gnu" ;;
      *) echo "stepci: unsupported Linux arch '$arch' — build from source with cargo." >&2; exit 1 ;;
    esac ;;
  *)
    echo "stepci: unsupported OS '$os' — build from source with cargo." >&2; exit 1 ;;
esac

url="https://github.com/$REPO/releases/latest/download/stepci-$target.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "stepci: downloading $target …"
if ! curl -fsSL "$url" -o "$tmp/stepci.tar.gz"; then
  echo "stepci: download failed (no release yet?). Try: cargo install --git https://github.com/$REPO" >&2
  exit 1
fi

tar -C "$tmp" -xzf "$tmp/stepci.tar.gz"
mkdir -p "$BIN_DIR"
install -m 755 "$tmp/stepci" "$BIN_DIR/stepci"

echo "stepci: installed to $BIN_DIR/stepci"
case ":$PATH:" in
  *":$BIN_DIR:"*) : ;;
  *) echo "stepci: add $BIN_DIR to your PATH to run 'stepci'." ;;
esac
