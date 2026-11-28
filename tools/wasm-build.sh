#!/bin/sh
# Build the WebAssembly emulator into web/wasm/sbc-emu.wasm and stage the demo guest in web/assets.
#   tools/wasm-build.sh ; python3 -m http.server -d web 8791 ; open http://127.0.0.1:8791/
set -e
cd "$(dirname "$0")/.."
# a non-rustup cargo/rustc on PATH (Homebrew) has no wasm32 std: build with rustup's toolchain
RUSTC="$(rustup which rustc)"; export RUSTC
CARGO="$(rustup which cargo)"
DYLD_FALLBACK_LIBRARY_PATH="$(dirname "$(dirname "$RUSTC")")/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"; export DYLD_FALLBACK_LIBRARY_PATH
RUSTFLAGS="-C target-feature=+bulk-memory,+simd128 -C link-arg=--max-memory=4294967296 -C link-arg=--export-table -C link-arg=--growable-table" \
  "$CARGO" build --profile wasm --target wasm32-unknown-unknown -p sbc-emu-wasm
mkdir -p web/wasm web/assets
cp target/wasm32-unknown-unknown/wasm/sbc_emu_wasm.wasm web/wasm/sbc-emu.wasm
ls -la web/wasm/sbc-emu.wasm
# guest: the pinned Pi kernel (gzip, decompressed in the page) and the busybox initramfs
[ -f cache/fw/kernel8.img ] && cp cache/fw/kernel8.img web/assets/kernel8.img
[ -f cache/initramfs.cpio ] && gzip -9 -c cache/initramfs.cpio > web/assets/initramfs.cpio.gz
# the Raspberry Pi 3's BusyBox initramfs (its shell on the mini UART: mkinitramfs --board pi3b)
# for web/rpi3.html, with the same kernel8.img; and the Pi OS card both Pi pages boot, if present
if [ -f cache/rpi3/initramfs.cpio ]; then mkdir -p web/assets/rpi3; gzip -9n -c cache/rpi3/initramfs.cpio > web/assets/rpi3/initramfs.cpio.gz; fi
[ -f cache/os/raspios-lite.img ] && [ ! -e web/assets/raspios-lite.img ] && ln -s ../../cache/os/raspios-lite.img web/assets/raspios-lite.img
# the Tinker Board's quick demo (scripts/tinker-board-fixture.sh) and, if present, its
# Armbian card (scripts/armbian-tinkerboard-fixture.sh) for web/tinker.html
if [ -f cache/tinker-board/zImage ]; then
  mkdir -p web/assets/tinker
  cp cache/tinker-board/zImage cache/tinker-board/rk3288-tinker-s.dtb cache/tinker-board/initramfs.cpio.gz web/assets/tinker/
fi
armbian=$(ls cache/tinker-board-armbian/Armbian_*_Tinkerboard_*.img 2>/dev/null | head -n1)
[ -n "$armbian" ] && [ ! -e web/assets/tinker-armbian.img ] && ln -s "../../$armbian" web/assets/tinker-armbian.img
# the Orange Pi One's demo (scripts/orangepi-one-fixture.sh) for web/board.html
if [ -f cache/orangepi-one/zImage ]; then
  mkdir -p web/assets/orangepi-one
  cp cache/orangepi-one/zImage cache/orangepi-one/sun8i-h3-orangepi-one.dtb cache/orangepi-one/initramfs.cpio.gz web/assets/orangepi-one/
fi
# the Orange Pi RV's demo (scripts/orangepi-rv-fixture.sh) for web/board.html?b=orangepi-rv
if [ -f cache/orangepi-rv/web/initramfs.cpio.gz ]; then
  mkdir -p web/assets/orangepi-rv
  cp cache/orangepi-rv/fw_dynamic.bin cache/orangepi-rv/jh7110-orangepi-rv.dtb web/assets/orangepi-rv/
  cp cache/orangepi-rv/web/initramfs.cpio.gz web/assets/orangepi-rv/
  gzip -9n -c cache/orangepi-rv/Image > web/assets/orangepi-rv/Image.gz
fi
# the Orange Pi 5's demo (scripts/orangepi5-fixture.sh) for web/board.html?b=orangepi5
if [ -f cache/orangepi5/Image.gz ]; then
  mkdir -p web/assets/orangepi5
  cp cache/orangepi5/Image.gz cache/orangepi5/initramfs.cpio.gz web/assets/orangepi5/
fi
# the Jetson Nano's demo (scripts/jetson-nano-debian-fixture.sh) for web/board.html?b=jetson-nano
if [ -f cache/jetson-nano/debian/initramfs.cpio.gz ]; then
  mkdir -p web/assets/jetson-nano
  cp cache/jetson-nano/debian/tegra210-p3450-0000.dtb cache/jetson-nano/debian/initramfs.cpio.gz web/assets/jetson-nano/
  gzip -9n -c cache/jetson-nano/debian/vmlinuz > web/assets/jetson-nano/Image.gz
  # and its GPU demos (scripts/jetson-nano-gpu-demo-fixture.sh) for web/board.html?b=jetson-nano-gpu
  if [ -f cache/jetson-nano/gpu-demo/gpu-demo.cpio.xz ]; then cp cache/jetson-nano/gpu-demo/gpu-demo.cpio.xz web/assets/jetson-nano/; fi
fi
# the Jetson Orin Nano's rover demo (demos/orin-copy/build.sh)
if [ -f cache/jetson-orin-nano/copy-demo.Image ]; then
  mkdir -p web/assets/jetson-orin-nano-4gb
  cp cache/jetson-orin-nano/copy-demo.Image web/assets/jetson-orin-nano-4gb/
fi
# the ESP32-S31's demo (scripts/esp32s31-fixture.sh) for web/board.html?b=esp32s31
if [ -f cache/esp32s31/images/s31_full_flash.bin ]; then
  mkdir -p web/assets/esp32s31
  cp cache/esp32s31/esp32s31_rev0_rom.elf web/assets/esp32s31/
  gzip -9n -c cache/esp32s31/images/s31_full_flash.bin > web/assets/esp32s31/s31_full_flash.bin.gz
fi
ls -la web/assets
