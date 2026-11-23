#!/bin/sh
# Raspberry Pi OS Lite (64-bit, Debian 13 "trixie", 2026-10-06), unmodified: download, verify
# (SHA-256 published by Raspberry Pi), decompress to cache/os/raspios-lite.img. Boot it with
#   sbc-emu run --sd cache/os/raspios-lite.img --input usb --vnc :1
# Usage: scripts/raspios-fixture.sh [OUT]   (default cache/os)
set -eu
OUT=${1:-cache/os}
XZ=2026-10-06-raspios-trixie-arm64-lite.img.xz
URL=https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/$XZ
SHA=483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843
mkdir -p "$OUT"
[ -f "$OUT/raspios-lite.img.xz" ] || { curl -fL --retry 3 -o "$OUT/raspios-lite.img.xz.part" "$URL" && mv "$OUT/raspios-lite.img.xz.part" "$OUT/raspios-lite.img.xz"; }
actual=$(shasum -a 256 "$OUT/raspios-lite.img.xz" | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || { echo "$OUT/raspios-lite.img.xz: sha256 $actual, expected $SHA (delete it to fetch it again)" >&2; exit 1; }
[ -f "$OUT/raspios-lite.img" ] || { xz -dc "$OUT/raspios-lite.img.xz" > "$OUT/raspios-lite.img.part" && mv "$OUT/raspios-lite.img.part" "$OUT/raspios-lite.img"; }
echo "$OUT/raspios-lite.img"
