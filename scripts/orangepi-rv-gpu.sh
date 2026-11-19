#!/bin/sh
# The Orange Pi RV's GPU (Imagination PowerVR BXE-4-32, BVNC 36.50.54.182) with the open
# stack: mainline Linux's `powervr` DRM driver and Mesa's PowerVR Vulkan driver.
#   cache/orangepi-rv/gpu/Image, modules/  linux 7.1.5 (kernel.org) with Alpine's config
#                                          plus CONFIG_DRM_POWERVR=m, built in Docker
# Alpine's own linux-stable does not build the powervr driver.
set -eu
cd "$(dirname "$0")/.."
OUT=$PWD/cache/orangepi-rv/gpu
KVER=7.1.5
KSHA=22a0196b3cbcdf34dc27b77561f4d040585fd3447edc9ab3531a1ac79e3041e7
[ -f cache/orangepi-rv/kernel.config ] || { echo "run scripts/orangepi-rv-fixture.sh first" >&2; exit 1; }
mkdir -p "$OUT"
cp cache/orangepi-rv/kernel.config "$OUT/alpine.config"
if [ ! -f "$OUT/Image" ]; then
  cat > "$OUT/build.sh" <<SCRIPT
#!/bin/sh
set -ex
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq make gcc gcc-riscv64-linux-gnu bc bison flex \
    libssl-dev libelf-dev cpio kmod python3 rsync ca-certificates curl xz-utils >/dev/null
cd /work
if [ ! -d linux-$KVER ]; then
  curl -fsSL -o linux.tar.xz https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-$KVER.tar.xz
  echo "$KSHA  linux.tar.xz" | sha256sum -c
  tar -xJf linux.tar.xz && rm linux.tar.xz
fi
cd linux-$KVER
export ARCH=riscv CROSS_COMPILE=riscv64-linux-gnu-
cp /out/alpine.config .config
./scripts/config --module DRM_POWERVR --set-str LOCALVERSION "-sbcemu-gpu" --disable LOCALVERSION_AUTO \
    --disable MODULE_SIG --disable MODULE_SIG_ALL --set-str MODULE_SIG_KEY ""
make olddefconfig >/dev/null
grep -E "CONFIG_DRM_POWERVR|CONFIG_DRM_GPUVM|CONFIG_DRM_SCHED=" .config
make -j\$(nproc) Image modules 2>&1 | grep -E "error|Error" | head -20 || true
test -f arch/riscv/boot/Image
rm -rf /out/modules && make -s INSTALL_MOD_PATH=/out/modules modules_install
cp arch/riscv/boot/Image /out/Image
cp .config /out/kernel.config
SCRIPT
  chmod +x "$OUT/build.sh"
  docker volume create sbc-emu-opirv-gpu >/dev/null
  docker run --rm -v sbc-emu-opirv-gpu:/work -v "$OUT":/out debian:trixie /out/build.sh
fi
ls -l "$OUT/Image"
# modules.dep: Alpine's kmod (Debian's cannot read the gzip-compressed modules Alpine's
# config asks for), on a copy inside the container (depmod's directory walk misses files on
# a macOS bind mount)
KREL=$(ls "$OUT/modules/lib/modules")
if [ ! -s "$OUT/modules/lib/modules/$KREL/modules.dep" ]; then
  docker run --rm --platform linux/arm64 -v "$OUT":/out alpine:3.24 sh -c "apk add -q kmod >/dev/null && \
    mkdir -p /tmp/m && cp -a /out/modules/lib /tmp/m/ && depmod -b /tmp/m $KREL && \
    cp /tmp/m/lib/modules/$KREL/modules.* /out/modules/lib/modules/$KREL/"
fi

