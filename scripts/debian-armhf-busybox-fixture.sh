#!/bin/sh
# Thumb-2 userland fixture: Debian 13 (trixie) armhf busybox-static, built for ARMv7
# Thumb-2 with hard float, as a BusyBox initramfs. Boots with the Orange Pi One kernel
# and DTB from scripts/orangepi-one-fixture.sh.
# Usage: scripts/debian-armhf-busybox-fixture.sh [OUT]   (default cache/debian-armhf)
set -eu
OUT=${1:-cache/debian-armhf}
DEB=busybox-static_1.37.0-6+b9_armhf.deb
URL=${DEBIAN_MIRROR:-https://deb.debian.org/debian}/pool/main/b/busybox/$DEB
SHA=8b35210e08d559ecc0ddb660dd96f6f9694f69eb27f1dc608f5eeda56d94509e

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)   # absolute: the script changes directory below
[ -f "$OUT/$DEB" ] || curl -fL --retry 3 -o "$OUT/$DEB" "$URL"
actual=$(shasum -a 256 "$OUT/$DEB" | cut -d' ' -f1)
[ "$actual" = "$SHA" ] || { echo "$DEB: sha256 $actual, expected $SHA" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
(cd "$work" && ar x "$OUT/$DEB" && tar -xf data.tar.*)
root="$work/root"
mkdir -p "$root/bin" "$root/dev" "$root/proc" "$root/sys" "$root/tmp"
cp "$work/usr/bin/busybox" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"
cat > "$root/init" <<'INIT'
#!/bin/busybox sh
/bin/busybox mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t tmpfs tmpfs /tmp
/bin/busybox --install -s /bin
export PATH=/bin PS1='thumb# '
echo "THUMB_USERLAND_OK"
exec /bin/busybox setsid -c /bin/sh
INIT
chmod 755 "$root/init"
(cd "$root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 2>/dev/null) | gzip -9n > "$OUT/initramfs.cpio.gz"
shasum -a 256 "$OUT/initramfs.cpio.gz"
