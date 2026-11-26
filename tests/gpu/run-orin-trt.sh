#!/bin/sh
# YOLOv8n through JetPack 6's TensorRT 10 on the emulated Orin: trtexec builds an FP32 engine from the
# ONNX file (TensorRT's own kernels: cuDNN and cuBLAS tactics off, no TF32), runs one inference on
# the bus image, and tests/gpu/orin/trtpost.c decodes the boxes, which should match ONNX Runtime's
# on the host (FIXTURE/yolo/reference.txt). The Nano's version is docs/jetson-nano-cuda.md.
#
# Prepare the model and input on the host first (FIXTURE/yolo):
#   cd cache/jetson-r3644/yolo && uv run --with ultralytics yolo export model=yolov8n.pt format=onnx imgsz=640 opset=12
#   curl -LO https://ultralytics.com/images/bus.jpg && cd -
#   uv run --with opencv-python-headless --with onnxruntime python tests/gpu/l4t/yolo-input.py \
#       cache/jetson-r3644/yolo/bus.jpg cache/jetson-r3644/yolo > cache/jetson-r3644/yolo/reference.txt
# Usage: tests/gpu/run-orin-trt.sh [FIXTURE]   (SBC_TRT_TIMEOUT: seconds of emulated time, default 7200;
# SBC_TRT_YOLO: another image's folder from yolo-input.py, default FIXTURE/yolo)
set -eu
cd "$(dirname "$0")/../.."
fixture=${1:-cache/jetson-r3644}
emulator=${SBC_EMU:-target/release/sbc-emu}
yolo=${SBC_TRT_YOLO:-$fixture/yolo}
test -s "$fixture/Image"
test -s "$fixture/probe-initramfs.cpio.gz"
for f in yolov8n.onnx input.bin input.scale reference.txt; do test -s "$yolo/$f"; done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root/modules" "$work/root/lib" "$work/root/y" "$work/packages" "$fixture/userspace-debs"

version=36.4.4-20250616085344
nvidia=https://repo.download.nvidia.com/jetson
debian=https://deb.debian.org/debian/pool/main
fetch() {
    path=$1
    url=$2
    hash=$3
    if [ ! -s "$path" ]; then
        curl -fL --retry 3 -o "$work/download.deb" "$url"
        printf '%s  %s\n' "$hash" "$work/download.deb" | shasum -a 256 -c -
        mv "$work/download.deb" "$path"
    fi
    printf '%s  %s\n' "$hash" "$path" | shasum -a 256 -c - >/dev/null
    data=$(bsdtar -tf "$path" | sed -n '/^data\.tar/p')
    bsdtar -xOf "$path" "$data" > "$work/data.tar"
    bsdtar -xf "$work/data.tar" -C "$work/packages"
}
D=$fixture/downloads
# NVIDIA's driver userspace (as tests/gpu/run-orin-cuda.sh), glibc, libstdc++ and TensorRT
fetch "$D/nvidia-l4t-core_${version}_arm64.deb" "$nvidia/t234/pool/main/n/nvidia-l4t-core/nvidia-l4t-core_${version}_arm64.deb" \
    04975607d121dd679a9f026939d5c126dd9e682bbba6b71c01942212ebc2b090
fetch "$D/nvidia-l4t-cuda_${version}_arm64.deb" "$nvidia/t234/pool/main/n/nvidia-l4t-cuda/nvidia-l4t-cuda_${version}_arm64.deb" \
    b7bcb9d5b6c10347b1c4d05cd2540edc5e69f438d9bd249fff00c536f7bea0cb
fetch "$D/nvidia-l4t-3d-core_${version}_arm64.deb" "$nvidia/t234/pool/main/n/nvidia-l4t-3d-core/nvidia-l4t-3d-core_${version}_arm64.deb" \
    12440f0eff87ff22906726fdad61d87a2681bf8ace9d824b8b208e0ee8dfdbcc
fetch "$D/nvidia-l4t-dla-compiler_${version}_arm64.deb" \
    "$nvidia/common/pool/main/n/nvidia-l4t-dla-compiler/nvidia-l4t-dla-compiler_${version}_arm64.deb" \
    01f0453a154e077a468c9bfb9b19c817cb130d207cd82d1d57487ffffe7c65b9
