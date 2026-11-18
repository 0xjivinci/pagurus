#!/bin/sh
# Install pagurus from the GitHub releases.
#   curl -fsSL https://raw.githubusercontent.com/0xjivinci/pagurus/main/install.sh | sh
# Options (environment): PAGURUS_VERSION=v0.1.0  PAGURUS_BIN_DIR=/usr/local/bin  PAGURUS_REPO=owner/name
set -eu
REPO="${PAGURUS_REPO:-0xjivinci/pagurus}"
VERSION="${PAGURUS_VERSION:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'pagurus install: %s\n' "$*" >&2; exit 1; }

# https only, also across redirects (GitHub sends release downloads to another host)
if command -v curl >/dev/null 2>&1; then fetch() { curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then fetch() { wget -q --https-only -O "$2" "$1"; }
else die "needs curl or wget"; fi
case "$REPO" in */*) ;; *) die "PAGURUS_REPO must look like owner/name" ;; esac

os=$(uname -s); arch=$(uname -m)
case "$os/$arch" in
  Linux/x86_64|Linux/amd64)   asset=pagurus-linux-x86_64 ;;
  Linux/aarch64|Linux/arm64)  asset=pagurus-linux-arm64 ;;
  Linux/armv7l|Linux/armv8l)  asset=pagurus-linux-armv7 ;;
  Linux/armv6l)               asset=pagurus-linux-armv6 ;;
  Darwin/arm64)               asset=pagurus-macos-arm64 ;;
  Darwin/x86_64)              die "no Intel-Mac build yet: clone the repo and run 'cargo build --release'" ;;
  *)                          die "no build for $os/$arch yet" ;;
esac

if [ -n "${PAGURUS_BIN_DIR:-}" ]; then dir=$PAGURUS_BIN_DIR
elif [ -w /usr/local/bin ]; then dir=/usr/local/bin
else dir="$HOME/.local/bin"; fi
mkdir -p "$dir"

if [ -n "$VERSION" ]; then base="https://github.com/${REPO}/releases/download/${VERSION}"
else base="https://github.com/${REPO}/releases/latest/download"; fi

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
say "downloading ${asset} ${VERSION:-(latest)} from ${REPO}..."
fetch "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "couldn't download the checksums"

want=$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS" | head -n 1 | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch for $asset; not installing"

chmod +x "$tmp/$asset"
mv "$tmp/$asset" "$dir/pagurus"
"$dir/pagurus" --version >/dev/null 2>&1 || die "installed, but $dir/pagurus doesn't run on this machine"
say "installed $("$dir/pagurus" --version) → $dir/pagurus"
case ":$PATH:" in *":$dir:"*) ;; *) say "add $dir to your PATH:  export PATH=\"$dir:\$PATH\"" ;; esac
say "next: pagurus setup"
