#!/bin/sh
# A Debian armhf root filesystem with Mesa (panfrost), kmscube, glmark2 and gcc for the
# Mali-T760 tests on the Tinker Board profile. The userland is the Orange Pi One GPU image's
# (scripts/armbian-gpu-fixture.sh; both boards are armhf, and the userland has no
# board-specific parts the tests need); /boot (kernel, device trees, initramfs,
# armbianEnv.txt) and /lib/modules come from the Tinker Board's Armbian image
# (scripts/armbian-tinkerboard-fixture.sh), whose kernel has Panfrost as a module. The card
# then boots like an Armbian card. The GLES test program and its runner are put in /root
# (there is no network on the RK3288 yet).
#   scripts/armbian-gpu-fixture.sh && scripts/armbian-tinkerboard-fixture.sh && scripts/mali-t760-fixture.sh
#   sbc-emu run --board tinkerboard --sd cache/mali-t760/debian-gpu.img --append init=/bin/bash \
#       --script scripts/mali-t760-gpu.txt --timeout 1800
#       # or --script scripts/mali-t760-gles2.txt --timeout 7200
# Needs e2fsprogs (Homebrew: e2fsprogs).
# Usage: scripts/mali-t760-fixture.sh [OUT]   (default cache/mali-t760)
set -eu
OUT=${1:-cache/mali-t760}
SRC=${MALI_GPU_IMAGE:-cache/mali/armbian-gpu.img}
PATH="$PATH:/opt/homebrew/opt/e2fsprogs/sbin:/usr/local/opt/e2fsprogs/sbin:/sbin:/usr/sbin"
TINKER=cache/tinker-board-armbian/Armbian_26.8.1_Tinkerboard_trixie_current_6.18.43_minimal.img
[ -f "$SRC" ] || { echo "$SRC missing: run scripts/armbian-gpu-fixture.sh" >&2; exit 1; }
[ -f "$TINKER" ] || { echo "$TINKER missing: run scripts/armbian-tinkerboard-fixture.sh" >&2; exit 1; }
mkdir -p "$OUT"
IMG=$OUT/debian-gpu.img
rm -f "$IMG"
cp -c "$SRC" "$IMG" 2>/dev/null || cp "$SRC" "$IMG"
# The root filesystem starts at 4 MiB. Replay its journal first: debugfs ignores the
# journal, and writes made around it are lost (or corrupt files) when the guest replays it.
# e2fsck cannot reopen an offset image for journal recovery, so work on a copy.
FS=$OUT/root.ext4
dd if="$IMG" of="$FS" bs=1048576 skip=4 2>/dev/null
e2fsck -fy "$FS" >/dev/null || [ $? -le 1 ]
for f in tests/gpu/gles2test.c scripts/mali-t760-gles2.sh; do
    debugfs -w -R "write $f /root/$(basename "$f")" "$FS" >/dev/null 2>&1
done
e2fsck -fn "$FS" >/dev/null
dd if="$FS" of="$IMG" bs=1048576 seek=4 conv=notrunc 2>/dev/null
rm "$FS"
# Build gles2test on the emulated Orange Pi One (same armhf userland): the binary lands in
# /root/g2.
BIN=${SBC_EMU:-target/release/sbc-emu}
cat > "$OUT/build.txt" <<'SCRIPT'
wait-serial root@
serial mount -o remount,rw /; mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; mount -t tmpfs tmp /tmp; mkdir -p /root/g2 && cd /root/g2 && gcc -O1 -o gles2test /root/gles2test.c -lEGL -lGLESv2 -lm && sync && echo BUILT-$((6*7))\n
wait-serial BUILT-42
quit
SCRIPT
"$BIN" run --board orangepi-one --sd "$IMG" --sd-persist --append init=/bin/bash --script "$OUT/build.txt" --timeout 1800 >/dev/null
# Then the Tinker Board's kernel: boot its Armbian card with this image as the eMMC and copy
# /boot and /lib/modules over, pointing armbianEnv.txt's rootdev at this filesystem. The
# Armbian card is a throwaway clone: --sd-persist, which the eMMC needs, applies to it too.
SIG=$(od -An -tx4 -j440 -N4 "$IMG" | tr -d ' ')
CARD=$OUT/tinker-card.img
rm -f "$CARD"
cp -c "$TINKER" "$CARD" 2>/dev/null || cp "$TINKER" "$CARD"
cat > "$OUT/kernel.txt" <<SCRIPT
wait-serial root@
serial mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev; D=\$(findfs PARTUUID=$SIG-01) && mount \$D /mnt && rm -f /mnt/boot/zImage /mnt/boot/uInitrd /mnt/boot/dtb && cp -a /boot/. /mnt/boot/ && cp -a /lib/modules/. /mnt/lib/modules/ && sed -i "s/^rootdev=.*/rootdev=UUID=\$(blkid -s UUID -o value \$D)/; s/^overlays=.*/overlays=/" /mnt/boot/armbianEnv.txt && umount /mnt && echo COPIED-\$((6*7))\\n
wait-serial COPIED-42
quit
SCRIPT
"$BIN" run --board tinkerboard --sd "$CARD" --emmc "$IMG" --sd-persist --append init=/bin/bash --script "$OUT/kernel.txt" --timeout 1800 >/dev/null
rm -f "$CARD"
echo "$IMG"
