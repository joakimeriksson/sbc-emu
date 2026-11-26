#!/bin/sh
# The Jetson Nano's GL test suites (gles2test, gltest, gles32test) on the Orin: every scene
# rendered by NVIDIA's stock JetPack driver on the emulated GA10B and by Mesa's llvmpipe in the
# same guest (glvnd picks the vendor), then compared per pixel with the programs' own `cmp`.
# Builds on the EGL fixture (tests/gpu/run-orin-egl.sh), which it runs first if needed.
#   sh tests/gpu/run-orin-gl.sh [SUITE...]     (default: all three)
# SBC_GL_SCENE=NAME renders only that scene. SBC_GL_DUMP="scene ..." copies those scenes' comparison images (GPU | software | difference)
# out of the guest into $SBC_GL_OUT (default cache/jetson-r3644/gl-out) as PPM.
set -eu
cd "$(dirname "$0")/../.."
fixture=cache/jetson-r3644
emulator=${SBC_EMU:-target/release/sbc-emu}
suites=${*:-gles2test gltest gles32test}
test -s "$fixture/egl-initramfs.cpio.gz" || sh tests/gpu/run-orin-egl.sh > /dev/null
work=$(mktemp -d)
trap 'rm -rf "${work:?}"' EXIT
debs="$fixture/userspace-debs"
mkdir -p "$debs" "$work/root/lib" "$work/sysroot" "$work/mesa"

# Debian 12 (bookworm) arm64: Mesa's software renderer with what it loads, and the headers.
debian=https://deb.debian.org/debian
while read -r name path hash kind; do
    file="$debs/$name.deb"
    if [ ! -s "$file" ]; then
        curl -fL --retry 3 -o "$work/download.deb" "$debian/$path"
        printf '%s  %s\n' "$hash" "$work/download.deb" | shasum -a 256 -c - > /dev/null
        mv "$work/download.deb" "$file"
    fi
    printf '%s  %s\n' "$hash" "$file" | shasum -a 256 -c - > /dev/null
    data=$(bsdtar -tf "$file" | sed -n '/^data\.tar/p')
    case "$kind" in
        run) dest="$work/mesa" ;;
        dev) dest="$work/sysroot" ;;
    esac
    bsdtar -xOf "$file" "$data" | bsdtar -xf - -C "$dest"
