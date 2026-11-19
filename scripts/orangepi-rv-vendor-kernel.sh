#!/bin/sh
# Build Xunlong's own Orange Pi RV kernel (orangepi-xunlong/linux-orangepi, branch
# orange-pi-5.15-jh7110: StarFive's 5.15 BSP with the board's jh7110-orangepi-rv.dts and the
# vendor display (DC8200 + Inno HDMI), GMAC and SD drivers) with orangepi-build's
# linux-starfive2-current config, in Docker, into cache/orangepi-rv/vendor/:
#   Image, jh7110-orangepi-rv.dtb, modules/ (lib/modules/<version>), kernel.config
# Usage: scripts/orangepi-rv-vendor-kernel.sh     (about 30-60 min the first time)
set -eu
cd "$(dirname "$0")/.."
OUT=$PWD/cache/orangepi-rv/vendor
KERNEL_REV=835cdda06a7ad62ea5619f6b323b3fa4c59a58c9   # orange-pi-5.15-jh7110, 2024-08-19
BUILD_REV=7f24222b928eab94780f2af03289ce527e07241e   # orangepi-xunlong/orangepi-build next, 2026-11-18
mkdir -p "$OUT"
cat > "$OUT/build.sh" <<SCRIPT
#!/bin/sh
set -ex
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git make gcc gcc-riscv64-linux-gnu bc bison flex \
    libssl-dev libelf-dev cpio kmod python3 rsync ca-certificates curl >/dev/null
cd /work
if [ ! -d linux ]; then
  git init -q linux && cd linux
  git remote add origin https://github.com/orangepi-xunlong/linux-orangepi.git
  git fetch -q --depth 1 origin $KERNEL_REV && git checkout -q FETCH_HEAD
  cd ..
fi
curl -fsSL https://raw.githubusercontent.com/orangepi-xunlong/orangepi-build/$BUILD_REV/external/config/kernel/linux-starfive2-current.config -o linux/.config
cd linux
export ARCH=riscv CROSS_COMPILE=riscv64-linux-gnu-
make olddefconfig >/dev/null
make -j\$(nproc) Image modules starfive/jh7110-orangepi-rv.dtb 2>&1 | grep -E "error|warning: (implicit|incompatible)" | head -20 || true
test -f arch/riscv/boot/Image
rm -rf /out/modules && make -s INSTALL_MOD_PATH=/out/modules modules_install
cp arch/riscv/boot/Image /out/Image
cp arch/riscv/boot/dts/starfive/jh7110-orangepi-rv.dtb /out/
cp .config /out/kernel.config
SCRIPT
chmod +x "$OUT/build.sh"
docker volume create sbc-emu-opirv-kernel >/dev/null
docker run --rm -v sbc-emu-opirv-kernel:/work -v "$OUT":/out debian:bookworm /out/build.sh
ls -l "$OUT/Image" "$OUT/jh7110-orangepi-rv.dtb"

# An initramfs for this kernel: the mainline fixture's (scripts/orangepi-rv-fixture.sh) plus
# Broadcom's bcmdhd Wi-Fi module from this build and the AP6256 firmware and NVRAM it loads
# (CONFIG_BCMDHD_FW_PATH / _NVRAM_PATH) from orangepi-xunlong/firmware, and for Bluetooth the
# bluetooth/hci_uart modules with BlueZ's hciattach, hciconfig and hcitool (Alpine).
FW_REV=dc163b0ed2e0272d9ee31977a0e0a04884f0ae61
FW_SHA=a603124ff862a33263df0cd461722f95c9346ba4f63c5e0024033c328d903a5e
NVRAM_SHA=588430b4c2c167ca035f17da7478bec3d4922487152610d385d5ccb8f7c0a75f
ALPINE=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}/v3.24/main/riscv64
BLUEZ_APK=bluez-deprecated-5.86-r2.apk
BLUEZ_SHA=9417498d8fed8b84c931b057acffefd34cb84e1248ba62fbe7cbec80fc2a105d
UDEV_APK=eudev-libs-3.2.14-r6.apk
UDEV_SHA=4f9cc385a9a96bc0e36415b45827c8d1eddb1690c4189dad5cd46dedf092578d
BASE_INITRD=$PWD/cache/orangepi-rv/initramfs.cpio.gz
if [ -f "$BASE_INITRD" ]; then
  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT
  mkdir -p "$work/root/lib/firmware" "$work/dl"
  for f in fw_bcm43456c5_ag.bin:$FW_SHA nvram_ap6256.txt:$NVRAM_SHA; do
    name=${f%%:*}; sha=${f#*:}
    curl -fsSL -o "$work/dl/$name" "https://raw.githubusercontent.com/orangepi-xunlong/firmware/$FW_REV/$name"
    [ "$(shasum -a 256 "$work/dl/$name" | cut -d' ' -f1)" = "$sha" ] || { echo "$name: checksum mismatch" >&2; exit 1; }
    cp "$work/dl/$name" "$work/root/lib/firmware/"
  done
  for f in $BLUEZ_APK:$BLUEZ_SHA $UDEV_APK:$UDEV_SHA; do
    name=${f%%:*}; sha=${f#*:}
    curl -fsSL -o "$work/dl/$name" "$ALPINE/$name"
    [ "$(shasum -a 256 "$work/dl/$name" | cut -d' ' -f1)" = "$sha" ] || { echo "$name: checksum mismatch" >&2; exit 1; }
  done
  (cd "$work/root" && gzip -dc "$BASE_INITRD" | cpio -id --quiet 2>/dev/null)
  tar -xzf "$work/dl/$BLUEZ_APK" -C "$work/root" usr/bin/hciattach usr/bin/hciconfig usr/bin/hcitool 2>/dev/null
  tar -xzf "$work/dl/$UDEV_APK" -C "$work/root" usr/lib/libudev.so.1 usr/lib/libudev.so.1.6.3 2>/dev/null
  ver=$(ls "$OUT/modules/lib/modules")
  mkdir -p "$work/root/lib/modules/$ver/extra"
  for m in drivers/net/wireless/bcmdhd/bcmdhd.ko net/bluetooth/bluetooth.ko drivers/bluetooth/hci_uart.ko; do
    cp "$OUT/modules/lib/modules/$ver/kernel/$m" "$work/root/lib/modules/$ver/extra/"
  done
  # load them before the shell starts (this kernel's Wi-Fi driver is bcmdhd; mainline uses
  # brcmfmac; Bluetooth is the H4 line discipline on UART1)
  sed -i.bak "s|^echo \"JH7110_BOOT_OK|for m in bcmdhd bluetooth hci_uart; do [ -f /lib/modules/\$(uname -r)/extra/\$m.ko ] \&\& insmod /lib/modules/\$(uname -r)/extra/\$m.ko 2>/dev/null; done\necho \"JH7110_BOOT_OK|" "$work/root/init"
  rm -f "$work/root/init.bak"
  (cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) | gzip -9n > "$OUT/initramfs.cpio.gz"
  ls -l "$OUT/initramfs.cpio.gz"
else
  echo "no $BASE_INITRD: run scripts/orangepi-rv-fixture.sh, then this again for the initramfs" >&2
fi
