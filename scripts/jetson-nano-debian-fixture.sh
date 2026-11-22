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
sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
fetch() { # file sha256 url...
    f=$1 want=$2
    shift 2
    if [ ! -f "$DL/$f" ]; then
        for url; do
            echo "fetching $url" >&2
            curl -fL --retry 3 -o "$DL/$f.part" "$url" || continue
            [ "$(sha "$DL/$f.part")" = "$want" ] && mv "$DL/$f.part" "$DL/$f" && return
            echo "$url: not the file with sha256 $want" >&2
        done
        rm -f "$DL/$f.part"
        exit 1
    fi
    [ "$(sha "$DL/$f")" = "$want" ] || { echo "$DL/$f: sha256 is not $want" >&2; exit 1; }
}
fetch "$DEB" "$DEB_SHA" "$DEB_URL" "$DEB_SNAPSHOT"
fetch "$ROOTFS" "$ROOTFS_SHA" "$ROOTFS_URL"
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
# drivers of the board, and what each of them needs (modules.dep is made in the guest)
find "$src/kernel" -name '*.ko.xz' > "$work/modules"
todo="i2c-tegra fixed gpio-regulator max77620-regulator pwm-tegra pwm-regulator sdhci-tegra ext4 crc32c_generic
    phy-tegra-xusb r8169 realtek af_packet xhci-tegra usbhid hid-generic evdev usb-storage sd_mod tegra-drm nouveau"
done=" "
while [ -n "$todo" ]; do
    set -- $todo
    todo=
    for m; do
        case $done in *" $m "*) continue ;; esac
        done="$done$m "
        f=$(grep "/$(echo "$m" | sed 's/[-_]/[-_]/g')\.ko\.xz$" "$work/modules" | head -n1)
        if [ -z "$f" ]; then
            grep -q "/$(echo "$m" | sed 's/[-_]/[-_]/g')\.ko$" "$src/modules.builtin" && continue
            echo "$m: no such module, and not built in" >&2
            exit 1
        fi
        o=${f#"$src"/}
        mkdir -p "$dst/$(dirname "$o")"
        xz -dc "$f" > "$dst/${o%.xz}"
        todo="$todo $(LC_ALL=C tr "\0" "\n" < "$dst/${o%.xz}" | LC_ALL=C sed -n 's/^depends=//p' | tr ',' ' ')"
    done
done
# The USB controller's driver wants NVIDIA's firmware, which Debian has in firmware-misc-nonfree. The
# emulator does not run it, so a file with nothing but a length (fwimg_len, at byte 100) does.
fw=$work/root/lib/firmware/nvidia/tegra210
mkdir -p "$fw"
{ head -c 100 /dev/zero; printf '\000\020\000\000'; head -c 3992 /dev/zero; } > "$fw/xusb.bin"
python3 "$(dirname "$0")/../tools/nano_gpu_firmware.py" "$work/root/lib/firmware"
cat > "$work/root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin PS1='jetson-nano# '
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
depmod -a
for m in pwm-tegra fixed gpio-regulator i2c-tegra max77620-regulator pwm-regulator sdhci-tegra crc32c_generic ext4 phy-tegra-xusb realtek r8169 af_packet \
    usbhid hid-generic evdev sd_mod usb-storage xhci-tegra tegra-drm nouveau; do
    modprobe $m || echo "modprobe $m failed"
done
# root=/dev/..., LABEL= or UUID= (BusyBox finds no PARTUUID=); "card" is the first ext4 partition of the microSD
# card (the kernel names the two SD hosts in the order they probe)
root=$(sed -n 's/.*root=\([^ ]*\).*/\1/p' /proc/cmdline)
if [ -n "$root" ]; then
    dev=
    case $root in card | /dev/* | LABEL=* | UUID=*) ;; *) echo "root=$root: use /dev/..., LABEL=, UUID= or card"; root= ;; esac
fi
if [ -n "$root" ]; then
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
    if [ -b "$dev" ] && mount -t ext4 "$dev" /new; then
        for d in dev proc sys; do mount -o move /$d /new/$d; done
        exec switch_root /new /sbin/init
    fi
    [ -b "$dev" ] && echo "cannot mount $dev" || echo "no block device for root=$root"
fi
# a shell on the monitor too, if there is one (openvt: it has its own session, and Ctrl-C works)
if grep -q '^connected' /sys/class/drm/card*/status 2>/dev/null; then
    openvt -c 1 -- sh -c 'while :; do sh; done'
fi
echo "TEGRA210_BOOT_OK: arm64 Linux BusyBox shell"
# (late drivers' messages would bury the prompt; dmesg has them)
dmesg -n 1
exec setsid -c /bin/sh
INIT
chmod 755 "$work/root/init"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) | gzip -9n > "$OUT/initramfs.cpio.gz"
shasum -a 256 "$OUT/vmlinuz" "$OUT/tegra210-p3450-0000.dtb" "$OUT/initramfs.cpio.gz"
