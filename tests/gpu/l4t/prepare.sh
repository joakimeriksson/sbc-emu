#!/bin/sh
# Guest side, once, in Debian on the Jetson Nano's card (scripts/jetson-nano-l4t-prepare.txt):
# NVIDIA's L4T userspace, firmware and modules into the card, the CUDA test built, TensorRT
# unpacked; everything from the host's OUT/serve of scripts/jetson-nano-l4t-fixture.sh.
# Any failure stops it before PREPARE_OK.
set -eu
H=${L4T_SERVER:-http://10.0.2.2:8800}
# a download reaches its name only when it is complete
get() {
    python3 -c "import urllib.request as u,sys; d=u.urlopen(sys.argv[1]).read(); open(sys.argv[2]+'.part','wb').write(d)" "$H/$1" "$2"
    mv "$2.part" "$2"
}
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update
apt-get -y -qq install --no-install-recommends bzip2 gcc libc6-dev python3-numpy cloud-guest-utils libegl1 >/dev/null
# the root fills the card (made larger on the host: NVIDIA's files take about 2 GB); growpart
# fails harmlessly when the partition fills the card already
growpart /dev/mmcblk0 1 >/dev/null || true
resize2fs /dev/mmcblk0p1 >/dev/null 2>&1 || true
free_mb=$(df -m / | awk 'NR == 2 { print $4 }')
if [ "$free_mb" -lt 3000 ]; then
    echo "only $free_mb MB free on the card: make it larger on the host (truncate -s 8G)" >&2
    exit 1
fi
mkdir -p /opt/l4t /opt/l4t/y
cd /opt/l4t
# libcuda and the rest of the GPU userspace, the GPU firmware
get nvidia_drivers.tbz2 /tmp/nv.tbz2
tar xjf /tmp/nv.tbz2
rm /tmp/nv.tbz2
mkdir -p /usr/lib/firmware
cp -r lib/firmware/gm20b lib/firmware/tegra21x /usr/lib/firmware/
# (NVIDIA ships no CDE firmware: its names only point at each other)
rm -f /usr/lib/firmware/gm20b/gpu2cde.bin /usr/lib/firmware/tegra21x/gpu2cde.bin
T=/opt/l4t/usr/lib/aarch64-linux-gnu/tegra
ln -sf libnvidia-ptxjitcompiler.so.440.18 $T/libnvidia-ptxjitcompiler.so.1
ln -sf libcuda.so.1.1 $T/libcuda.so.1
# the kernel's modules (nvgpu)
get kernel_supplements.tbz2 /tmp/ks.tbz2
tar xjf /tmp/ks.tbz2 -C /usr
rm /tmp/ks.tbz2
# TensorRT, cuDNN, cuBLAS, the CUDA runtime
for d in libnvinfer8_8.2.1-1+cuda10.2_arm64.deb libnvinfer-bin_8.2.1-1+cuda10.2_arm64.deb \
    libnvonnxparsers8_8.2.1-1+cuda10.2_arm64.deb libnvparsers8_8.2.1-1+cuda10.2_arm64.deb \
    libnvinfer-plugin8_8.2.1-1+cuda10.2_arm64.deb libcudnn8_8.2.1.32-1+cuda10.2_arm64.deb \
    libcublas10_10.2.3.300-1_arm64.deb cuda-cudart-10-2_10.2.300-1_arm64.deb; do
    get "$d" /tmp/d.deb
    dpkg-deb -x /tmp/d.deb /opt/l4t
    rm /tmp/d.deb
done
# the tests
for f in env.sh cuda-vadd.c cuda.sh trt.sh trtpost.py; do
    get "$f" "/opt/l4t/$f"
done
gcc -O1 -o /opt/l4t/cuda-vadd /opt/l4t/cuda-vadd.c -ldl
# YOLOv8n and its input, if the host has them (docs/jetson-nano-cuda.md); TensorRT's test needs them
for f in yolov8n.onnx input.bin input.scale; do
    get "$f" "/opt/l4t/y/$f" 2>/dev/null || echo "no $f on the host: TensorRT's test needs it"
done
echo PREPARE_""OK
