#!/bin/sh
# Build the Orange Pi RV (StarFive JH7110) direct-boot fixture from pinned Alpine packages:
#   OUT/Image                       Alpine linux-stable (riscv64) kernel, unmodified
#                                   (the package ships it gzip'd as vmlinuz; only gunzipped)
#   OUT/jh7110-orangepi-rv.dtb      the matching mainline device tree, unmodified
#   OUT/fw_dynamic.bin              Alpine opensbi, generic platform, FW_DYNAMIC firmware
#   OUT/initramfs.cpio.gz           Alpine riscv64 minirootfs + a serial-shell /init
# Usage: scripts/orangepi-rv-fixture.sh [OUT]   (default cache/orangepi-rv)
# Downloads are verified against SHA-256; Alpine eventually removes superseded
# package versions, so update the pins together.
#
# Why linux-stable and not linux-lts: v3.24's linux-lts (6.18) predates
# jh7110-orangepi-rv.dts; linux-stable (community) ships it. linux-stable is bumped
# often within a release, so expect to move this pin more often than the others.
set -eu

OUT=${1:-cache/orangepi-rv}
MIRROR=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
ROOTFS=alpine-minirootfs-3.24.4-riscv64.tar.gz
ROOTFS_URL=$MIRROR/v3.24/releases/riscv64/$ROOTFS
ROOTFS_SHA=010683db6c73b3ad1c334fc8f93f64ae748ef67ae6d69e964929aeaf8517bff3
KERNEL_APK=linux-stable-7.1.5-r0.apk
KERNEL_URL=$MIRROR/v3.24/community/riscv64/$KERNEL_APK
KERNEL_SHA=48712755bb52ace44a01f89ccd0677a528ba39e2a271664475380759cb9eb401
OPENSBI_APK=opensbi-1.8.1-r0.apk
OPENSBI_URL=$MIRROR/v3.24/main/riscv64/$OPENSBI_APK
OPENSBI_SHA=b1086a0b5d1b18249db6df7df31e97cbdab90886ee465e1c429f8ad8cac47664
DTB=jh7110-orangepi-rv.dtb

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
fetch "$OPENSBI_APK" "$OPENSBI_URL" "$OPENSBI_SHA"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# GNU tar needs --wildcards for member patterns; bsdtar (macOS) matches them by default.
wildcards=
tar --version 2>/dev/null | grep -q 'GNU tar' && wildcards=--wildcards

# The .apk is a sequence of gzip'd tar segments; tar reads through all of them.
mkdir "$work/kernel"
tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/kernel" $wildcards \
    'boot/vmlinuz-*' 'boot/config-*' "boot/dtbs-*/starfive/$DTB"
vmlinuz=$(ls "$work"/kernel/boot/vmlinuz-*)
# Alpine's riscv64 vmlinuz is the arch/riscv/boot/Image.gz; the emulator wants the raw Image.
if [ "$(od -An -tx1 -N2 "$vmlinuz" | tr -d ' ')" = 1f8b ]; then
    gzip -dc "$vmlinuz" > "$OUT/Image"
else
    cp "$vmlinuz" "$OUT/Image"
fi
# RISC-V boot header: magic "RISCV\0\0\0" at 0x30, magic2 "RSC\x05" at 0x38.
magic=$(od -An -tx1 -j48 -N12 "$OUT/Image" | tr -d ' \n')
if [ "$magic" != 524953435600000052534305 ]; then
    echo "Image: no RISC-V boot header magic at 0x30 ($magic)" >&2
    exit 1
fi
cp "$work"/kernel/boot/config-* "$OUT/kernel.config"
find "$work/kernel/boot" -name "$DTB" -exec cp {} "$OUT/" \;
test -s "$OUT/$DTB"

mkdir "$work/opensbi"
tar -xzf "$OUT/downloads/$OPENSBI_APK" -C "$work/opensbi" \
    usr/share/opensbi/generic/firmware/fw_dynamic.bin
cp "$work/opensbi/usr/share/opensbi/generic/firmware/fw_dynamic.bin" "$OUT/"

mkdir "$work/rootfs"
tar -xzf "$OUT/downloads/$ROOTFS" -C "$work/rootfs"
# Kernel modules for the SD card (DesignWare MMC, mmc_block) and ext4: modules in
# linux-stable, with modules.dep. The JH7110 clocks/resets/pinctrl/UART/PLIC are built in.
tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/rootfs" $wildcards \
    'lib/modules/*/modules.*' 'lib/modules/*/kernel/drivers/mmc/*' \
    'lib/modules/*/kernel/fs/ext4/*' 'lib/modules/*/kernel/fs/jbd2/*' \
    'lib/modules/*/kernel/fs/mbcache.ko.gz' 'lib/modules/*/kernel/lib/crc/*'
ls "$work"/rootfs/lib/modules/*/kernel/drivers/mmc/host/dw_mmc-starfive.ko* > /dev/null
cat > "$work/rootfs/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t tmpfs tmpfs /tmp
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin PS1='orangepi-rv# '
# SD card: the DesignWare MMC driver, the block layer and ext4 are modules.
modprobe dw_mmc-starfive 2>/dev/null; modprobe mmc_block 2>/dev/null
modprobe ext4 2>/dev/null
echo "JH7110_BOOT_OK: RV64 Linux BusyBox shell"
# setsid -c makes the console the shell's controlling terminal (job control, ^C).
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$work/rootfs/init"
(cd "$work/rootfs" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
    gzip -9n > "$OUT/initramfs.cpio.gz"

shasum -a 256 "$OUT/Image" "$OUT/$DTB" "$OUT/fw_dynamic.bin" "$OUT/initramfs.cpio.gz"
