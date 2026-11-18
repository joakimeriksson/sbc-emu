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