fetch "$fixture/userspace-debs/libc6.deb" "$debian/g/glibc/libc6_2.36-9+deb12u14_arm64.deb" \
    01f4330719fd4f65580e16ea5a0527f372fca750e8f588d26deaf09f2d3b1cf4
fetch "$fixture/userspace-debs/libc6-dev.deb" "$debian/g/glibc/libc6-dev_2.36-9+deb12u14_arm64.deb" \
    8bc395bf2ee9939f1a6ae395f974cffdcf541d98f99adfe89ce7e4bef36d159e
fetch "$D/libstdc++6_12.2.0-14+deb12u1_arm64.deb" "$debian/g/gcc-12/libstdc++6_12.2.0-14+deb12u1_arm64.deb" \
    26e138c677a985775331373828a6c286c551ff397cb735d00e2383cb273d1cb2
fetch "$D/libgcc-s1_12.2.0-14+deb12u1_arm64.deb" "$debian/g/gcc-12/libgcc-s1_12.2.0-14+deb12u1_arm64.deb" \
    576926b283613db80168ddf76380a3bd877602778cf0d226caa7bfbfa71eacf3
common=$nvidia/common/pool/main
fetch "$D/cuda-cudart-12-6_12.6.68-1_arm64.deb" "$common/c/cuda-cudart/cuda-cudart-12-6_12.6.68-1_arm64.deb" \
    54456ed3372670d6a6d7e19d5971ec234800fe620dfb822aa751bc7d80f2a516
fetch "$D/libcudla-12-6_12.6.68-1_arm64.deb" "$common/libc/libcudla/libcudla-12-6_12.6.68-1_arm64.deb" \
    bc14ff94b6211e7536b54faa6a7f38bb67a919671869c22d13b2bcae2146887c
trt=10.3.0.30-1+cuda12.5
while read -r name hash; do
    fetch "$D/${name}_${trt}_arm64.deb" "$common/t/tensorrt/${name}_${trt}_arm64.deb" "$hash"
done <<EOF
libnvinfer10 fabaad3ac8a50cf25da83a25938844808be4a3a43c2e76ea0b70b9cd2c6234bc
libnvinfer-plugin10 709a8b3111e4d5966abaa806de3282c59586e175f3db9898f3f2a3bf93a5886b
libnvonnxparsers10 042e21f28a271934d3c497504b41533e25f044893474e7567e3042e803384950
libnvinfer-bin 98494de17fc6596475141fa342ab34ce05f6c19d34769cf686628e16398484b7
EOF

modules() {
    name=$1
    shift
    bsdtar -xOf "$D/${name}_5.15.148-tegra-${version}_arm64.deb" data.tar.zst > "$work/modules.tar.zst"
    bsdtar -xf "$work/modules.tar.zst" -C "$work/root/modules" -s ',.*/,,' "$@"
}
modules nvidia-l4t-kernel --include '*/drm.ko' --include '*/drm_kms_helper.ko' \
    --include '*/cec.ko' --include '*/governor_userspace.ko'
modules nvidia-l4t-kernel-oot-modules --include '*/tegra-drm.ko' --include '*/nvhwpm.ko' \
    --include '*/tegra_wmark.ko' --include '*/host1x-nvhost.ko' --include '*/host1x-fence.ko'

P=$work/packages
lib=$P/lib/aarch64-linux-gnu
usr_lib=$P/usr/lib/aarch64-linux-gnu
cuda=$P/usr/local/cuda-12.6/targets/aarch64-linux/lib
for name in ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1 libgcc_s.so.1; do
    cp "$lib/$name" "$work/root/lib/"
done
cp "$usr_lib/libstdc++.so.6.0.30" "$work/root/lib/libstdc++.so.6"
for name in nvcucompat nvrm_gpu nvrm_mem nvrm_chip nvrm_sync nvrm_host1x nvos nvsocsys nvtegrahv nvsciipc nvcudla \
    nvdla_compiler nvdla_runtime; do
    cp "$usr_lib/nvidia/lib$name.so" "$work/root/lib/"
