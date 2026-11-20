#!/bin/sh
# Build pagurus for every supported platform into dist/, with checksums.
#   scripts/build-release.sh
# Needs Docker (or Colima) for Linux and Windows; macOS (Apple Silicon) is built natively
# when run on a Mac. Linux builds are static (musl): one file, no dependencies.
set -eu
cd "$(dirname "$0")/.."
BUDGET=3670016      # 3.5 MiB: the small build must stay tiny (x86_64 is the largest)
FULL_BUDGET=10485760 # 10 MiB: "pagurus uge" (--features uge: SQLite FTS5, the art tool, …)
rm -rf dist && mkdir -p dist

musl() { # image target output [features]
  echo "== $3"
  docker run --rm -v "$PWD":/src -v "pagurus-cargo-$1":/root/.cargo/registry -e CARGO_TARGET_DIR=/src/target-x/"$1" \
    -w /src "messense/rust-musl-cross:$1" cargo build --release -q --target "$2" ${4:+--features "$4"}
  cp "target-x/$1/$2/release/pagurus" "dist/$3"
}

echo "== tests (Linux aarch64)"
docker run --rm -v "$PWD":/src -v pagurus-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/src/target-linux \
  -w /src rust:1-slim-bookworm sh -c 'cargo test --release -q 2>&1 | grep "test result" &&
    cargo test --release -q --features full 2>&1 | grep "test result"'

musl x86_64-musl        x86_64-unknown-linux-musl       pagurus-linux-x86_64
musl aarch64-musl       aarch64-unknown-linux-musl      pagurus-linux-arm64
musl armv7-musleabihf   armv7-unknown-linux-musleabihf  pagurus-linux-armv7
musl arm-musleabihf     arm-unknown-linux-musleabihf    pagurus-linux-armv6
musl aarch64-musl       aarch64-unknown-linux-musl      pagurus-full-linux-arm64 full

echo "== ARM1176 (Pi Zero W) check under QEMU: an ARMv7 instruction would SIGILL here"
docker run --rm -v "$PWD":/src -v pagurus-cargo-arm-musleabihf:/root/.cargo/registry -e CARGO_TARGET_DIR=/src/target-x/arm-musleabihf \
  -w /src messense/rust-musl-cross:arm-musleabihf sh -c \
  'cargo test --release -q --target arm-unknown-linux-musleabihf --no-run &&
   ls -t target-x/arm-musleabihf/arm-unknown-linux-musleabihf/release/deps/pagurus-* | grep -v "\." | head -1 > target-x/armv6-testbin.txt'
docker run --rm -v "$PWD":/src -w /src rust:1-slim-bookworm sh -c \
  'apt-get update -qq >/dev/null && apt-get install -y -qq qemu-user >/dev/null 2>&1 &&
   qemu-arm -cpu arm1176 "$(cat target-x/armv6-testbin.txt)" 2>&1 | grep "test result" &&
   qemu-arm -cpu arm1176 dist/pagurus-linux-armv6 --version'

echo "== pagurus-windows-x86_64.exe"
docker run --rm -v "$PWD":/src -v pagurus-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/src/target-x/win \
  -w /src rust:1-slim-bookworm sh -c \
  'apt-get update -qq >/dev/null && apt-get install -y -qq gcc-mingw-w64-x86-64 >/dev/null 2>&1 &&
   rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1 &&
   cargo build --release -q --target x86_64-pc-windows-gnu'
cp target-x/win/x86_64-pc-windows-gnu/release/pagurus.exe dist/pagurus-windows-x86_64.exe

if [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ]; then
  echo "== pagurus-macos-arm64"
  cargo build --release -q
  cp target/release/pagurus dist/pagurus-macos-arm64
  echo "== pagurus-full-macos-arm64"
  cargo build --release -q --features full
  cp target/release/pagurus dist/pagurus-full-macos-arm64
fi

echo "== size budget ($BUDGET bytes, full builds $FULL_BUDGET) and checksums"
fail=0
for f in dist/pagurus-*; do
  size=$(wc -c < "$f" | tr -d " ")
  case "$f" in dist/pagurus-full-*) max=$FULL_BUDGET ;; *) max=$BUDGET ;; esac
  if [ "$size" -gt "$max" ]; then echo "  ✗ $f $size"; fail=1; else echo "  ✓ $f $size"; fi
done
(cd dist && shasum -a 256 pagurus-* > SHA256SUMS 2>/dev/null || sha256sum pagurus-* > SHA256SUMS)
exit $fail
