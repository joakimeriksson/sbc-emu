#!/bin/sh
# Build the Jetson Nano direct-boot fixture from pinned Alpine Linux packages:
#   OUT/vmlinuz                     Alpine linux-lts (aarch64) kernel, unmodified (EFI zboot)
#   OUT/tegra210-p3450-0000.dtb     the matching mainline device tree, unmodified
#   OUT/initramfs.cpio.gz           Alpine aarch64 minirootfs + a serial-shell /init
# Usage: scripts/jetson-nano-fixture.sh [OUT]   (default cache/jetson-nano)
# Downloads are verified against SHA-256; Alpine eventually removes superseded
# package versions, so update the pins together.
set -eu

OUT=${1:-cache/jetson-nano}
MIRROR=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
ROOTFS=alpine-minirootfs-3.24.4-aarch64.tar.gz
ROOTFS_URL=$MIRROR/v3.24/releases/aarch64/$ROOTFS
ROOTFS_SHA=4b14c18f7e90a063b61462ea1462a5732c154bf6adad594974fe6487cd98a8de
KERNEL_APK=linux-lts-6.18.65-r0.apk
KERNEL_URL=$MIRROR/v3.24/main/aarch64/$KERNEL_APK
KERNEL_SHA=cdf4e799807d7e63db228ed18ed4110b133cfec65fc81e9466744bb552dd9389

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
    boot/dtbs-lts/nvidia/tegra210-p3450-0000.dtb 2>/dev/null ||
    tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/kernel"
cp "$work/kernel/boot/vmlinuz-lts" "$OUT/vmlinuz"
cp "$work/kernel/boot/dtbs-lts/nvidia/tegra210-p3450-0000.dtb" "$OUT/"

mkdir "$work/rootfs"
tar -xzf "$OUT/downloads/$ROOTFS" -C "$work/rootfs"
# Kernel modules for the console UART (8250_tegra), the SD card (sdhci-tegra) and ext4,
# with modules.dep.
# GNU tar needs --wildcards for member patterns; bsdtar (macOS) matches them by default.
wildcards=
tar --version 2>/dev/null | grep -q 'GNU tar' && wildcards=--wildcards
tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/rootfs" $wildcards \
    'lib/modules/*/modules.*' 'lib/modules/*/kernel/drivers/mmc/*' \
    'lib/modules/*/kernel/fs/ext4/*' 'lib/modules/*/kernel/fs/jbd2/*' \
    'lib/modules/*/kernel/fs/mbcache.ko.gz' 'lib/modules/*/kernel/lib/crc/*' \
    'lib/modules/*/kernel/drivers/tty/serial/8250/8250_tegra.ko.gz'
cat > "$work/rootfs/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
# Tegra's UART driver is a module in this kernel: ttyS0 (the console) exists once it loads
/bin/busybox modprobe 8250_tegra
exec </dev/console >/dev/console 2>&1
/bin/busybox mount -t tmpfs tmpfs /tmp
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin PS1='jetson-nano# '
echo "TEGRA210_BOOT_OK: arm64 Linux BusyBox shell"
# setsid -c makes the console the shell's controlling terminal (job control, ^C).
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$work/rootfs/init"
(cd "$work/rootfs" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
    gzip -9n > "$OUT/initramfs.cpio.gz"

shasum -a 256 "$OUT/vmlinuz" "$OUT/tegra210-p3450-0000.dtb" "$OUT/initramfs.cpio.gz"
