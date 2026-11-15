#!/bin/sh
# Build the Orange Pi One direct-boot fixture from pinned Alpine Linux packages:
#   OUT/zImage                      Alpine linux-lts (armv7) kernel, unmodified
#   OUT/sun8i-h3-orangepi-one.dtb   the matching mainline device tree, unmodified
#   OUT/initramfs.cpio.gz           Alpine armhf minirootfs + a serial-shell /init
# Usage: scripts/orangepi-one-fixture.sh [OUT]   (default cache/orangepi-one)
# Downloads are verified against SHA-256; Alpine eventually removes superseded
# package versions, so update the pins (and the README evidence) together.
set -eu

OUT=${1:-cache/orangepi-one}
MIRROR=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
ROOTFS=alpine-minirootfs-3.24.4-armhf.tar.gz
ROOTFS_URL=$MIRROR/v3.24/releases/armhf/$ROOTFS
ROOTFS_SHA=5d3c3debe55cbfca4f5b4252b0a3a7f75434bb6b6e3e1859fe917e02a4a326ed
KERNEL_APK=linux-lts-6.18.63-r0.apk
KERNEL_URL=$MIRROR/v3.24/main/armv7/$KERNEL_APK
KERNEL_SHA=676ce55f608d0d19e18af783cadc70b80fd0da32dd01a85df589c2668daa485b

mkdir -p "$OUT/downloads"

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
fetch "$KERNEL_APK" "$KERNEL_URL" "$KERNEL_SHA"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The .apk is a sequence of gzip'd tar segments; tar reads through all of them.
mkdir "$work/kernel"
tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/kernel" boot/vmlinuz-lts \
    boot/dtbs-lts/sun8i-h3-orangepi-one.dtb 2>/dev/null ||
    tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/kernel"
cp "$work/kernel/boot/vmlinuz-lts" "$OUT/zImage"
find "$work/kernel/boot" -name sun8i-h3-orangepi-one.dtb -exec cp {} "$OUT/" \;
test -s "$OUT/sun8i-h3-orangepi-one.dtb"

mkdir "$work/rootfs"
tar -xzf "$OUT/downloads/$ROOTFS" -C "$work/rootfs"
cat > "$work/rootfs/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t tmpfs tmpfs /tmp
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin PS1='orangepi-one# '
echo "H3_BOOT_OK: ARM32 Linux BusyBox shell"
# setsid -c makes the console the shell's controlling terminal (job control, ^C).
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$work/rootfs/init"
(cd "$work/rootfs" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
    gzip -9n > "$OUT/initramfs.cpio.gz"

shasum -a 256 "$OUT/zImage" "$OUT/sun8i-h3-orangepi-one.dtb" "$OUT/initramfs.cpio.gz"
