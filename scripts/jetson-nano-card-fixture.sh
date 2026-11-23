#!/bin/sh
# The Jetson Nano's microSD card: Debian 13's arm64 "nocloud" image, unmodified: download, verify
# (SHA-512 from cloud.debian.org's SHA512SUMS), unpack to OUT/disk.raw (3 GiB). It boots with the
# kernel and initramfs of scripts/jetson-nano-debian-fixture.sh; the command line is in
# scripts/jetson-nano-debian.txt and docs/getting-started.md.
# Usage: scripts/jetson-nano-card-fixture.sh [OUT]   (default cache/jetson-nano/debian)
set -eu
OUT=${1:-cache/jetson-nano/debian}
TXZ=debian-13-nocloud-arm64-20261116-2664.tar.xz
URL=https://cloud.debian.org/images/cloud/trixie/20261116-2664/$TXZ
SHA=9e2f39589c5eac8d727099e42dba91746231dfe425995c6a90b6a541735d6776c5b08f71ee6841f0b0f31080eddfb2f09998a0e0553292dbdce9994c405fb0b1
mkdir -p "$OUT"
[ -f "$OUT/$TXZ" ] || { curl -fL --retry 3 -o "$OUT/$TXZ.part" "$URL" && mv "$OUT/$TXZ.part" "$OUT/$TXZ"; }
actual=$(shasum -a 512 "$OUT/$TXZ" | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || { echo "$TXZ: sha512 $actual, expected $SHA" >&2; exit 1; }
[ -f "$OUT/disk.raw" ] || tar -xJf "$OUT/$TXZ" -C "$OUT" disk.raw
echo "$OUT/disk.raw"