# An initramfs for this kernel: the mainline fixture's, with its Alpine kernel modules
# replaced by this build's (the same set, plus the powervr driver and its DRM helpers).
BASE_INITRD=$PWD/cache/orangepi-rv/initramfs.cpio.gz
[ -f "$BASE_INITRD" ] || { echo "no $BASE_INITRD: run scripts/orangepi-rv-fixture.sh first" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root"
(cd "$work/root" && gzip -dc "$BASE_INITRD" | cpio -id --quiet 2>/dev/null)
rm -rf "$work/root/lib/modules"
(cd "$OUT/modules" && for p in 'lib/modules/*/modules.*' 'lib/modules/*/kernel/drivers/mmc' \
    'lib/modules/*/kernel/fs/ext4' 'lib/modules/*/kernel/fs/jbd2' 'lib/modules/*/kernel/fs/mbcache.ko*' \
    'lib/modules/*/kernel/lib/crc' 'lib/modules/*/kernel/drivers/net/ethernet/stmicro' \
    'lib/modules/*/kernel/drivers/net/phy' 'lib/modules/*/kernel/drivers/net/pcs' 'lib/modules/*/kernel/net/packet' \
    'lib/modules/*/kernel/drivers/hid/hid.ko*' 'lib/modules/*/kernel/drivers/hid/hid-generic.ko*' \
    'lib/modules/*/kernel/drivers/hid/usbhid' 'lib/modules/*/kernel/drivers/input/evdev.ko*' \
    'lib/modules/*/kernel/drivers/i2c/busses/i2c-designware-*' 'lib/modules/*/kernel/drivers/mfd/axp20x*' \
    'lib/modules/*/kernel/drivers/regulator/axp20x-regulator.ko*' 'lib/modules/*/kernel/drivers/regulator/fixed.ko*' \
    'lib/modules/*/kernel/drivers/gpu/drm/imagination' 'lib/modules/*/kernel/drivers/gpu/drm/scheduler' \
    'lib/modules/*/kernel/drivers/gpu/drm/drm_exec.ko*' 'lib/modules/*/kernel/drivers/gpu/drm/drm_gpuvm.ko*'; do
  for f in $p; do [ -e "$f" ] && mkdir -p "$work/root/$(dirname "$f")" && cp -R "$f" "$work/root/$(dirname "$f")/"; done
done)
# Mesa's PowerVR Vulkan driver with the Vulkan loader and vulkan-tools (vulkaninfo, vkcube),
# from Debian sid riscv64 pinned to a snapshot (Alpine builds no PowerVR driver), unpacked
# into /debian (glibc; run with `chroot /debian`). Only the PowerVR driver is taken from
# mesa-vulkan-drivers.
SNAPSHOT=https://snapshot.debian.org/archive/debian/20261119T000000Z
mkdir -p "$OUT/debs" "$work/root/debian"
while read -r file sha; do
  deb="$OUT/debs/$(basename "$file")"
  [ -f "$deb" ] || curl -fsSL -o "$deb" "$SNAPSHOT/$file"
  [ "$(shasum -a 256 "$deb" | cut -d' ' -f1)" = "$sha" ] || { echo "$deb: checksum mismatch" >&2; exit 1; }
  mkdir -p "$work/x" && (cd "$work/x" && ar x "$deb" && tar -xf data.tar.* && rm -f data.tar.* control.tar.* debian-binary)
  case "$file" in
    */mesa-vulkan-drivers_*)
      mkdir -p "$work/root/debian/usr/lib/riscv64-linux-gnu" "$work/root/debian/usr/share/vulkan/icd.d"
      cp "$work/x/usr/lib/riscv64-linux-gnu/libvulkan_powervr_mesa.so" "$work/root/debian/usr/lib/riscv64-linux-gnu/"
      cp "$work/x/usr/share/vulkan/icd.d/powervr_mesa_icd.json" "$work/root/debian/usr/share/vulkan/icd.d/" ;;
    *) cp -R "$work/x/." "$work/root/debian/" ;;
  esac
  rm -rf "$work/x"
