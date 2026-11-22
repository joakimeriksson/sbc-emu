#!/bin/sh
# Build the Jetson Nano guest with Debian 13's arm64 kernel, which has every Tegra210 driver the
# board needs (Alpine's lacks the MAX77620 PMIC, PCIe and the SMMU):
#   OUT/vmlinuz, OUT/tegra210-p3450-0000.dtb   Debian's, unmodified
#   OUT/initramfs.cpio.gz                      Alpine aarch64 BusyBox, the kernel modules, /init
# /init loads the board's drivers and, with root=DEV on the command line, switches to it.
# The kernel is pinned; Debian's mirrors drop superseded versions, snapshot.debian.org keeps them.
# Usage: scripts/jetson-nano-debian-fixture.sh [OUT]   (default cache/jetson-nano/debian)
set -eu
OUT=${1:-cache/jetson-nano/debian}
DEB=linux-image-6.12.117+deb13-arm64_6.12.117-1_arm64.deb
DEB_URL=${DEBIAN_MIRROR:-https://deb.debian.org/debian}/pool/main/l/linux-signed-arm64/$DEB
DEB_SNAPSHOT=https://snapshot.debian.org/archive/debian/20261121T000000Z/pool/main/l/linux-signed-arm64/$DEB
DEB_SHA=73805585385c78e048e63fdfba66da36bab724c2cdebd96a3055a8a4f1ae287a
ROOTFS=alpine-minirootfs-3.24.4-aarch64.tar.gz
ROOTFS_URL=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}/v3.24/releases/aarch64/$ROOTFS
ROOTFS_SHA=4b14c18f7e90a063b61462ea1462a5732c154bf6adad594974fe6487cd98a8de
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
DL=${DOWNLOADS:-$OUT/downloads}
mkdir -p "$DL"
fetch() { # file url sha256 [other-url]
    if [ ! -f "$DL/$1" ]; then
        echo "fetching $2" >&2
        curl -fL --retry 3 -o "$DL/$1.part" "$2" || { [ -n "${4:-}" ] && curl -fL --retry 3 -o "$DL/$1.part" "$4"; }
        mv "$DL/$1.part" "$DL/$1"
    fi
    actual=$(shasum -a 256 "$DL/$1" | cut -d' ' -f1)
    [ "$actual" = "$3" ] || { echo "$1: sha256 $actual, expected $3" >&2; exit 1; }
}
fetch "$DEB" "$DEB_URL" "$DEB_SHA" "$DEB_SNAPSHOT"
fetch "$ROOTFS" "$ROOTFS_URL" "$ROOTFS_SHA"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/deb" "$work/root"
(cd "$work/deb" && ar x "$DL/$DEB" && tar -xf data.tar.*)
cp "$work"/deb/boot/vmlinuz-* "$OUT/vmlinuz"
cp "$work"/deb/usr/lib/linux-image-*/nvidia/tegra210-p3450-0000.dtb "$OUT/"
tar -xzf "$DL/$ROOTFS" -C "$work/root"
version=$(basename "$work"/deb/usr/lib/modules/*)
src=$work/deb/usr/lib/modules/$version
dst=$work/root/lib/modules/$version
mkdir -p "$dst"
cp "$src/modules.builtin" "$src/modules.builtin.modinfo" "$src/modules.order" "$dst/"
# drivers of the board and what they need (modules.dep is made in the guest)
for m in drivers/i2c/busses/i2c-tegra drivers/regulator/fixed drivers/regulator/gpio-regulator \
    drivers/regulator/max77620-regulator drivers/pwm/pwm-tegra drivers/regulator/pwm-regulator \
    drivers/mmc/host/sdhci drivers/mmc/host/cqhci drivers/mmc/host/sdhci-pltfm drivers/mmc/host/sdhci-tegra \
    fs/ext4/ext4 fs/jbd2/jbd2 fs/mbcache lib/crc16 lib/libcrc32c crypto/crc32c_generic; do
    mkdir -p "$dst/kernel/$(dirname $m)"
    xz -dc "$src/kernel/$m.ko.xz" > "$dst/kernel/$m.ko"
done
cat > "$work/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin PS1='jetson-nano# '
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
depmod -a
for m in pwm-tegra fixed gpio-regulator i2c-tegra max77620-regulator pwm-regulator sdhci-tegra crc32c_generic ext4; do
    modprobe $m || echo "modprobe $m failed"
done
# root=/dev/..., LABEL= or UUID= (BusyBox finds no PARTUUID=); "card" is the first ext4 partition of the microSD
# card (the kernel names the two SD hosts in the order they probe)
root=$(sed -n 's/.*root=\([^ ]*\).*/\1/p' /proc/cmdline)
if [ -n "$root" ]; then
    dev=
    for i in $(seq 100); do
        case $root in
            card) dev=$(blkid | grep 'TYPE="ext4"' | sed -n 's/^\(\/dev\/mmcblk[0-9]*p[0-9]*\):.*/\1/p' | head -n1) ;;
            *=*) dev=$(findfs "$root" 2>/dev/null) ;;
            *) dev=$root ;;
        esac
        [ -b "$dev" ] && break
        usleep 100000
    done
    mkdir -p /new
    if mount -t ext4 "$dev" /new; then
        for d in dev proc sys; do mount -o move /$d /new/$d; done
        exec switch_root /new /sbin/init
    fi
    echo "cannot mount root=$root (found: ${dev:-nothing}; use /dev/..., LABEL=, UUID= or card)"
fi
echo "TEGRA210_BOOT_OK: arm64 Linux BusyBox shell"
exec setsid -c /bin/sh
INIT
chmod 755 "$work/root/init"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) | gzip -9n > "$OUT/initramfs.cpio.gz"
shasum -a 256 "$OUT/vmlinuz" "$OUT/tegra210-p3450-0000.dtb" "$OUT/initramfs.cpio.gz"
