#!/bin/sh
# Build a freestanding arm64 Image for sbc-emu's Orin direct-boot loader.
set -eu
out=${1:-cache/jetson-orin-nano/copy-demo.Image}
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
clang=${CLANG:-clang}
"$clang" --target=aarch64-linux-gnu -c "$here/boot.S" -o "$work/boot.o"
"$clang" --target=aarch64-linux-gnu -ffreestanding -fno-stack-protector \
    -mno-outline-atomics -mgeneral-regs-only -O2 -Wall -Wextra -Werror \
    -c "$here/demo.c" -o "$work/demo.o"
mkdir -p "$(dirname "$out")"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
"$lld" -flavor gnu -m aarch64elf -e _start -Ttext 0x80200000 --oformat=binary \
    "$work/boot.o" "$work/demo.o" -o "$out"
