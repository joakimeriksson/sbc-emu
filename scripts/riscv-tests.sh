#!/bin/sh
# Build the riscv-tests ISA suites (physical-memory "-p" variants) for riscv64's
# `cargo test -p riscv64 --test isa`, in Docker (Debian's riscv64-unknown-elf GCC):
#   cache/riscv-tests/isa/rv64*-p-*
# Usage: scripts/riscv-tests.sh
set -eu
cd "$(dirname "$0")/.."
OUT=$PWD/cache/riscv-tests
REV=3df703cd52d5f0d8224e3c50bf03c6cb70efff98   # riscv-software-src/riscv-tests master, 2026-11-18
mkdir -p "$OUT/isa"
cat > "$OUT/build.sh" <<SCRIPT
#!/bin/sh
set -ex
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc gcc-riscv64-unknown-elf git make autoconf >/dev/null
cd /tmp && git clone -q https://github.com/riscv-software-src/riscv-tests.git && cd riscv-tests
git checkout -q $REV && git submodule update -q --init --recursive
autoconf && ./configure --with-xlen=64 >/dev/null
# -k: the "-v" variants and other ISAs may fail to build; only the -p binaries are used
make -k -C isa -j8 RISCV_PREFIX=riscv64-unknown-elf- XLEN=64 >/dev/null 2>&1 || true
find isa -maxdepth 1 -type f -name 'rv64*-p-*' ! -name '*.dump' -exec cp {} /out/isa/ \;
SCRIPT
chmod +x "$OUT/build.sh"
docker run --rm -v "$OUT":/out debian:trixie /out/build.sh
ls "$OUT/isa" | wc -l
