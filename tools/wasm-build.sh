#!/bin/sh
# Build the WebAssembly emulator into web/wasm/sbc-emu.wasm and stage the demo guest in web/assets.
#   tools/wasm-build.sh ; python3 -m http.server -d web 8791 ; open http://127.0.0.1:8791/
set -e
cd "$(dirname "$0")/.."
# a non-rustup cargo/rustc on PATH (Homebrew) has no wasm32 std: build with rustup's toolchain
RUSTC="$(rustup which rustc)"; export RUSTC
CARGO="$(rustup which cargo)"
DYLD_FALLBACK_LIBRARY_PATH="$(dirname "$(dirname "$RUSTC")")/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"; export DYLD_FALLBACK_LIBRARY_PATH
RUSTFLAGS="-C target-feature=+bulk-memory,+simd128 -C link-arg=--max-memory=4294967296" \
  "$CARGO" build --profile wasm --target wasm32-unknown-unknown -p sbc-emu-wasm
mkdir -p web/wasm web/assets
cp target/wasm32-unknown-unknown/wasm/sbc_emu_wasm.wasm web/wasm/sbc-emu.wasm
ls -la web/wasm/sbc-emu.wasm
# guest: the pinned Pi kernel (gzip, decompressed in the page) and the busybox initramfs
[ -f cache/fw/kernel8.img ] && cp cache/fw/kernel8.img web/assets/kernel8.img
[ -f cache/initramfs.cpio ] && gzip -9 -c cache/initramfs.cpio > web/assets/initramfs.cpio.gz
ls -la web/assets
