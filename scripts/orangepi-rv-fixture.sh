#!/bin/sh
# Build the Orange Pi RV (StarFive JH7110) direct-boot fixture from pinned Alpine packages:
#   OUT/Image                       Alpine linux-stable (riscv64) kernel, unmodified
#                                   (the package ships it gzip'd as vmlinuz; only gunzipped)
#   OUT/jh7110-orangepi-rv.dtb      the matching mainline device tree, unmodified
#   OUT/fw_dynamic.bin              Alpine opensbi, generic platform, FW_DYNAMIC firmware
#   OUT/initramfs.cpio.gz           Alpine riscv64 minirootfs + a serial-shell /init, with the
#                                   SD/Ethernet/USB HID/Wi-Fi/audio modules, the BCM43456 firmware,
#                                   iw, aplay and speaker-test
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
FIRMWARE_APK=linux-firmware-brcm-20260519-r0.apk
FIRMWARE_URL=$MIRROR/v3.24/main/riscv64/$FIRMWARE_APK
FIRMWARE_SHA=8ccddc77b11c411a05cc5639f91049bf91c6e191dc3569be48a537925c3d838c
IW_APK=iw-6.17-r0.apk
IW_SHA=14b204e41395781f49edfd26982ed18aa58ecf23e3c78cfe1b38b9372d6ad6a6
LIBNL_APK=libnl3-3.11.0-r0.apk
LIBNL_SHA=9f768fa96f6b282e62322c4769fadb587afc71d37ab3df65af2e90e492237268
ALSALIB_APK=alsa-lib-1.2.15.3-r0.apk
ALSALIB_SHA=095540647f4bd4c5c70abeb68e242f3c285857e84ecfc3788d8b0b41bd1580e3
ALSAUTILS_APK=alsa-utils-1.2.15.2-r1.apk
ALSAUTILS_SHA=88f905da0d555f26231c52b3a17b9fe5d4db1f95f7683000bb0b3ad0429d08a8
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
fetch "$FIRMWARE_APK" "$FIRMWARE_URL" "$FIRMWARE_SHA"
fetch "$IW_APK" "$MIRROR/v3.24/main/riscv64/$IW_APK" "$IW_SHA"
fetch "$LIBNL_APK" "$MIRROR/v3.24/main/riscv64/$LIBNL_APK" "$LIBNL_SHA"
fetch "$ALSALIB_APK" "$MIRROR/v3.24/main/riscv64/$ALSALIB_APK" "$ALSALIB_SHA"
fetch "$ALSAUTILS_APK" "$MIRROR/v3.24/main/riscv64/$ALSAUTILS_APK" "$ALSAUTILS_SHA"

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
# Kernel modules for the SD card (DesignWare MMC, mmc_block), ext4 and Ethernet (stmmac,
# dwmac-starfive, the Motorcomm PHY) and USB HID: modules in
# linux-stable, with modules.dep. The JH7110 clocks/resets/pinctrl/UART/PLIC are built in.
tar -xzf "$OUT/downloads/$KERNEL_APK" -C "$work/rootfs" $wildcards \
    'lib/modules/*/modules.*' 'lib/modules/*/kernel/drivers/mmc/*' \
    'lib/modules/*/kernel/fs/ext4/*' 'lib/modules/*/kernel/fs/jbd2/*' \
    'lib/modules/*/kernel/fs/mbcache.ko.gz' 'lib/modules/*/kernel/lib/crc/*' \
    'lib/modules/*/kernel/drivers/net/ethernet/stmicro/*' 'lib/modules/*/kernel/drivers/net/phy/*' \
    'lib/modules/*/kernel/drivers/net/pcs/*' 'lib/modules/*/kernel/net/packet/*' \
    'lib/modules/*/kernel/drivers/hid/hid.ko.gz' 'lib/modules/*/kernel/drivers/hid/hid-generic.ko.gz' \
    'lib/modules/*/kernel/drivers/hid/usbhid/*' 'lib/modules/*/kernel/drivers/input/evdev.ko.gz' \
    'lib/modules/*/kernel/drivers/net/wireless/broadcom/brcm80211/*' 'lib/modules/*/kernel/net/wireless/*' \
    'lib/modules/*/kernel/drivers/i2c/busses/i2c-designware-*' 'lib/modules/*/kernel/drivers/mfd/axp20x*' \
    'lib/modules/*/kernel/drivers/regulator/axp20x-regulator.ko.gz' 'lib/modules/*/kernel/drivers/mmc/core/*' \
    'lib/modules/*/kernel/drivers/regulator/fixed.ko.gz' 'lib/modules/*/kernel/net/rfkill/*' \
    'lib/modules/*/kernel/sound/*'
