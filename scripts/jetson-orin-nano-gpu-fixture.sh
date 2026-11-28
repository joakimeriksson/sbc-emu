#!/bin/sh
# Build the R36.4.4 nvgpu fixture using the Orin Alpine serial shell.
# Usage: sh scripts/jetson-orin-nano-gpu-fixture.sh [OUT] [ALPINE_INITRAMFS]
set -eu

out=${1:-cache/jetson-r3644}
initramfs=${2:-cache/jetson-orin-nano/initramfs.cpio.gz}
mirror=https://repo.download.nvidia.com/jetson/t234/pool/main/n
version=5.15.148-tegra-36.4.4-20250616085344
kernel=nvidia-l4t-kernel
modules=nvidia-l4t-kernel-oot-modules
firmware=nvidia-l4t-firmware

test -s "$initramfs"
mkdir -p "$out/downloads"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() {
    name=$1
    expected=$2
    file="${name}_${3}_arm64.deb"
    if [ ! -f "$out/downloads/$file" ]; then
        curl -fL --retry 3 -o "$work/$file" "$mirror/$name/$file"
        printf '%s  %s\n' "$expected" "$work/$file" | shasum -a 256 -c -
        mv "$work/$file" "$out/downloads/$file"
    else
        printf '%s  %s\n' "$expected" "$out/downloads/$file" | shasum -a 256 -c -
    fi
    bsdtar -xOf "$out/downloads/$file" data.tar.zst > "$work/$name.tar.zst"
}

fetch "$kernel" f26ccf85c64f9d4c36001b0d04df8c8bdd370aa173ebdcacb4f4fc3995420500 "$version"
fetch "$modules" 12eb3890197b66a2a8b632ac160ef385ae8627bc4d0f6156af84afb2f7f8181b "$version"
fetch "$firmware" 08407971957c6e454d0e53d90cf1a15cda85a59aa9da00fb3cca9c4759cd2b17 "${version#5.15.148-tegra-}"
bsdtar -xOf "$work/$kernel.tar.zst" ./boot/Image > "$work/Image"

mkdir -p "$work/root/modules"
bsdtar -xf "$work/$modules.tar.zst" -C "$work/root/modules" -s ',.*/,,' \
    --include '*/host1x.ko' --include '*/mc-utils.ko' --include '*/nvsciipc.ko' \
    --include '*/nvmap.ko' --include '*/nvgpu.ko' --include '*/governor_pod_scaling.ko'
for module in host1x mc-utils nvsciipc nvmap governor_pod_scaling nvgpu; do
    test -s "$work/root/modules/$module.ko"
done
bsdtar -xf "$work/$firmware.tar.zst" -C "$work/root" --include './lib/firmware/nvidia/ga10b/*'
test -s "$work/root/lib/firmware/nvidia/ga10b/NETC_img_prod_encrypted.bin"
(
    cd "$work/root"
    find modules lib | LC_ALL=C sort | cpio -o -H newc -R 0:0 > "$work/gpu.cpio"
)
gzip -n "$work/gpu.cpio"
cat "$initramfs" "$work/gpu.cpio.gz" > "$work/probe-initramfs.cpio.gz"
mv "$work/Image" "$work/probe-initramfs.cpio.gz" "$out/"
