#!/bin/sh
# Mainline U-Boot v2026.10 for the Orange Pi RV (starfive_visionfive2_defconfig, which
# recognises the board by its EEPROM product ID "XOPIRV"), built in Docker:
#   cache/orangepi-rv/uboot/u-boot-spl.bin.normal.out  SPL with the JH7110 boot ROM header
#   cache/orangepi-rv/uboot/u-boot.itb                 U-Boot + OpenSBI (fw_dynamic) FIT
# OpenSBI is the fixture's (scripts/orangepi-rv-fixture.sh), so run that first.
set -eu
cd "$(dirname "$0")/.."
OUT=$PWD/cache/orangepi-rv/uboot
UBOOT_REV=a58e2b7af42c179ace2c6210de0a219fa3f3cf3c   # v2026.10
[ -f cache/orangepi-rv/fw_dynamic.bin ] || { echo "run scripts/orangepi-rv-fixture.sh first" >&2; exit 1; }
mkdir -p "$OUT"
cp cache/orangepi-rv/fw_dynamic.bin "$OUT/fw_dynamic.bin"
cat > "$OUT/build.sh" <<SCRIPT
#!/bin/sh
set -ex
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git make gcc gcc-riscv64-linux-gnu bc bison flex \
    libssl-dev libgnutls28-dev python3 python3-setuptools python3-pyelftools python3-dev swig \
    device-tree-compiler uuid-dev ca-certificates >/dev/null
cd /work
# the pinned commit, also after an interrupted first fetch or a change of UBOOT_REV
[ -d u-boot/.git ] || { rm -rf u-boot; git init -q u-boot; git -C u-boot remote add origin https://github.com/u-boot/u-boot.git; }
cd u-boot
if [ "\$(git rev-parse -q --verify HEAD 2>/dev/null)" != "$UBOOT_REV" ]; then
  git fetch -q --depth 1 origin $UBOOT_REV
  git checkout -q --force FETCH_HEAD
fi
export ARCH=riscv CROSS_COMPILE=riscv64-linux-gnu-
make starfive_visionfive2_defconfig >/dev/null
rm -f spl/u-boot-spl.bin.normal.out u-boot.itb
make -j\$(nproc) OPENSBI=/out/fw_dynamic.bin > /out/build.log 2>&1 || { grep -E "error|Error" /out/build.log | head -20; exit 1; }
cp spl/u-boot-spl.bin.normal.out u-boot.itb /out/
cp .config /out/uboot.config
SCRIPT
chmod +x "$OUT/build.sh"
docker volume create sbc-emu-opirv-uboot >/dev/null
docker run --rm -v sbc-emu-opirv-uboot:/work -v "$OUT":/out debian:bookworm /out/build.sh
ls -l "$OUT/u-boot-spl.bin.normal.out" "$OUT/u-boot.itb"

# A bootable microSD image in StarFive's layout, with the mainline fixture's kernel:
#   partition 1 (2 MiB, type "HiFive/StarFive SPL")  u-boot-spl.bin.normal.out; the boot ROM
#               finds it at the header's backup offset, 2 MiB
#   partition 2 (4 MiB, type "HiFive/StarFive U-Boot") u-boot.itb, which SPL loads
#   partition 3 (ext4, legacy-BIOS-bootable)            /boot with extlinux.conf, Image, the DTB
#               and the initramfs, which U-Boot's distro boot finds
F=$PWD/cache/orangepi-rv
if [ -f "$F/Image" ] && [ -f "$F/initramfs.cpio.gz" ]; then
  mkdir -p "$OUT/boot/extlinux"
  cp "$F/Image" "$F/jh7110-orangepi-rv.dtb" "$F/initramfs.cpio.gz" "$OUT/boot/"
  cat > "$OUT/boot/extlinux/extlinux.conf" <<CONF
default alpine
timeout 1
label alpine
    menu label Alpine linux-stable (mainline)
    kernel /boot/Image
    fdt /boot/jh7110-orangepi-rv.dtb
    initrd /boot/initramfs.cpio.gz
    append console=ttyS0,115200 earlycon rdinit=/init
CONF
  cat > "$OUT/card.sh" <<'SCRIPT'
#!/bin/sh
set -e
apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gdisk e2fsprogs >/dev/null
cd /out
rm -f sdcard.img root.img
truncate -s 128M sdcard.img
sgdisk -o \
  -n 1:4096:8191   -t 1:2E54B353-1271-4842-806F-E436D6AF6985 -c 1:spl \
  -n 2:8192:16383  -t 2:BC13C2FF-59E6-4262-A352-B275FD6F7172 -c 2:uboot \
  -n 3:16384:0     -t 3:0FC63DAF-8483-4772-8E79-3D69D8477DE4 -c 3:root -A 3:set:2 sdcard.img
dd if=u-boot-spl.bin.normal.out of=sdcard.img bs=512 seek=4096 conv=notrunc status=none
dd if=u-boot.itb of=sdcard.img bs=512 seek=8192 conv=notrunc status=none
mkdir -p rootfs && cp -r boot rootfs/
end=$(sgdisk -i 3 sdcard.img | sed -n 's/^Last sector: \([0-9]*\).*/\1/p')
mkfs.ext4 -q -F -L root -d rootfs root.img $(( (end - 16384 + 1) / 2 ))k
dd if=root.img of=sdcard.img bs=512 seek=16384 conv=notrunc status=none
rm -rf rootfs root.img
sgdisk -p sdcard.img
SCRIPT
  chmod +x "$OUT/card.sh"
  docker run --rm -v "$OUT":/out debian:bookworm /out/card.sh
  ls -l "$OUT/sdcard.img"
fi