# ALSA: aplay and speaker-test for the PWM DAC (headphone jack)
tar -xzf "$OUT/downloads/$ALSALIB_APK" -C "$work/rootfs" $wildcards 'usr/lib/*' 'usr/share/alsa/*'
tar -xzf "$OUT/downloads/$ALSAUTILS_APK" -C "$work/rootfs" usr/bin/aplay usr/bin/speaker-test
# iw (and libnl) to scan for and join the virtual access point
tar -xzf "$OUT/downloads/$IW_APK" -C "$work/rootfs" usr/sbin/iw
tar -xzf "$OUT/downloads/$LIBNL_APK" -C "$work/rootfs" $wildcards 'usr/lib/*'
# The AP6256's BCM43456 firmware (brcmfmac43456-sdio: firmware, CLM, generic NVRAM)
tar -xzf "$OUT/downloads/$FIRMWARE_APK" -C "$work/rootfs" $wildcards 'lib/firmware/brcm/brcmfmac43456-sdio.bin.zst' \
    'lib/firmware/brcm/brcmfmac43456-sdio.clm_blob.zst' 'lib/firmware/brcm/brcmfmac43456-sdio.txt.zst'
ls "$work"/rootfs/lib/modules/*/kernel/drivers/mmc/host/dw_mmc-starfive.ko* > /dev/null
ls "$work"/rootfs/lib/modules/*/kernel/drivers/net/ethernet/stmicro/stmmac/dwmac-starfive.ko* > /dev/null
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
# Gigabit Ethernet: stmmac with the StarFive glue and the Motorcomm PHY driver are modules.
modprobe dwmac-starfive 2>/dev/null; modprobe motorcomm 2>/dev/null; modprobe af_packet 2>/dev/null
# USB keyboard and tablet on the VL805 (USB HID and evdev are modules)
modprobe usbhid 2>/dev/null; modprobe hid-generic 2>/dev/null; modprobe evdev 2>/dev/null
# Wi-Fi: the PMIC's regulators (I2C5) power the SDIO slot; then brcmfmac on SDIO0
modprobe fixed 2>/dev/null; modprobe i2c-designware-platform 2>/dev/null; modprobe axp20x-i2c 2>/dev/null
modprobe axp20x-regulator 2>/dev/null
modprobe pwrseq_simple 2>/dev/null; modprobe brcmfmac 2>/dev/null
# audio: the PWM DAC, its dummy S/PDIF codec and the simple sound card
modprobe jh7110_pwmdac 2>/dev/null; modprobe snd-soc-spdif-tx 2>/dev/null; modprobe snd-soc-simple-card 2>/dev/null
echo "JH7110_BOOT_OK: RV64 Linux BusyBox shell"
# setsid -c makes the console the shell's controlling terminal (job control, ^C).
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$work/rootfs/init"
(cd "$work/rootfs" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) |
    gzip -9n > "$OUT/initramfs.cpio.gz"

shasum -a 256 "$OUT/Image" "$OUT/$DTB" "$OUT/fw_dynamic.bin" "$OUT/initramfs.cpio.gz"
