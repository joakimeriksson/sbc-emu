#!/bin/sh
# Build the Tinker Board S (RK3288) direct-boot fixture from pinned packages:
#   OUT/zImage                 Debian 13 linux-image-armmp kernel, unmodified
#   OUT/rk3288-tinker-s.dtb    the matching mainline device tree, unmodified
#   OUT/initramfs.cpio.gz      Alpine armhf minirootfs + a serial-shell /init
# Usage: scripts/tinker-board-fixture.sh [OUT]   (default cache/tinker-board)
#
# Alpine's armv7 linux-lts (the Orange Pi One kernel) is built without ARCH_ROCKCHIP and
# ships no rk3288 device trees, so the kernel comes from Debian's multiplatform armmp
# flavour (ARM mode, no LPAE; CLK_RK3288, ROCKCHIP_TIMER, PINCTRL_ROCKCHIP and
# SERIAL_8250_DW built in). Downloads are verified against SHA-256; Debian and Alpine drop
# superseded versions, so update the pins (and the README evidence) together.
set -eu

OUT=${1:-cache/tinker-board}
ALPINE=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
DEBIAN=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
ROOTFS=alpine-minirootfs-3.24.4-armhf.tar.gz
ROOTFS_URL=$ALPINE/v3.24/releases/armhf/$ROOTFS
ROOTFS_SHA=5d3c3debe55cbfca4f5b4252b0a3a7f75434bb6b6e3e1859fe917e02a4a326ed
KVER=6.12.107+deb13-armmp
KERNEL_DEB=linux-image-${KVER}_6.12.107-1_armhf.deb
KERNEL_URL=$DEBIAN/pool/main/l/linux/$KERNEL_DEB
KERNEL_SHA=9db6efc15fcc408acdc417911bdb7ac2c3ee0dd04fd1535a16b34744293a4380

mkdir -p "$OUT/downloads"
OUT=$(cd "$OUT" && pwd)   # absolute: the script changes directory below

fetch() { # file url sha256
    if [ ! -f "$OUT/downloads/$1" ]; then
        echo "fetching $2" >&2
        curl -fL --retry 3 -o "$OUT/downloads/$1.part" "$2"
        mv "$OUT/downloads/$1.part" "$OUT/downloads/$1"
    fi
    actual=$(shasum -a 256 "$OUT/downloads/$1" | cut -d' ' -f1)
    if [ "$actual" != "$3" ]; then
        echo "$1: sha256 $actual, expected $3" >&2
        exit 1
    fi
}

fetch "$ROOTFS" "$ROOTFS_URL" "$ROOTFS_SHA"
fetch "$KERNEL_DEB" "$KERNEL_URL" "$KERNEL_SHA"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A .deb is an ar archive; the kernel and device trees are in data.tar.xz.
mkdir "$work/deb"
(cd "$work/deb" && ar x "$OUT/downloads/$KERNEL_DEB" data.tar.xz)
tar -xJf "$work/deb/data.tar.xz" -C "$work/deb" "./boot/vmlinuz-$KVER" \
    "./usr/lib/linux-image-$KVER/rk3288-tinker-s.dtb"
cp "$work/deb/boot/vmlinuz-$KVER" "$OUT/zImage"
cp "$work/deb/usr/lib/linux-image-$KVER/rk3288-tinker-s.dtb" "$OUT/"

mkdir "$work/rootfs"
tar -xzf "$OUT/downloads/$ROOTFS" -C "$work/rootfs"
cat > "$work/rootfs/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t tmpfs tmpfs /tmp
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin PS1='tinker-board# '
echo "RK3288_BOOT_OK: ARM32 Linux BusyBox shell"
# setsid -c makes the console the shell's controlling terminal (job control, ^C).
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$work/rootfs/init"
(cd "$work/rootfs" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
    gzip -9n > "$OUT/initramfs.cpio.gz"

shasum -a 256 "$OUT/zImage" "$OUT/rk3288-tinker-s.dtb" "$OUT/initramfs.cpio.gz"
