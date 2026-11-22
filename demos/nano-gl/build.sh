#!/bin/sh
# Build the demos for aarch64 Linux with rustc's own linker: demos/nano-gl/build.sh LIBS OUT
# LIBS is a directory with the guest's shared libraries (Debian's /usr/lib/aarch64-linux-gnu).
set -eu
LIBS=$(cd "$1" && pwd)
OUT=$2
cd "$(dirname "$0")"
T=aarch64-unknown-linux-gnu
RUSTC="$(rustup which rustc)"; export RUSTC
rustup target list --installed | grep -qx $T || rustup target add $T
# rustc's linker finds its own libraries through this (macOS; a Homebrew cargo would not set it)
DYLD_FALLBACK_LIBRARY_PATH="$(dirname "$(dirname "$RUSTC")")/lib"; export DYLD_FALLBACK_LIBRARY_PATH
mkdir -p target/libs
# the names the linker asks for (glibc has them all in libc by now)
for l in c m; do ln -sf "$LIBS/lib$l.so.6" target/libs/lib$l.so; done
for l in util rt pthread dl; do ln -sf "$LIBS/libc.so.6" target/libs/lib$l.so; done
for l in gcc_s.so.1 EGL.so.1 GLESv2.so.2 gbm.so.1 drm.so.2; do ln -sf "$LIBS/lib$l" "target/libs/lib${l%%.*}.so"; done
RUSTFLAGS="-C linker=rust-lld -C linker-flavor=ld.lld -C link-arg=-L$PWD/target/libs -C link-arg=--allow-shlib-undefined -C link-arg=--dynamic-linker=/lib/ld-linux-aarch64.so.1" \
    "$(rustup which cargo)" build -q --release --target $T
cp target/$T/release/nano-gl "$OUT"