done <<'EOF'
libegl-mesa0 pool/main/m/mesa/libegl-mesa0_22.3.6-1+deb12u2_arm64.deb fe41047fbd6b823f56d87d2fe4cec28f9058985862f3aac828971624a37f8871 run
libgl1-mesa-dri pool/main/m/mesa/libgl1-mesa-dri_22.3.6-1+deb12u2_arm64.deb 2381957e885754c0aa59ffef0a3671ad4aeb9e83476d87c979bb2d7675a6a342 run
libglapi-mesa pool/main/m/mesa/libglapi-mesa_22.3.6-1+deb12u2_arm64.deb bc604daab873ed7ddf82667903cd84070dde7b99d78a9f7d51a6ffac45575183 run
libgbm1 pool/main/m/mesa/libgbm1_22.3.6-1+deb12u2_arm64.deb f5c8fdddbf365259d74af270fb10f30d7fddb3fbe7b2ff62f0fdd556f8db0dc8 run
libdrm2 pool/main/libd/libdrm/libdrm2_2.4.114-1+b1_arm64.deb f5f15a46d02cf5d9fa52d4f1c54b8cf80c398711ad771a9938b12399b8d8090c run
libdrm-amdgpu1 pool/main/libd/libdrm/libdrm-amdgpu1_2.4.114-1+b1_arm64.deb 24692e933b98b2b8b80d87c45728efa2bc7176120ce9efcb8793407e508b5a32 run
libdrm-nouveau2 pool/main/libd/libdrm/libdrm-nouveau2_2.4.114-1+b1_arm64.deb 2acf0db609e8569dd89eea5f3e6f27968de50474d31213665901ad1c6d1133de run
libdrm-radeon1 pool/main/libd/libdrm/libdrm-radeon1_2.4.114-1+b1_arm64.deb e53c2b4be53b4a8e6413887525c9172bdc95ed3a140a52ce23d6e1a68789aaa0 run
libexpat1 pool/main/e/expat/libexpat1_2.5.0-1+deb12u2_arm64.deb 8dc1976735acf825e3a922d82f171e20feec7394da2b8c616091983e38706275 run
libwayland-server0 pool/main/w/wayland/libwayland-server0_1.21.0-1_arm64.deb 4d99e7e0503fdaa999e67d47d8612c0465e775eafa47819573da50bdb09064f2 run
libwayland-client0 pool/main/w/wayland/libwayland-client0_1.21.0-1_arm64.deb ae390cc04c2eb1de90b9a6373505b22c730ada5e72daa50c507b7f99c12faf06 run
libffi8 pool/main/libf/libffi/libffi8_3.4.4-1_arm64.deb 80b5c36177dc0e29d531c7eddbed3cc7355cb490e49f8cfa5959572d161f27b3 run
libx11-xcb1 pool/main/libx/libx11/libx11-xcb1_1.8.4-2+deb12u2_arm64.deb 21c7e6f19909bb1e02bb3c2089789791858c54d37e86cef9d7872782c1cb711e run
libx11-6 pool/main/libx/libx11/libx11-6_1.8.4-2+deb12u2_arm64.deb d1d533e983582282a9ea82c87ac5ce715a9b67bd6d1acbd2439a11c63c36549b run
libxcb1 pool/main/libx/libxcb/libxcb1_1.15-1_arm64.deb 041d9a68415c3ccf3ce8f4f8b88e4bbfb5dc1f0d97013c6ef8423e620ea50f84 run
libxau6 pool/main/libx/libxau/libxau6_1.0.9-1_arm64.deb 36c2bf400641a80521093771dc2562c903df4065f9eb03add50d90564337ea6c run
libxdmcp6 pool/main/libx/libxdmcp/libxdmcp6_1.1.2-3_arm64.deb e92569ac33247261aa09adfadc34ced3994ac301cf8b58de415a2d5dbf15ccfc run
libbsd0 pool/main/libb/libbsd/libbsd0_0.11.7-2_arm64.deb ef00f132ddd268ee67d756998723ee18543db27ab34930c0a9f1cff75cf55382 run
libmd0 pool/main/libm/libmd/libmd0_1.0.4-2_arm64.deb 99a8c7dd591fae9fb37d8bf8dfdffa850e207fa405b3198c5b24711a5f972381 run
libxcb-dri2-0 pool/main/libx/libxcb/libxcb-dri2-0_1.15-1_arm64.deb ec012de2f5945a5662b32ab1eb59be944e4b5b5ddae89ae28765819df62817f6 run
libxcb-dri3-0 pool/main/libx/libxcb/libxcb-dri3-0_1.15-1_arm64.deb a4b2a700d0da1dcaddc6435986d2ae45e8efdb58a2892a2e87cebb0c85dca359 run
libxcb-present0 pool/main/libx/libxcb/libxcb-present0_1.15-1_arm64.deb df2148b69f25b6a31af73bcfb355b2ae3724678167457996febb3d335377ca17 run
libxcb-randr0 pool/main/libx/libxcb/libxcb-randr0_1.15-1_arm64.deb 9e09fe9b7f6ac596f8b14499e9a10c336b7d3ff27e78f751dffeac9975113004 run
libxcb-sync1 pool/main/libx/libxcb/libxcb-sync1_1.15-1_arm64.deb 63f165fb851ffacf021cdc4224fb93f5694a5c03dc699f2f4237aa67e1550d25 run
libxcb-xfixes0 pool/main/libx/libxcb/libxcb-xfixes0_1.15-1_arm64.deb e6cfc1f2a68d4c7f0c10e8bf24ebab20d791ba96ec1ca70824ccd927f0fa9e21 run
libxshmfence1 pool/main/libx/libxshmfence/libxshmfence1_1.3-1_arm64.deb fa2edaae6902681ecd02534dcdf8389ac48714d3385d572cc17747160957acc8 run
libelf1 pool/main/e/elfutils/libelf1_0.188-2.1_arm64.deb 32971952d8f5d563447bf275f24e26057500924da2d855c1edb53b0f0400bd11 run
zlib1g pool/main/z/zlib/zlib1g_1.2.13.dfsg-1_arm64.deb 52b8b8a145bbe1956bba82034f77022cbef0c3d0885c9e32d9817a7932fe1913 run
libllvm15 pool/main/l/llvm-toolchain-15/libllvm15_15.0.6-4+b1_arm64.deb 9eecca617a53fb80ce0de5c6f475817528b3284cd43eec3863126715dc850dd2 run
libedit2 pool/main/libe/libedit/libedit2_3.1-20221030-2_arm64.deb 6ca1e355586ebfcfe8122b4d317fcaad4a9a10ca455c6a9bbd36f55bf4be85d3 run
libtinfo6 pool/main/n/ncurses/libtinfo6_6.4-4_arm64.deb baef0f6776f84c7eed4f1146d6e5774689567dad43216894d41da02e6608e4b3 run
libxml2 pool/main/libx/libxml2/libxml2_2.9.14+dfsg-1.3~deb12u6_arm64.deb 0bc00975e459387535a4bbabe98a398f0e77c9ab82474face18d01ae8ed0dae4 run
libicu72 pool/main/i/icu/libicu72_72.1-3+deb12u1_arm64.deb 4f5d892fd81110435e45ed0a5f1b12899d7ff989d51db283cbc043f5631646d8 run
liblzma5 pool/main/x/xz-utils/liblzma5_5.4.1-1+deb12u1_arm64.deb 647ce9d81e41c30071ce6ac0d3324579216a33aaa1d28971d6952315f6a95a75 run
libz3-4 pool/main/z/z3/libz3-4_4.8.12-3.1_arm64.deb 115080b0aee2d75f316da4be24c8a5e88ed7c362c7b4a4cd13f3af9b9da130f2 run
libsensors5 pool/main/l/lm-sensors/libsensors5_3.6.0-7.1_arm64.deb adfdf7c7ee1a1c7bc61b5bf691d3a501e857c44243c7cdb6c8b81de86592e03b run
libzstd1 pool/main/libz/libzstd/libzstd1_1.5.4+dfsg2-5_arm64.deb 95e173c9538f96ede4fc275ec7863f395a97dd0ea62454be9bc914efa1b9be93 run
libstdc++6 pool/main/g/gcc-12/libstdc++6_12.2.0-14+deb12u1_arm64.deb 26e138c677a985775331373828a6c286c551ff397cb735d00e2383cb273d1cb2 run
libgcc-s1 pool/main/g/gcc-12/libgcc-s1_12.2.0-14+deb12u1_arm64.deb 576926b283613db80168ddf76380a3bd877602778cf0d226caa7bfbfa71eacf3 run
libc6-dev pool/main/g/glibc/libc6-dev_2.36-9+deb12u14_arm64.deb 8bc395bf2ee9939f1a6ae395f974cffdcf541d98f99adfe89ce7e4bef36d159e dev
libc6 pool/main/g/glibc/libc6_2.36-9+deb12u14_arm64.deb 01f4330719fd4f65580e16ea5a0527f372fca750e8f588d26deaf09f2d3b1cf4 dev
linux-libc-dev pool/main/l/linux/linux-libc-dev_6.1.176-1_arm64.deb eeca5ec13bed7206f8171fab9a48e84119dcc20cc79557f104628447a524ad10 dev
libegl-dev pool/main/libg/libglvnd/libegl-dev_1.6.0-1_arm64.deb 8a3004f892785a5fa4c9f969c116f567a36d1c8e9b3d5fd0403055151e36c7d3 dev
libgles-dev pool/main/libg/libglvnd/libgles-dev_1.6.0-1_arm64.deb 941e9963d64479544691f7183fe0eeae32b53b633df71c4c51263517ab2ea8f3 dev
libgl-dev pool/main/libg/libglvnd/libgl-dev_1.6.0-1_arm64.deb 56add9e710c072cf56504beed50a639f9f7c64f4e31e66958fa65a566084888c dev
libegl1 pool/main/libg/libglvnd/libegl1_1.6.0-1_arm64.deb 707097a275155c600e2e9251c4ee7cdfdb2d8f50a678a850a2526a7bc9664166 dev
libgles2 pool/main/libg/libglvnd/libgles2_1.6.0-1_arm64.deb efeb2717380f8411d66b3f669bb99bbd83ff09db3d4a1661533aa31af659608c dev
libglvnd0 pool/main/libg/libglvnd/libglvnd0_1.6.0-1_arm64.deb d5063205fab5322ee686f33f87c6315b3fcd1335ea4e7bc8202b48ed4f9c9e83 dev
EOF

