#!/bin/sh
# Boot the Orin GPU fixture and exercise NVIDIA's userspace control ABI.
set -eu
cd "$(dirname "$0")/../.."
fixture=${1:-cache/jetson-r3644}
emulator=${SBC_EMU:-target/release/sbc-emu}
test -s "$fixture/Image"
test -s "$fixture/probe-initramfs.cpio.gz"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
rustc_path=$(rustup which rustc)
rustlib=$("$rustc_path" --print sysroot)/lib
lld="$rustlib/rustlib/$("$rustc_path" -vV | sed -n 's/^host: //p')/bin/rust-lld"
"${CLANG:-clang}" --target=aarch64-linux-gnu -ffreestanding -fno-stack-protector \
    -mgeneral-regs-only -O2 -Wall -Wextra -Werror -c tests/gpu/orin/ioctl.c -o "$work/ioctl.o"
DYLD_LIBRARY_PATH="$rustlib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
LD_LIBRARY_PATH="$rustlib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$lld" -flavor gnu -m aarch64elf -e _start "$work/ioctl.o" -o "$work/gpu-ioctl"
(cd "$work" && printf '%s\n' gpu-ioctl | cpio -o -H newc >ioctl.cpio)
gzip -n "$work/ioctl.cpio"
cat "$fixture/probe-initramfs.cpio.gz" "$work/ioctl.cpio.gz" >"$work/initramfs.cpio.gz"

sed '$d' scripts/jetson-orin-nano-gpu-power.txt >"$work/ioctl.txt"
cat >>"$work/ioctl.txt" <<'SCRIPT'
serial /gpu-ioctl; echo "NVGPU_IOCTL_" "STATUS=$?"\r
wait-serial NVGPU_IOCTL_ STATUS=
quit
SCRIPT

"$emulator" run --board jetson-orin-nano-4gb --gpu on \
    --kernel "$fixture/Image" --initrd "$work/initramfs.cpio.gz" \
    --script "$work/ioctl.txt" --timeout 60 >"$work/boot.log" 2>&1 || {
        cat "$work/boot.log"
        exit 1
    }
if ! rg '^NVGPU_IOCTL_ STATUS=0[[:space:]]*$' "$work/boot.log" ||
   ! rg 'GPU: 2 completed GPFIFO entries' "$work/boot.log"; then
    tail -40 "$work/boot.log"
    exit 1
fi
