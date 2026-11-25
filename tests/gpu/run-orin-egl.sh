#!/bin/sh
# Run the stock JetPack EGL and OpenGL ES stack in the Orin GPU fixture: a pbuffer
# context, a red clear through the Ampere 3D class and a read back through the copy engine.
set -eu
cd "$(dirname "$0")/../.."
fixture=${1:-cache/jetson-r3644}
emulator=${SBC_EMU:-target/release/sbc-emu}
test -s "$fixture/Image"
test -s "$fixture/probe-initramfs.cpio.gz"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
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
glvnd=https://deb.debian.org/debian/pool/main/libg/libglvnd
fetch "$fixture/userspace-debs/libglvnd0.deb" "$glvnd/libglvnd0_1.6.0-1_arm64.deb" \
    d5063205fab5322ee686f33f87c6315b3fcd1335ea4e7bc8202b48ed4f9c9e83
fetch "$fixture/userspace-debs/libegl1.deb" "$glvnd/libegl1_1.6.0-1_arm64.deb" \
    707097a275155c600e2e9251c4ee7cdfdb2d8f50a678a850a2526a7bc9664166
fetch "$fixture/userspace-debs/libgles2.deb" "$glvnd/libgles2_1.6.0-1_arm64.deb" \
    efeb2717380f8411d66b3f669bb99bbd83ff09db3d4a1661533aa31af659608c

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
for name in nvdc nvimp nvos nvrm_chip nvrm_gpu nvrm_host1x nvrm_mem nvrm_surface nvrm_sync \
    nvsciipc nvsocsys nvtegrahv; do
    cp "$usr_lib/nvidia/lib$name.so" "$work/root/lib/"
done
for name in nvidia-eglcore nvidia-glsi nvidia-rmapi-tegra; do
    cp "$usr_lib/nvidia/lib$name.so.540.4.0" "$work/root/lib/"
done
cp "$usr_lib/tegra-egl/libEGL_nvidia.so.0" "$usr_lib/tegra-egl/libGLESv2_nvidia.so.2" "$work/root/lib/"
for name in libEGL.so.1 libGLESv2.so.2 libGLdispatch.so.0; do
    cp -L "$usr_lib/$name" "$work/root/lib/"
done
mkdir -p "$work/root/usr/share/glvnd/egl_vendor.d"
cp "$work/packages/usr/share/glvnd/egl_vendor.d/10_nvidia.json" "$work/root/usr/share/glvnd/egl_vendor.d/"

rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
"${CLANG:-clang}" --target=aarch64-linux-gnu -fPIE -O2 -Wall -Wextra -Werror \
    -c tests/gpu/orin/egl.c -o "$work/egl.o"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$lld" -flavor gnu -m aarch64elf -pie --dynamic-linker /lib/ld-linux-aarch64.so.1 \
    "$usr_lib/Scrt1.o" "$usr_lib/crti.o" "$work/egl.o" "$usr_lib/crtn.o" \
    -L "$lib" -l:libc.so.6 -o "$work/root/egl-probe"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 > "$work/egl.cpio")
gzip -n "$work/egl.cpio"
cat "$fixture/probe-initramfs.cpio.gz" "$work/egl.cpio.gz" > "$work/initramfs.cpio.gz"

sed -e '$d' -e 's/host1x mc-utils/host1x host1x-fence host1x-nvhost mc-utils/' \
    -e 's/nvmap nvgpu/nvmap nvgpu cec drm drm_kms_helper nvhwpm tegra_wmark governor_userspace tegra-drm/' \
    scripts/jetson-orin-nano-gpu-power.txt > "$work/egl.txt"
printf 'serial /egl-probe; echo "EGL_PROBE_" "STATUS=$?"\\r\n' >> "$work/egl.txt"
printf 'wait-serial EGL_PROBE_ STATUS=\nquit\n' >> "$work/egl.txt"

if [ "${SBC_ORIN_WASM:-0}" = 1 ]; then
    node tests/gpu/orin/wasm-linux.mjs "$fixture/Image" "$work/initramfs.cpio.gz" "$work/egl.txt" egl
    exit
fi

"$emulator" run --board jetson-orin-nano-4gb --gpu on \
    --kernel "$fixture/Image" --initrd "$work/initramfs.cpio.gz" \
    --script "$work/egl.txt" --timeout 120 > "$work/boot.log" 2>&1 || {
        tail -80 "$work/boot.log"
        exit 1
    }
grep -E '^eglInitialize=1 version=1\.5|^eglCreateContext=0x|^GL renderer: NVIDIA Tegra Orin|^GL pixel: 255 0 0 255[[:space:]]*$' "$work/boot.log"
grep -E '^EGL_PROBE_ STATUS=0[[:space:]]*$' "$work/boot.log" || {
    tail -50 "$work/boot.log"
    exit 1
}