# Mesa in the guest: its libraries and DRI drivers at Debian's paths, its glvnd vendor file.
multiarch=usr/lib/aarch64-linux-gnu
mkdir -p "$work/root/$multiarch" "$work/root/usr/share/glvnd/egl_vendor.d"
for dir in "$work/mesa/$multiarch" "$work/mesa/lib/aarch64-linux-gnu"; do
    [ -d "$dir" ] && cp -R "$dir/." "$work/root/$multiarch/"
done
cp "$work/mesa/usr/share/glvnd/egl_vendor.d/50_mesa.json" "$work/root/usr/share/glvnd/egl_vendor.d/"

# The suites, cross-compiled against the glvnd libraries.
sysroot="$work/sysroot"
lib="$sysroot/lib/aarch64-linux-gnu"
usr_lib="$sysroot/$multiarch"
rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
for suite in $suites; do
    "${CLANG:-clang}" --target=aarch64-linux-gnu --sysroot="$sysroot" -fPIE -O1 -DEGL_NO_X11 \
        -isystem "$sysroot/usr/include/aarch64-linux-gnu" -isystem "$sysroot/usr/include" \
        -w -c "tests/gpu/$suite.c" -o "$work/$suite.o"
    DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
    LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        "$lld" -flavor gnu -m aarch64elf -pie --dynamic-linker /lib/ld-linux-aarch64.so.1 \
        "$usr_lib/Scrt1.o" "$usr_lib/crti.o" "$work/$suite.o" "$usr_lib/crtn.o" \
        -L "$lib" -l:libc.so.6 -l:libm.so.6 "$usr_lib/libEGL.so.1" "$usr_lib/libGLESv2.so.2" \
        --allow-shlib-undefined -o "$work/root/$suite"