done
cp "$usr_lib/nvidia/libcuda.so.1.1" "$work/root/lib/libcuda.so.1"
ln -s libcuda.so.1 "$work/root/lib/libcuda.so"  # TensorRT opens the unversioned name
cp "$usr_lib/nvidia/libnvidia-ptxjitcompiler.so.540.4.0" "$work/root/lib/libnvidia-ptxjitcompiler.so.1"
cp "$cuda/libcudart.so.12.6.68" "$work/root/lib/libcudart.so.12"
cp "$cuda/libcudla.so.1.0.0" "$work/root/lib/libcudla.so.1"
for name in libnvinfer libnvinfer_plugin libnvonnxparser; do
    cp "$usr_lib/$name.so.10.3.0" "$work/root/lib/$name.so.10"
done
cp "$usr_lib/libnvinfer_builder_resource.so.10.3.0" "$work/root/lib/"
cp "$P/usr/src/tensorrt/bin/trtexec" "$work/root/trtexec"
cp "$yolo/yolov8n.onnx" "$yolo/input.bin" "$work/root/y/"

rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
"${CLANG:-clang}" --target=aarch64-linux-gnu -fPIE -O2 -Wall -Werror -nostdinc \
    -isystem "$("${CLANG:-clang}" -print-resource-dir)/include" -isystem "$P/usr/include" \
    -isystem "$P/usr/include/aarch64-linux-gnu" -c tests/gpu/orin/trtpost.c -o "$work/trtpost.o"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$lld" -flavor gnu -m aarch64elf -pie --dynamic-linker /lib/ld-linux-aarch64.so.1 \
    "$usr_lib/Scrt1.o" "$usr_lib/crti.o" "$work/trtpost.o" "$usr_lib/crtn.o" \
    -L "$lib" -l:libc.so.6 -o "$work/root/trtpost"
# uncompressed: the guest's kernel would spend minutes inflating 400 MB
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 > "$work/trt.cpio")
{ gzip -dc "$fixture/probe-initramfs.cpio.gz"; cat "$work/trt.cpio"; } > "$work/initramfs.cpio"

scale=$(cat "$yolo/input.scale")
sed -e '$d' -e 's/host1x mc-utils/host1x host1x-fence host1x-nvhost mc-utils/' \
    -e 's/nvmap nvgpu/nvmap nvgpu cec drm drm_kms_helper nvhwpm tegra_wmark governor_userspace tegra-drm/' \
    scripts/jetson-orin-nano-gpu-power.txt > "$work/trt.txt"
# an interpreted kernel takes longer than nvgpu's channel watchdog allows
cat >> "$work/trt.txt" <<EOF
serial mount -t debugfs none /sys/kernel/debug 2>/dev/null; for t in /sys/kernel/debug/*gpu*/timeouts_enabled; do echo 0 > \$t; done; mkdir -p /tmp; mount -t tmpfs tmp /tmp; export LD_LIBRARY_PATH=/lib; echo "TRT_" "READY"\\r
wait-serial TRT_ READY
serial /trtexec --onnx=/y/yolov8n.onnx --memPoolSize=workspace:256M --noTF32 --builderOptimizationLevel=0 --tacticSources=-CUDNN,-CUBLAS,-CUBLAS_LT,-EDGE_MASK_CONVOLUTIONS,-JIT_CONVOLUTIONS --loadInputs=images:/y/input.bin --exportOutput=/tmp/out.json --iterations=1 --warmUp=0 --duration=0 --avgRuns=1 2>&1 | grep -E "Engine built|GPU Compute Time: min|PASSED|FAILED|\\[E\\]|rror|not found"; /trtpost /tmp/out.json $scale; echo "TRT_" "STATUS=\$?"\\r
wait-serial TRT_ STATUS=
quit
EOF

"$emulator" run --board jetson-orin-nano-4gb --gpu on \
    --kernel "$fixture/Image" --initrd "$work/initramfs.cpio" \
    --script "$work/trt.txt" --timeout "${SBC_TRT_TIMEOUT:-7200}" > "$work/boot.log" 2>&1 || true
cp "$work/boot.log" "$fixture/trt-boot.log"
grep -E '^(DET |TRT_ STATUS|.*(Engine built|GPU Compute Time|PASSED|FAILED))' "$work/boot.log" || true
grep '^DET ' "$work/boot.log" | diff - "$yolo/reference.txt" && echo "TRT_YOLO MATCH" || {
    tail -40 "$work/boot.log"
    exit 1
}
