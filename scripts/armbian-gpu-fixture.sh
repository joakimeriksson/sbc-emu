#!/bin/sh
# A writable copy of the Armbian image with Mesa (the lima driver), kmscube and a compiler,
# for the Mali-400 GPU: the root filesystem is grown to 4 GiB and the packages are
# installed by the emulated board itself (Debian's armhf archive through the NAT, about
# ten minutes with the JIT). Needs e2fsprogs (e2fsck, resize2fs; Homebrew: e2fsprogs).
#   scripts/armbian-orangepi-one-fixture.sh && scripts/armbian-gpu-fixture.sh
#   sbc-emu run --board orangepi-one --sd cache/mali/armbian-gpu.img --append init=/bin/bash \
#       --script scripts/orangepi-one-gpu.txt --timeout 900
# Usage: scripts/armbian-gpu-fixture.sh [OUT]   (default cache/mali)
set -eu
OUT=${1:-cache/mali}
SRC=cache/armbian/Armbian_26.8.1_Orangepione_trixie_current_6.18.44_minimal.img
BIN=${SBC_EMU:-target/release/sbc-emu}
PATH="$PATH:/opt/homebrew/opt/e2fsprogs/sbin:/usr/local/opt/e2fsprogs/sbin:/sbin:/usr/sbin"
[ -f "$SRC" ] || { echo "$SRC missing: run scripts/armbian-orangepi-one-fixture.sh" >&2; exit 1; }
mkdir -p "$OUT"
IMG=$OUT/armbian-gpu.img
if [ ! -f "$IMG" ]; then
    # the root partition starts at sector 8192 (4 MiB)
    dd if="$SRC" of="$OUT/root.ext4" bs=1048576 skip=4 2>/dev/null
    truncate -s 4G "$OUT/root.ext4"
    e2fsck -fy "$OUT/root.ext4" >/dev/null || true
    resize2fs "$OUT/root.ext4" >/dev/null
    dd if="$SRC" of="$IMG" bs=1048576 count=4 2>/dev/null
    cat "$OUT/root.ext4" >> "$IMG"
    rm "$OUT/root.ext4"
    # partition 1 now covers 4 GiB
    printf '\000\000\200\000' | dd of="$IMG" bs=1 seek=458 conv=notrunc 2>/dev/null
fi
# the guest's clock starts in 1970: set it so apt accepts the archive signatures
NOW=$(( $(date +%s) + 600 ))
cat > "$OUT/install.txt" <<SCRIPT
wait-serial root@
serial mount -o remount,rw /; mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp; mount -t tmpfs run /run; ip link set end0 up; ip addr add 10.0.2.15/24 dev end0; ip route add default via 10.0.2.2; [ -e /etc/resolv.conf.orig ] || mv /etc/resolv.conf /etc/resolv.conf.orig; echo nameserver 10.0.2.3 > /etc/resolv.conf; date -s @$NOW; echo NET-\$((6*7))\\n
wait-serial NET-42
serial apt-get update; DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends kmscube mesa-utils-bin libgl1-mesa-dri libegl-mesa0 libgles2 gcc libc6-dev libgles-dev libegl-dev libgbm-dev libdrm-dev pkg-config; echo APT-\$((6*7))-\$?\\n
wait-serial APT-42-
serial mv /etc/resolv.conf.orig /etc/resolv.conf; sync; echo SYNC-\$((6*7))\\n
wait-serial SYNC-42
quit
SCRIPT
"$BIN" run --board orangepi-one --sd "$IMG" --sd-persist --append init=/bin/bash --script "$OUT/install.txt" --timeout 7200
echo "$IMG"