done
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null > "$work/gl.cpio")
gzip -n "$work/gl.cpio"
cat "$fixture/egl-initramfs.cpio.gz" "$work/gl.cpio.gz" > "$work/initramfs.cpio.gz"

# Each suite: the GPU through NVIDIA's EGL, then llvmpipe through Mesa's, then the comparison.
sed -e '$d' -e 's/host1x mc-utils/host1x host1x-fence host1x-nvhost mc-utils/' \
    -e 's/nvmap nvgpu/nvmap nvgpu cec drm drm_kms_helper nvhwpm tegra_wmark governor_userspace tegra-drm/' \
    scripts/jetson-orin-nano-gpu-power.txt > "$work/gl.txt"
vendors=/usr/share/glvnd/egl_vendor.d
for suite in $suites; do
    printf 'serial cd /tmp; mkdir -p gpu-%s sw-%s; LD_LIBRARY_PATH=/lib:/%s __EGL_VENDOR_LIBRARY_FILENAMES=%s/10_nvidia.json /%s run gpu-%s '"${SBC_GL_SCENE:-}"' 2>&1; echo "GL_GPU_" "%s=$?"\\r\n' \
        "$suite" "$suite" "$multiarch" "$vendors" "$suite" "$suite" "$suite" >> "$work/gl.txt"
    printf 'wait-serial GL_GPU_ %s=\n' "$suite" >> "$work/gl.txt"
    printf 'serial LD_LIBRARY_PATH=/lib:/%s __EGL_VENDOR_LIBRARY_FILENAMES=%s/50_mesa.json LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe /%s run sw-%s '"${SBC_GL_SCENE:-}"' > sw.log 2>&1 || tail -3 sw.log; /%s cmp gpu-%s sw-%s; echo "GL_CMP_" "%s=$?"\\r\n' \
        "$multiarch" "$vendors" "$suite" "$suite" "$suite" "$suite" "$suite" "$suite" >> "$work/gl.txt"
    printf 'wait-serial GL_CMP_ %s=\n' "$suite" >> "$work/gl.txt"
    for scene in ${SBC_GL_DUMP:-}; do
        printf 'serial f=gpu-%s/%s.ppm; [ -f $f ] && { echo "PPM_BEGIN %s-%s"; base64 $f; echo "PPM_""END"; }; echo "PPM_" "DONE"\\r\n' \
            "$suite" "$scene" "$suite" "$scene" >> "$work/gl.txt"
        printf 'wait-serial PPM_ DONE\n' >> "$work/gl.txt"
    done
done
printf 'quit\n' >> "$work/gl.txt"

"$emulator" run --board jetson-orin-nano-4gb --gpu on --kernel "$fixture/Image" \
    --initrd "$work/initramfs.cpio.gz" --script "$work/gl.txt" --timeout "${SBC_GL_TIMEOUT:-1800}" \
    > "$work/boot.log" 2>&1 || {
        cp "$work/boot.log" "$fixture/gl-boot.log"
        tail -60 "$work/boot.log"
        exit 1
    }
cp "$work/boot.log" "$fixture/gl-boot.log"
tr -d '\r' < "$work/boot.log" | grep -E ' (PASS|FAIL) |scenes differ|^GL_(GPU|CMP)_|\[maxwell\]' | tee "${SBC_GL_LOG:-/dev/null}"
if [ -n "${SBC_GL_DUMP:-}" ]; then
    out=${SBC_GL_OUT:-$fixture/gl-out}
    mkdir -p "$out"
    tr -d '\r' < "$work/boot.log" | python3 -c '
import base64, sys
out, name, lines = sys.argv[1], None, []
for line in sys.stdin:
    line = line.strip()
    if line.startswith("PPM_BEGIN "):
        name, lines = line.split()[1], []
    elif line == "PPM_END" and name:
        open(f"{out}/{name}.ppm", "wb").write(base64.b64decode("".join(lines)))
        name = None
    elif name:
        lines.append(line)
' "$out"
fi
! tr -d '\r' < "$work/boot.log" | grep -qE '^GL_CMP_ .*=[^0]'
