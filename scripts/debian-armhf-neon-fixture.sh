#!/bin/sh
# NEON userland fixture: Debian 13 (trixie) armhf glibc, OpenSSL and libjpeg-turbo with
# Debian's busybox-static as the shell, as an initramfs. These pick their Advanced SIMD
# code at run time: glibc's memcpy/memchr ifuncs and OpenSSL's SHA-256/SHA-512/ChaCha20/
# Poly1305 from AT_HWCAP (HWCAP_NEON; OPENSSL_armcap=0 turns it off), libjpeg-turbo's
# DCT/colour conversion/upsampling from /proc/cpuinfo (JSIMD_FORCENONE=1 turns it off).
# Boots with the Orange Pi One kernel and DTB from scripts/orangepi-one-fixture.sh;
# scripts/orangepi-one-neon.txt checks the results.
# Usage: scripts/debian-armhf-neon-fixture.sh [OUT]   (default cache/debian-armhf-neon)
# Debian drops superseded versions from the pool at point releases: update the pins (and
# DEBIAN_MIRROR=https://snapshot.debian.org/archive/debian/<date> works for old ones).
set -eu
OUT=${1:-cache/debian-armhf-neon}
MIRROR=${DEBIAN_MIRROR:-https://deb.debian.org/debian}

# package file (under pool/main) and sha256
PACKAGES="
b/busybox/busybox-static_1.37.0-6+b9_armhf.deb 8b35210e08d559ecc0ddb660dd96f6f9694f69eb27f1dc608f5eeda56d94509e
g/glibc/libc6_2.41-12+deb13u4_armhf.deb 4fc6fed8d77d01c0dacb5fda8b880a8c8a669575936de5761b693077a6f6c2ec
g/gcc-14/libgcc-s1_14.2.0-19_armhf.deb 25910c3a0bce3985e388de445244578b7922dc670c9bba1c8673779b89447873
o/openssl/openssl_3.5.7-1~deb13u2_armhf.deb 94afc1c0bf72552b729dfe73eba3439e914ab8ec0c36b5564247dc286ed46bb1
o/openssl/libssl3t64_3.5.7-1~deb13u2_armhf.deb 94538529b1471c2055476df47fbc29a67a734c7c9d7f1fb44fd54c97e43c40c5
z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_armhf.deb 81c55a59e1570477ecef6a449bf6dce44dad67ba4ce9e04760451d4cfe200534
libz/libzstd/libzstd1_1.5.7+dfsg-1_armhf.deb da5238dd84fc51f782f39d435821bff556409b3dbc82d232e4e81f427fb1ca65
libj/libjpeg-turbo/libjpeg-turbo-progs_2.1.5-4_armhf.deb 454ff3f8dbd0647257a18a264c1d1046d53ed07e05802134c925f11f97c40176
libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_armhf.deb 77eb9383b6b824bce2fa4dcf9856963dcb99534d01021708196dfa5dd2f2ec86
libj/libjpeg-turbo/libturbojpeg0_2.1.5-4_armhf.deb cad148a5bc59598101b8f8bbfcd4887a6e4e9ba3eaa189905a400d1645c8e377
"

mkdir -p "$OUT/downloads"
OUT=$(cd "$OUT" && pwd)   # absolute: the script changes directory below
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root="$work/root"
mkdir -p "$root"

echo "$PACKAGES" | while read -r path sha; do
    [ -n "$path" ] || continue
    deb=$(basename "$path")
    if [ ! -f "$OUT/downloads/$deb" ]; then
        curl -fL --retry 3 -o "$OUT/downloads/$deb.part" "$MIRROR/pool/main/$path"
        mv "$OUT/downloads/$deb.part" "$OUT/downloads/$deb"
    fi
    actual=$(shasum -a 256 "$OUT/downloads/$deb" | cut -d' ' -f1)
    [ "$actual" = "$sha" ] || { echo "$deb: sha256 $actual, expected $sha" >&2; exit 1; }
    mkdir "$work/deb"
    (cd "$work/deb" && ar x "$OUT/downloads/$deb" && tar -xf data.tar.* -C "$root")
    rm -rf "$work/deb"
done

# Merged /usr as on Debian; only what the test needs (no gconv, locales or docs).
rm -rf "$root/usr/share" "$root/usr/lib/arm-linux-gnueabihf/gconv" "$root/etc/ssl/certs" "$root/usr/lib/ssl/misc"
mv "$root/usr/bin/busybox" "$root/busybox"
mkdir -p "$root/bin" "$root/dev" "$root/proc" "$root/sys" "$root/tmp"
mv "$root/busybox" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"
ln -s usr/lib "$root/lib"
cat > "$root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t tmpfs tmpfs /tmp
/bin/busybox --install -s /bin
export PATH=/usr/bin:/bin PS1='neon# '
echo "NEON_USERLAND_OK"
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$root/init"
(cd "$root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) | gzip -9n > "$OUT/initramfs.cpio.gz"
shasum -a 256 "$OUT/initramfs.cpio.gz"
