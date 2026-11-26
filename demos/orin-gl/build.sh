#!/bin/sh
# Build the Orin GL demo: demos/orin-gl/cube.c on NVIDIA's stock JetPack GLES driver, shown
# over VNC through the Linux framebuffer. Needs the EGL fixture (tests/gpu/run-orin-egl.sh)
# and the Debian headers the GL suite fetches (tests/gpu/run-orin-gl.sh).
# Run:  target/release/sbc-emu run --board jetson-orin-nano-4gb --gpu on \
#         --kernel cache/jetson-r3644/Image --initrd cache/jetson-r3644/gl-demo.cpio.gz \
#         --script cache/jetson-r3644/gl-demo.txt --vnc :7,password=orin --timeout 86400
set -eu
cd "$(dirname "$0")/../.."
fixture=cache/jetson-r3644
debs=$fixture/userspace-debs
test -s "$fixture/egl-initramfs.cpio.gz" || sh tests/gpu/run-orin-egl.sh > /dev/null
for name in libc6 libc6-dev linux-libc-dev; do
    test -s "$debs/$name.deb" || { echo "run tests/gpu/run-orin-gl.sh first (it fetches $name)"; exit 1; }
done
work=$(mktemp -d)
trap 'rm -rf "${work:?}"' EXIT
mkdir -p "$work/sysroot" "$work/root"
for name in libc6 libc6-dev linux-libc-dev; do
    data=$(bsdtar -tf "$debs/$name.deb" | sed -n '/^data\.tar/p')
    bsdtar -xOf "$debs/$name.deb" "$data" | bsdtar -xf - -C "$work/sysroot"
done
sysroot=$work/sysroot
lib=$sysroot/lib/aarch64-linux-gnu
usr_lib=$sysroot/usr/lib/aarch64-linux-gnu
rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
"${CLANG:-clang}" --target=aarch64-linux-gnu --sysroot="$sysroot" -fPIE -O2 -Wall \
    -isystem "$sysroot/usr/include/aarch64-linux-gnu" -isystem "$sysroot/usr/include" \
    -c demos/orin-gl/cube.c -o "$work/cube.o"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
    "$lld" -flavor gnu -m aarch64elf -pie --dynamic-linker /lib/ld-linux-aarch64.so.1 \
    "$usr_lib/Scrt1.o" "$usr_lib/crti.o" "$work/cube.o" "$usr_lib/crtn.o" \
    -L "$lib" -l:libc.so.6 -l:libm.so.6 -o "$work/root/gl-cube"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null | gzip -n > "$work/demo.cpio.gz")
cat "$fixture/egl-initramfs.cpio.gz" "$work/demo.cpio.gz" > "$fixture/gl-demo.cpio.gz"
sed -e '$d' -e 's/host1x mc-utils/host1x host1x-fence host1x-nvhost mc-utils/' \
    -e 's/nvmap nvgpu/nvmap nvgpu cec drm drm_kms_helper nvhwpm tegra_wmark governor_userspace tegra-drm/' \
    scripts/jetson-orin-nano-gpu-power.txt > "$fixture/gl-demo.txt"
printf 'serial /gl-cube ${GL_DEMO_FRAMES:-0}\\r\n' | sed "s/\${GL_DEMO_FRAMES:-0}/${GL_DEMO_FRAMES:-0}/" >> "$fixture/gl-demo.txt"
printf 'wait-serial ORIN_GL_DEMO_ DONE\nquit\n' >> "$fixture/gl-demo.txt"
echo "built $fixture/gl-demo.cpio.gz and $fixture/gl-demo.txt"