done <<LIST
pool/main/v/vulkan-loader/libvulkan1_1.4.363.0-1_riscv64.deb eb8624e56615d5de82bdbda55e896aba40b9c26c5f84eedeab30c58763330b99
pool/main/v/vulkan-tools/vulkan-tools_1.4.341.0+dfsg1-1_riscv64.deb 5dd8d741bc175f2a9403db9443aa3a967ceab89aa3568d6ccd45de3d60e67984
pool/main/libd/libdrm/libdrm2_2.4.134-3_riscv64.deb a831cb65ade84e522345c74565977639cc866e372cf988f56e4bd6b371609976
pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.2-4_riscv64.deb c4c5ff0d6ac7ffbac12fb47135b9ce0924b638c44ff2be0b457879345c4c6b54
pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-4_riscv64.deb 07c29376b63d416f0f838bbe9ec4e070254356c1b0d01ffa3af0279b49196973
pool/main/libx/libxcb/libxcb1_1.17.0-2+b2_riscv64.deb bab245ae891f9c764546f62dc442335a39218a82864e788e53befa31f5668438
pool/main/libx/libx11/libx11-xcb1_1.8.14-1_riscv64.deb 1830f91685cb5739bdf7767812296c297ddb62682de403a3b641665aee7a9203
pool/main/libx/libxcb/libxcb-dri3-0_1.17.0-2+b2_riscv64.deb d924ff210446df7325982702c4443c89583c1d7cc85777153e04c314be65c8df
pool/main/libx/libxcb/libxcb-present0_1.17.0-2+b2_riscv64.deb 7ca0a2b533ada26a2cda1b2e34b9aeb92691bc101aa901fce6523ae23405fde9
pool/main/libx/libxcb/libxcb-xfixes0_1.17.0-2+b2_riscv64.deb 48a70745818f5d2cb46b36afd60ed8f025b0e811cd44c0be2f0eec1eabbbd6cc
pool/main/libx/libxcb/libxcb-sync1_1.17.0-2+b2_riscv64.deb 71245f4045b58df0213bf7fb9fdc9e0e4cbd54e49816a097472e5eb7f0ba0ef0
pool/main/libx/libxcb/libxcb-randr0_1.17.0-2+b2_riscv64.deb 59b594fa4d5d10c3ed024d632f848be398c5464966522efaadb20cb172ecc3d5
pool/main/libx/libxcb/libxcb-shm0_1.17.0-2+b2_riscv64.deb dccc46b868be533f084c5b33d9b5543a7c3f468d9b414b9da19a7b006d7830a1
pool/main/libx/libxshmfence/libxshmfence1_1.3.3-1+b2_riscv64.deb a45c5b6e47e8a1d2d88f1d657681878c1a92a114f51ab018f632ddd81bb42835
pool/main/w/wayland/libwayland-client0_1.26.0-1_riscv64.deb b5cc259d4094568da833067f74b91ba9901ea3530a1447d8f5cf95c212c0b66b
pool/main/libd/libdisplay-info/libdisplay-info3_0.3.0-1+b1_riscv64.deb e2c4c4a2ed666b832dce3aea397f8d1dff5e725586b9bc4edcb24a49c757b0ec
pool/main/e/expat/libexpat1_2.10.0-1_riscv64.deb 2982bc81914d88849afe800c5498f7230fb9820f085ba3ac4c2cd7aeec37d93a
pool/main/g/gcc-16/libstdc++6_16.2.0-4_riscv64.deb ca46986768b91156b89d1a17f65cd54a7708bc72128f01d37df717f1385ba1ef
pool/main/g/gcc-16/libgcc-s1_16.2.0-4_riscv64.deb 053982cfb062799978b2b843ca7c2bcc321761e36a82ffe038d7460b4b229dd8
pool/main/g/glibc/libc6_2.43-7_riscv64.deb 78cf0e2e53b449b90a8752b0f151dfe70c35852afe986ec0961260b1a2565caf
pool/main/libx/libx11/libx11-6_1.8.14-1_riscv64.deb 55d375226226f3c1728998f6c04ed397cfed927803c40f3455e5f9a2613a20d3
pool/main/libd/libdrm/libdrm-common_2.4.134-3_all.deb 89127ee70fd643f0f1c1fe294bfda51315ea4e92234500211f9956351706363f
pool/main/libx/libxau/libxau6_1.0.11-1+b2_riscv64.deb 8e175ba9f1887f4e831a6f11d2e2ba581b090189de0ee56e264a27e8cf811413
pool/main/libx/libxdmcp/libxdmcp6_1.1.5-2+b1_riscv64.deb 14cb5cf4055e4c215a86a63e6ac1c9b4a4a4283eb3505dc3bf4f5af9ca35bc09
pool/main/libf/libffi/libffi8_3.8.0-2_riscv64.deb 5e7860732d1950eb7e1f2e9bd4d8dce41ea1c6a945bd33a4144147af8d6013cc
pool/main/g/gcc-16/gcc-16-base_16.2.0-4_riscv64.deb 06767e7470cb7491cda9173670f4b6c75065441fe532cacfa87c04a3acae57eb
pool/main/g/glibc/libc-gconv-modules-extra_2.43-7_riscv64.deb 438abd0ab49bcd87d24ee64a09ccccc279f13a2fe6525f470051411410499695
pool/main/libx/libx11/libx11-data_1.8.14-1_all.deb 4e9d56e61e5eca504179adf0fe32aed8bce39dc2d112049a80387f6c1e1cb66b
pool/main/m/mesa/mesa-vulkan-drivers_26.2.6-1_riscv64.deb a903d7e610b51bf41e6b936c055053a7ceb7c119c0c6aff7ff1336daae98f533
LIST
# Debian's merged /usr: /lib, /bin and /sbin are links into /usr
for d in lib bin sbin; do [ -e "$work/root/debian/$d" ] || ln -s "usr/$d" "$work/root/debian/$d"; done
mkdir -p "$work/root/debian/dev" "$work/root/debian/proc" "$work/root/debian/sys" "$work/root/debian/tmp"

# sbc-emu's GPU tests (tests/gpu/pvr/build.sh), when built
if [ -d tests/gpu/pvr/out ]; then
  mkdir -p "$work/root/debian/usr/local/bin"
  for t in tests/gpu/pvr/out/vk*; do case "$t" in *.spv|*.h) ;; *) cp "$t" "$work/root/debian/usr/local/bin/" ;; esac; done
fi

# the GPU firmware file: sbc-emu's stand-in (header, layout and the BXE-4-32's device info,
# no firmware code; the emulator plays the firmware), as Imagination publishes none for
# this core
mkdir -p "$work/root/lib/firmware/powervr"
cargo run -q --release -p powervr --example write-firmware "$work/root/lib/firmware/powervr/rogue_36.50.54.182_v1.fw"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) | gzip -9n > "$OUT/initramfs.cpio.gz"
ls -l "$OUT/initramfs.cpio.gz"
