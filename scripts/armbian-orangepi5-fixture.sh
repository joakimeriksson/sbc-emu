#!/bin/sh
# Armbian for the Orange Pi 5, unmodified: download, verify (SHA-256 published by Armbian next to
# the image), decompress. Boot it with
#   sbc-emu run --board orangepi5 --sd cache/armbian/Armbian_26.8.1_Orangepi5_trixie_current_6.18.43_minimal.img --ram-mb 1024
# Usage: scripts/armbian-orangepi5-fixture.sh [OUT]   (default cache/armbian)
set -eu
OUT=${1:-cache/armbian}
IMG=Armbian_26.8.1_Orangepi5_trixie_current_6.18.43_minimal.img
URL=${ARMBIAN_ARCHIVE:-https://archive.armbian.com}/orangepi5/archive/$IMG.xz
SHA=a7633627a54b6d8c5f03294cbf1b3b998d55b8e1e15cfb5296a3c440b981fcb5
mkdir -p "$OUT"
[ -f "$OUT/$IMG.xz" ] || curl -fL --retry 3 -o "$OUT/$IMG.xz" "$URL"
actual=$(shasum -a 256 "$OUT/$IMG.xz" | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || { echo "$IMG.xz: sha256 $actual, expected $SHA" >&2; exit 1; }
[ -f "$OUT/$IMG" ] || xz -dk "$OUT/$IMG.xz"
echo "$OUT/$IMG"
