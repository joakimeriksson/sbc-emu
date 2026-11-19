#!/bin/sh
# Cross-builds the PowerVR Vulkan tests for riscv64 (glibc) in Docker:
#   tests/gpu/pvr/build.sh  ->  tests/gpu/pvr/out/vkcompute
set -eu
cd "$(dirname "$0")"
mkdir -p out
docker run --rm -v "$PWD":/src debian:trixie sh -c '
  apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq gcc-riscv64-linux-gnu libvulkan-dev glslang-tools xxd >/dev/null
  cd /src && glslangValidator -V --target-env vulkan1.1 -o out/fill.spv fill.comp >/dev/null
  (cd out && xxd -i fill.spv > fill.spv.h)
  riscv64-linux-gnu-gcc -O2 -Wall -Iout -o out/vkcompute vkcompute.c -ldl'
ls -l out/vkcompute
