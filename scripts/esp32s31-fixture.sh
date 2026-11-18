#!/bin/sh
# Build Espressif's official ESP32-S31 Linux flash image (esp-buildroot-external,
# espressif_esp32s31_function_core_board_nor_defconfig) in Docker, and fetch the S31 mask ROM
# ELF, into cache/esp32s31/ (ignored; never committed):
#   cache/esp32s31/images/s31_full_flash.bin   SPL + OpenSBI + U-Boot FIT + DTB + XIP Linux + cramfs
#   cache/esp32s31/esp32s31_rev0_rom.elf       espressif/esp-rom-elfs 20260528
# Needs Docker (an x86_64 or AArch64 Linux VM; Colima on a Mac works) and ~10 GB in a volume.
# The defconfig fetches Espressif's prebuilt RV32 musl toolchain; Buildroot itself would clone the
# full U-Boot and Linux histories, so shallow checkouts of the same branches are used as
# *_OVERRIDE_SRCDIR instead. The image is not byte-reproducible (build timestamps).
#   scripts/esp32s31-fixture.sh
set -e
cd "$(dirname "$0")/.."
OUT=$PWD/cache/esp32s31
mkdir -p "$OUT/images" "$OUT/docker"

# Pins (2026-11-18). The branches are developer previews and move; these are what was booted.
BR_EXTERNAL_REV=d75370b      # espressif/esp-buildroot-external buildroot/v2025.02-esp32s31
UBOOT_REV=e5dff8d3           # espressif/u-boot integration/v2024.07-esp32s31
LINUX_REV=19629799           # espressif/linux integration/v6.18-esp32s31
ROM_SHA=a52ad7513deb656a910a5740125f1cce2c7941f11ce57213b7b43aea93d5ab87

if [ ! -f "$OUT/esp32s31_rev0_rom.elf" ]; then
  tmp=$(mktemp -d)
  curl -fsSL https://github.com/espressif/esp-rom-elfs/releases/download/20260528/esp-rom-elfs-20260528.tar.gz -o "$tmp/rom.tgz"
  tar -xzf "$tmp/rom.tgz" -C "$tmp"
  cp "$(find "$tmp" -name esp32s31_rev0_rom.elf | head -1)" "$OUT/"
  rm -rf "$tmp"
fi
echo "$ROM_SHA  $OUT/esp32s31_rev0_rom.elf" | shasum -a 256 -c

cat > "$OUT/docker/Dockerfile" <<'EOF'
FROM debian:bookworm
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential git wget curl cpio unzip rsync bc file python3 python3-pip python3-venv \
    libssl-dev libncurses-dev ca-certificates xz-utils bzip2 gzip patch perl device-tree-compiler \
    python3-dev python3-setuptools swig libgnutls28-dev uuid-dev zlib1g-dev pkg-config fakeroot \
    python3-pyelftools python3-yaml locales && rm -rf /var/lib/apt/lists/*
RUN python3 -m venv /opt/esptool && /opt/esptool/bin/pip install --no-cache-dir git+https://github.com/espressif/esptool.git@v5.6.0
RUN useradd -m br
USER br
EOF
docker build -q -t sbc-emu-s31-buildroot "$OUT/docker"

cat > "$OUT/docker/build.sh" <<EOF
#!/bin/bash
set -ex
git config --global --add safe.directory '*'
cd /work
[ -d buildroot ] || git clone --depth 1 -b 2025.02.x https://gitlab.com/buildroot.org/buildroot.git
if [ ! -d external ]; then
  git clone -b buildroot/v2025.02-esp32s31 https://github.com/espressif/esp-buildroot-external.git external
  git -C external checkout -q $BR_EXTERNAL_REV
fi
mkdir -p src
for r in "u-boot integration/v2024.07-esp32s31 $UBOOT_REV" "linux integration/v6.18-esp32s31 $LINUX_REV"; do
  set -- \$r
  if [ ! -d src/\$1 ]; then
    git clone --depth 50 -b \$2 https://github.com/espressif/\$1.git src/\$1
    git -C src/\$1 checkout -q \$3
  fi
done
[ -f output/.config ] || make -C buildroot BR2_EXTERNAL=/work/external O=/work/output espressif_esp32s31_function_core_board_nor_defconfig
printf 'UBOOT_OVERRIDE_SRCDIR = /work/src/u-boot\nLINUX_OVERRIDE_SRCDIR = /work/src/linux\n' > output/local.mk
ESP_ESPTOOL=/opt/esptool/bin/esptool PATH=/opt/esptool/bin:\$PATH make -C buildroot O=/work/output -j"\$(nproc)"
cp -a output/images/. /out/
EOF
chmod +x "$OUT/docker/build.sh"
docker volume create sbc-emu-s31 >/dev/null
docker run --rm -v sbc-emu-s31:/work --user root sbc-emu-s31-buildroot chown br /work
docker run --rm -v sbc-emu-s31:/work -v "$OUT/images":/out -v "$OUT/docker/build.sh":/build.sh:ro \
  --user br sbc-emu-s31-buildroot /build.sh
ls -l "$OUT/images/s31_full_flash.bin"
