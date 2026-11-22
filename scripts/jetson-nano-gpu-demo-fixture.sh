#!/bin/sh
# Build the Jetson Nano's GPU demos: an initramfs to load after the one of jetson-nano-debian-fixture.sh
# (the kernel takes several, one behind the other) with Mesa and what it needs, from Debian 13's arm64
# packages (pinned in jetson-nano-gpu-demo.packages), and the demos (demos/nano-gl).
#   OUT/gpu-demo.cpio.xz
# Mesa's library wants LLVM, for drivers that are not the Nano's: nouveau has its own compiler. LLVM is
# large, so a library of functions that do nothing takes its place (tools/stub_library.py).
# Usage: scripts/jetson-nano-gpu-demo-fixture.sh [OUT]   (default cache/jetson-nano/gpu-demo)
set -eu
cd "$(dirname "$0")/.."
OUT=${1:-cache/jetson-nano/gpu-demo}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
DL=${DOWNLOADS:-$OUT/downloads}
mkdir -p "$DL"
DL=$(cd "$DL" && pwd)
sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
while read -r want path; do
    f=$(basename "$path")
    [ -f "$DL/$f" ] && [ "$(sha "$DL/$f")" = "$want" ] && continue
    for url in https://deb.debian.org/debian https://snapshot.debian.org/archive/debian/20261121T000000Z; do
        curl -fsSL --retry 3 -o "$DL/$f.part" "$url/$path" || continue
        [ "$(sha "$DL/$f.part")" = "$want" ] && mv "$DL/$f.part" "$DL/$f" && break
        echo "$url/$path: not the file with sha256 $want" >&2
    done
    [ -f "$DL/$f" ] && [ "$(sha "$DL/$f")" = "$want" ] || { echo "cannot get $path" >&2; exit 1; }
done < scripts/jetson-nano-gpu-demo.packages
work=$(mktemp -d)
trap 'rm -rf "${work:?}"' EXIT
mkdir "$work/all" "$work/lib" "$work/root"
while read -r want path; do
    f=$(basename "$path")
    mkdir "$work/$f"
    (cd "$work/$f" && ar x "$DL/$f" && tar -C "$work/all" -xf data.tar.*)
done < scripts/jetson-nano-gpu-demo.packages
A=$work/all/usr/lib/aarch64-linux-gnu
python3 tools/stub_library.py "$A"/libgallium-*.so LLVM_ libLLVM.so.19.1 "$work/lib/libLLVM.so.19.1"
# what the libraries need, from the ones the demos use
python3 tools/stub_library.py --closure "$A" "$work/lib" libEGL.so.1 libGLESv2.so.2 libgbm.so.1 libdrm.so.2 \
    libEGL_mesa.so.0 dri_gbm.so=gbm/dri_gbm.so > "$work/needed"
R=$work/root/opt/gpu
mkdir -p "$R/lib/gbm" "$R/share/glvnd/egl_vendor.d" "$R/licenses" "$work/root/usr/bin"
while read -r name file; do cp "$file" "$R/lib/$name"; done < "$work/needed"
cp "$work/lib/libLLVM.so.19.1" "$R/lib/"
for d in "$work"/all/usr/share/doc/*/copyright; do
    [ -f "$d" ] && cp "$d" "$R/licenses/$(basename "$(dirname "$d")")"
done
# (which Debian's refer to)
for l in Apache-2.0 GPL-1 GPL-2 GPL-3 LGPL-2 LGPL-2.1 LGPL-3; do cp "$work/all/usr/share/common-licenses/$l" "$R/licenses/"; done
# the demos have Rust's standard library in them
cp "$(dirname "$(dirname "$(rustup which rustc)")")/share/doc/rust/COPYRIGHT-library.html" "$R/licenses/rust-std.html"
cp LICENSE "$R/licenses/nano-gl"
cp "$work/all/usr/share/glvnd/egl_vendor.d/50_mesa.json" "$R/share/glvnd/egl_vendor.d/"
sh demos/nano-gl/build.sh "$A" "$R/nano-gl"
cat > "$work/root/usr/bin/gpu-demo" <<'SH'
#!/bin/sh
# gpu-demo [cubes|planet|swarm|fur|tunnel|shadow]: a demo, or all of them in turn
export __EGL_VENDOR_LIBRARY_DIRS=/opt/gpu/share/glvnd/egl_vendor.d GBM_BACKENDS_PATH=/opt/gpu/lib/gbm
export MESA_SHADER_CACHE_DISABLE=true
# (a small picture, which a browser draws soon enough; --mode 1280x720 for the monitor's own)
exec /opt/gpu/lib/ld-linux-aarch64.so.1 --library-path /opt/gpu/lib /opt/gpu/nano-gl --mode 640x480 "$@"
SH
chmod 755 "$work/root/usr/bin/gpu-demo" "$R/nano-gl" "$R/lib/ld-linux-aarch64.so.1"
(cd "$work/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) > "$work/gpu-demo.cpio"
xz -9e --check=crc32 -c "$work/gpu-demo.cpio" > "$OUT/gpu-demo.cpio.xz"
ls -l "$work/gpu-demo.cpio" "$OUT/gpu-demo.cpio.xz" | awk '{ print $5, $9 }'
shasum -a 256 "$OUT/gpu-demo.cpio.xz"
