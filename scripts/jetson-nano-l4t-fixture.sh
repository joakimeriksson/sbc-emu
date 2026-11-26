#!/bin/sh
# NVIDIA's Linux for Tegra (L4T R32.7.3, JetPack 4.6.3) for the emulated Jetson Nano: its kernel,
# device tree, modules (nvgpu), GPU firmware and CUDA driver (libcuda), and TensorRT 8.2 with
# cuDNN, cuBLAS and the CUDA runtime from NVIDIA's Jetson apt repository. Downloaded and checked
# (SHA-256) into OUT; OUT/serve is what tests/gpu/l4t/prepare.sh fetches from the guest. These are
# NVIDIA's files under NVIDIA's licence: they stay in cache/ and are never committed.
# Usage: scripts/jetson-nano-l4t-fixture.sh [OUT]   (default the repository's cache/l4t);
# docs/jetson-nano-cuda.md
set -eu
REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$REPO/cache/l4t}
mkdir -p "$OUT/serve"
get() { # get NAME URL SHA256
  if [ ! -f "$OUT/$1" ]; then
    curl -fL --retry 3 -o "$OUT/$1.part" "$2"
    mv "$OUT/$1.part" "$OUT/$1"
  fi
  actual=$(shasum -a 256 "$OUT/$1" | cut -d' ' -f1)
  [ "$actual" = "$3" ] || { echo "$1: sha256 $actual, expected $3 (delete it to fetch it again)" >&2; exit 1; }
}
get l4t.tbz2 https://developer.nvidia.com/downloads/remetpack-463r32releasev73t210jetson-210linur3273aarch64tbz2 \
  3e7c304b56c2c9f2ca865d560d22e8ab3d99d2e79d782342fb0fb1c4a5e7946e
R=https://repo.download.nvidia.com/jetson/common/pool/main
while read -r deb path sha; do
  get "$deb" "$R/$path/$deb" "$sha"
  cp "$OUT/$deb" "$OUT/serve/$deb"
done <<'DEBS'
libnvinfer8_8.2.1-1+cuda10.2_arm64.deb t/tensorrt 13bc5c1e28a537972a24f7573b7196d399f51f2badf069afd2ed670b41868414
libnvinfer-bin_8.2.1-1+cuda10.2_arm64.deb t/tensorrt b03b5ca8e9b0df42d97255cea501cab44e982f9eb5e795e8bd8cab70a1debd85
libnvonnxparsers8_8.2.1-1+cuda10.2_arm64.deb t/tensorrt 440892db8de481677f5b91e78ca8f0a202e3f2447e0fa7e508870f72acf9e071
libnvparsers8_8.2.1-1+cuda10.2_arm64.deb t/tensorrt a1d9074b39fd589097c8fa95a0354baabc97a5395e0e1aa6d07ada0e0c3a0fcd
libnvinfer-plugin8_8.2.1-1+cuda10.2_arm64.deb t/tensorrt 2ee98a01a84bb6219e8e874be429006e12d4125257c3f3af73f56579f68a9b4f
libcudnn8_8.2.1.32-1+cuda10.2_arm64.deb c/cudnn 4c1619640e5411fb53e87828c62ff429daa608c8f02efb96460b43f743d64bb8
libcublas10_10.2.3.300-1_arm64.deb libc/libcublas a4cde28215c01bce78fa7c3d09909afaec487142e1144646bd7ff043a5c6c1df
cuda-cudart-10-2_10.2.300-1_arm64.deb c/cuda-cudart e3cd683965f7b2e4a13b27c58754443185dcc545d0f989e52c224840cfde48d1
DEBS
if [ ! -f "$OUT/serve/Image" ]; then
  rm -rf "$OUT/.unpack" && mkdir "$OUT/.unpack"
  tar -xjf "$OUT/l4t.tbz2" -C "$OUT/.unpack" Linux_for_Tegra/kernel/Image \
    Linux_for_Tegra/kernel/dtb/tegra210-p3448-0000-p3449-0000-b00.dtb \
    Linux_for_Tegra/kernel/kernel_supplements.tbz2 Linux_for_Tegra/nv_tegra/nvidia_drivers.tbz2
  L=$OUT/.unpack/Linux_for_Tegra
  mv "$L/kernel/dtb/tegra210-p3448-0000-p3449-0000-b00.dtb" "$L/kernel/kernel_supplements.tbz2" \
    "$L/nv_tegra/nvidia_drivers.tbz2" "$OUT/serve/"
  mv "$L/kernel/Image" "$OUT/serve/Image"
  rm -rf "$OUT/.unpack"
fi
cp "$REPO"/tests/gpu/l4t/*.sh "$REPO"/tests/gpu/l4t/*.c "$REPO"/tests/gpu/l4t/*.py "$OUT/serve/"
echo "$OUT/serve"
