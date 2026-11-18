#!/bin/sh
# Build pagurus for Raspberry Pi boards in Docker, enforce the size budget, and
# prove the ARMv6 build runs on the Pi Zero W's CPU (ARM1176) under QEMU.
#   scripts/build-pi.sh            aarch64 + armv6
# Needs Docker (or Colima). Outputs land in dist/.
set -eu
cd "$(dirname "$0")/.."
BUDGET=3670016 # 3.5 MiB: the binary must stay tiny (same as build-release.sh)
mkdir -p dist

echo "== aarch64 (Pi 3/4/5, 64-bit OS)"
docker run --rm -v "$PWD":/src -v pagurus-cargo:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/src/target-linux \
  -w /src rust:1-slim-bookworm sh -c 'cargo test --release -q 2>&1 | grep "test result"; cargo build --release -q'
cp target-linux/release/pagurus dist/pagurus-aarch64-linux

echo "== armv6 static (Pi Zero / Zero W / Pi 1; also runs on any 32-bit Pi OS)"
docker run --rm -v "$PWD":/src -v pagurus-cargo-armv6:/root/.cargo/registry -e CARGO_TARGET_DIR=/src/target-armv6 \
  -w /src messense/rust-musl-cross:arm-musleabihf sh -c \
  'cargo build --release -q --target arm-unknown-linux-musleabihf && cargo test --release -q --target arm-unknown-linux-musleabihf --no-run &&
   ls -t target-armv6/arm-unknown-linux-musleabihf/release/deps/pagurus-* | grep -v "\\." | head -1 > target-armv6/testbin.txt'
cp target-armv6/arm-unknown-linux-musleabihf/release/pagurus dist/pagurus-armv6-linux

echo "== ARM1176 check under QEMU (an ARMv7 instruction would SIGILL here)"
docker run --rm -v "$PWD":/src -w /src rust:1-slim-bookworm sh -c \
  'apt-get update -qq >/dev/null && apt-get install -y -qq qemu-user ca-certificates >/dev/null &&
   test -s target-armv6/testbin.txt &&
   qemu-arm -cpu arm1176 "$(cat target-armv6/testbin.txt)" 2>&1 | grep "test result" &&
   qemu-arm -cpu arm1176 dist/pagurus-armv6-linux --version'

echo "== size budget ($BUDGET bytes)"
fail=0
for f in dist/pagurus-*; do
  size=$(wc -c < "$f" | tr -d " ")
  if [ "$size" -gt "$BUDGET" ]; then echo "  ✗ $f $size"; fail=1; else echo "  ✓ $f $size"; fi
done
exit $fail
