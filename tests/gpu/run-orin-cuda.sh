#!/bin/sh
# Run the original JetPack CUDA driver in the Orin GPU fixture.
set -eu
cd "$(dirname "$0")/../.."
fixture=${1:-cache/jetson-r3644}
emulator=${SBC_EMU:-target/release/sbc-emu}
test -s "$fixture/Image"
test -s "$fixture/probe-initramfs.cpio.gz"
work=$(mktemp -d)
trap 'rm -rf "${work:?}"' EXIT
mkdir -p "$work/root/modules" "$work/root/lib" "$work/packages" "$fixture/userspace-debs"

version=36.4.4-20250616085344
nvidia=https://repo.download.nvidia.com/jetson/t234/pool/main/n
glibc=https://deb.debian.org/debian/pool/main/g/glibc
fetch() {
    path=$1
    url=$2
    hash=$3
    if [ ! -s "$path" ]; then
        curl -fL --retry 3 -o "$work/download.deb" "$url"
        printf '%s  %s\n' "$hash" "$work/download.deb" | shasum -a 256 -c -
        mv "$work/download.deb" "$path"
    fi
    printf '%s  %s\n' "$hash" "$path" | shasum -a 256 -c -
    data=$(bsdtar -tf "$path" | sed -n '/^data\.tar/p')
    bsdtar -xOf "$path" "$data" > "$work/data.tar"
    bsdtar -xf "$work/data.tar" -C "$work/packages"
}
for name in nvidia-l4t-core nvidia-l4t-cuda nvidia-l4t-3d-core; do
    file="${name}_${version}_arm64.deb"
    case "$name" in
        nvidia-l4t-core) hash=04975607d121dd679a9f026939d5c126dd9e682bbba6b71c01942212ebc2b090 ;;
        nvidia-l4t-cuda) hash=b7bcb9d5b6c10347b1c4d05cd2540edc5e69f438d9bd249fff00c536f7bea0cb ;;
        nvidia-l4t-3d-core) hash=12440f0eff87ff22906726fdad61d87a2681bf8ace9d824b8b208e0ee8dfdbcc ;;
    esac
    fetch "$fixture/downloads/$file" "$nvidia/$name/$file" "$hash"
done
fetch "$fixture/userspace-debs/libc6.deb" "$glibc/libc6_2.36-9+deb12u14_arm64.deb" \
    01f4330719fd4f65580e16ea5a0527f372fca750e8f588d26deaf09f2d3b1cf4
fetch "$fixture/userspace-debs/libc6-dev.deb" "$glibc/libc6-dev_2.36-9+deb12u14_arm64.deb" \
    8bc395bf2ee9939f1a6ae395f974cffdcf541d98f99adfe89ce7e4bef36d159e

modules() {
    name=$1
    shift
    bsdtar -xOf "$fixture/downloads/${name}_5.15.148-tegra-${version}_arm64.deb" data.tar.zst \
        > "$work/modules.tar.zst"
    bsdtar -xf "$work/modules.tar.zst" -C "$work/root/modules" -s ',.*/,,' "$@"
}
modules nvidia-l4t-kernel --include '*/drm.ko' --include '*/drm_kms_helper.ko' \
    --include '*/cec.ko' --include '*/governor_userspace.ko'
modules nvidia-l4t-kernel-oot-modules --include '*/tegra-drm.ko' --include '*/nvhwpm.ko' \
    --include '*/tegra_wmark.ko' --include '*/host1x-nvhost.ko' --include '*/host1x-fence.ko'

lib="$work/packages/lib/aarch64-linux-gnu"
usr_lib="$work/packages/usr/lib/aarch64-linux-gnu"
for name in ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1; do
    cp "$lib/$name" "$work/root/lib/"
done
for name in nvcucompat nvrm_gpu nvrm_mem nvrm_chip nvrm_sync nvrm_host1x nvos nvsocsys nvtegrahv nvsciipc; do
    cp "$usr_lib/nvidia/lib$name.so" "$work/root/lib/"
done
cp "$usr_lib/nvidia/libcuda.so.1.1" "$work/root/lib/libcuda.so.1"
cp "$usr_lib/nvidia/libnvidia-ptxjitcompiler.so.540.4.0" "$work/root/lib/libnvidia-ptxjitcompiler.so.1"

rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
"${CLANG:-clang}" --target=aarch64-linux-gnu -fPIE -O2 -Wall -Wextra -Werror \
    -c tests/gpu/orin/cuda.c -o "$work/cuda.o"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$lld" -flavor gnu -m aarch64elf -pie --dynamic-linker /lib/ld-linux-aarch64.so.1 \
    "$usr_lib/Scrt1.o" "$usr_lib/crti.o" "$work/cuda.o" "$usr_lib/crtn.o" \
    -L "$lib" -l:libc.so.6 -o "$work/root/cuda-probe"
# the Nano's vector add (tests/gpu/l4t): 4096 threads in 32 workgroups, PTX JIT-compiled for SM87
"${CLANG:-clang}" --target=aarch64-linux-gnu -fPIE -O2 -Wall -Werror -nostdinc \
    -isystem "$("${CLANG:-clang}" -print-resource-dir)/include" -isystem "$work/packages/usr/include" \
    -isystem "$work/packages/usr/include/aarch64-linux-gnu" -c tests/gpu/l4t/cuda-vadd.c -o "$work/cuda-vadd.o"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$lld" -flavor gnu -m aarch64elf -pie --dynamic-linker /lib/ld-linux-aarch64.so.1 \
    "$usr_lib/Scrt1.o" "$usr_lib/crti.o" "$work/cuda-vadd.o" "$usr_lib/crtn.o" \
    -L "$lib" -l:libc.so.6 -o "$work/root/cuda-vadd"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 > "$work/cuda.cpio")
gzip -n "$work/cuda.cpio"
cat "$fixture/probe-initramfs.cpio.gz" "$work/cuda.cpio.gz" > "$work/initramfs.cpio.gz"

sed -e '$d' -e 's/host1x mc-utils/host1x host1x-fence host1x-nvhost mc-utils/' \
    -e 's/nvmap nvgpu/nvmap nvgpu cec drm drm_kms_helper nvhwpm tegra_wmark governor_userspace tegra-drm/' \
    scripts/jetson-orin-nano-gpu-power.txt > "$work/cuda.txt"
printf 'serial /cuda-probe; echo "CUDA_PROBE_" "STATUS=$?"; /cuda-vadd; echo "CUDA_VADD_" "STATUS=$?"\\r\n' >> "$work/cuda.txt"
printf 'wait-serial CUDA_VADD_ STATUS=\nquit\n' >> "$work/cuda.txt"

if [ "${SBC_ORIN_WASM:-0}" = 1 ]; then
    node tests/gpu/orin/wasm-linux.mjs "$fixture/Image" "$work/initramfs.cpio.gz" "$work/cuda.txt" cuda
    exit
fi

"$emulator" run --board jetson-orin-nano-4gb --gpu on \
    --kernel "$fixture/Image" --initrd "$work/initramfs.cpio.gz" \
    --script "$work/cuda.txt" --timeout 180 > "$work/boot.log" 2>&1 || {
        cp "$work/boot.log" "$fixture/cuda-boot.log"
        tail -80 "$work/boot.log"
        exit 1
    }
cp "$work/boot.log" "$fixture/cuda-boot.log"
# every line must be there (one alternation would pass on any single one)
for want in 'cuInit=0' 'CUDA device: Orin' 'CUDA compute capability: 8\.7' 'cuCtxCreate=0' 'cuMemAlloc=0' \
    'cuMemcpyHtoD=0' 'cuMemcpyDtoH=0 data=0xfeedcafe' 'cuLaunchKernel=0' 'cuCtxSynchronize=0' \
    'CUDA kernel output=0x24681357 status=0'; do
    grep -Eq "^$want[[:space:]]*\$" "$work/boot.log" || {
        echo "missing: $want"
        tail -50 "$work/boot.log"
        exit 1
    }
done
grep -E '^CUDA_PROBE_ STATUS=0[[:space:]]*$|^CUDA_VADD OK' "$work/boot.log" &&
    grep -Eq '^CUDA_VADD_ STATUS=0[[:space:]]*$' "$work/boot.log" || {
    tail -50 "$work/boot.log"
    exit 1
}
