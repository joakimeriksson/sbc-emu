#!/bin/sh
# Armbian for the Orange Pi One, unmodified: download, verify (SHA-256 published by
# Armbian), decompress. Boot it with
#   sbc-emu run --board orangepi-one --sd cache/armbian/Armbian_26.8.1_Orangepione_trixie_current_6.18.44_minimal.img
# Usage: scripts/armbian-orangepi-one-fixture.sh [OUT]   (default cache/armbian)
set -eu
OUT=${1:-cache/armbian}
IMG=Armbian_26.8.1_Orangepione_trixie_current_6.18.44_minimal.img
URL=${ARMBIAN_MIRROR:-https://dl.armbian.com}/orangepione/archive/$IMG.xz
SHA=7babe415b7e55e0554572af24d0177d1f69b41388d0f3ab11357abbcd790a1a6
mkdir -p "$OUT"
[ -f "$OUT/$IMG.xz" ] || curl -fL --retry 3 -o "$OUT/$IMG.xz" "$URL"
actual=$(shasum -a 256 "$OUT/$IMG.xz" | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || { echo "$IMG.xz: sha256 $actual, expected $SHA" >&2; exit 1; }
[ -f "$OUT/$IMG" ] || xz -dk "$OUT/$IMG.xz"
echo "$OUT/$IMG"
