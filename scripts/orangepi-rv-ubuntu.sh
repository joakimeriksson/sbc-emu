#!/bin/sh
# Canonical's Ubuntu 24.04.5 LTS preinstalled server image for JH7110 boards, unmodified, for
# the Orange Pi RV: OUT/ubuntu-24.04.5-preinstalled-server-riscv64+jh7110.img (4.5 GiB, made
# read-only; the emulator writes to a copy-on-write overlay unless --sd-persist).
#   scripts/orangepi-rv-ubuntu.sh [OUT]     (default cache/orangepi-rv/ubuntu)
# The image's own U-Boot (loader1/loader2) knows the VisionFive 2, Mars, Star64 and the
# DeepComputing board but not the Orange Pi RV, so the board boots it as the real one does:
# from the firmware in its SPI flash (scripts/orangepi-rv-uboot.sh: spi-flash.img), whose
# U-Boot starts the image's GRUB over EFI. See docs/orangepi-rv.md.
set -eu
OUT=${1:-cache/orangepi-rv/ubuntu}
IMG=ubuntu-24.04.5-preinstalled-server-riscv64+jh7110.img
URL=https://cdimage.ubuntu.com/releases/24.04.5/release/$IMG.xz
# from https://cdimage.ubuntu.com/releases/24.04.5/release/SHA256SUMS
SHA=f1e6ebe69948dabde72642b70b8f195f3844915288d78e0a600f8b5e3a5ffd6f
mkdir -p "$OUT"
if [ ! -f "$OUT/$IMG" ]; then
    if [ ! -f "$OUT/$IMG.xz" ]; then
        echo "fetching $URL (1.1 GB)" >&2
        curl -fL --retry 3 -o "$OUT/$IMG.xz.part" "$URL"
        mv "$OUT/$IMG.xz.part" "$OUT/$IMG.xz"
    fi
    echo "$SHA  $OUT/$IMG.xz" | shasum -a 256 -c
    xz -dk -T0 "$OUT/$IMG.xz"
    chmod 444 "$OUT/$IMG"
fi
ls -l "$OUT/$IMG"
