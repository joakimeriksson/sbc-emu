#!/bin/sh
# Armbian for the ASUS Tinker Board (RK3288), unmodified: download, verify (SHA-256 published
# by Armbian next to the image, Armbian_26.8.1_..._minimal.img.xz.sha), decompress. Boot it with
#   sbc-emu run --board tinkerboard --sd cache/tinker-board-armbian/Armbian_26.8.1_Tinkerboard_trixie_current_6.18.43_minimal.img
# Usage: scripts/armbian-tinkerboard-fixture.sh [OUT]   (default cache/tinker-board-armbian)
set -eu
OUT=${1:-cache/tinker-board-armbian}
IMG=Armbian_26.8.1_Tinkerboard_trixie_current_6.18.43_minimal.img
URL=${ARMBIAN_MIRROR:-https://dl.armbian.com}/tinkerboard/archive/$IMG.xz
SHA=cd9a7e3fe6b4b7f25612df33f807674819a736370d65ea4465fd92735024ae18
mkdir -p "$OUT"
if [ ! -f "$OUT/$IMG.xz" ]; then
  curl -fL --retry 3 -o "$OUT/$IMG.xz.part" "$URL"
  mv "$OUT/$IMG.xz.part" "$OUT/$IMG.xz"
fi
actual=$(shasum -a 256 "$OUT/$IMG.xz" | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || { echo "$IMG.xz: sha256 $actual, expected $SHA (delete it to fetch it again)" >&2; exit 1; }
if [ ! -f "$OUT/$IMG" ]; then
  xz -dc "$OUT/$IMG.xz" > "$OUT/$IMG.part"
  mv "$OUT/$IMG.part" "$OUT/$IMG"
fi
echo "$OUT/$IMG"
