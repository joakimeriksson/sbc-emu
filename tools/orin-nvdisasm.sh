#!/bin/sh
# Disassemble raw SM87 code with NVIDIA's nvdisasm, run inside the emulated Orin.
# Usage: tools/orin-nvdisasm.sh FILE.bin [OFFSET]   (OFFSET skips a shader header, e.g. 0x80)
# Needs the disassembler fixture in cache/jetson-r3644/disasm-initrd (see docs/jetson-orin-nano.md).
set -eu
cd "$(dirname "$0")/.."
fixture=cache/jetson-r3644
file=$1
offset=$(($(printf '%d' "${2:-0}")))
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/root"
# Keep whole instructions up to the closing self-branch (or the last non-zero one).
python3 - "$file" "$offset" "$work/root/shader.bin" <<'PY'
import sys
data = open(sys.argv[1], 'rb').read()[int(sys.argv[2]):]
end = len(data) - len(data) % 16
for at in range(0, end, 16):
    if data[at:at + 8] == bytes.fromhex('477900 00f0ffffff'.replace(' ', '')):
        end = at + 16
        break
while end > 0 and not any(data[end - 16:end]):
    end -= 16
open(sys.argv[3], 'wb').write(data[:end])
PY
(cd "$work/root" && echo shader.bin | cpio -o -H newc -R 0:0 2>/dev/null | gzip -n > "$work/shader.cpio.gz")
cat "$fixture/disasm-initramfs.cpio.gz" "$work/shader.cpio.gz" > "$work/initramfs.cpio.gz"
cat > "$work/script.txt" <<'S'
wait-serial TEGRA234_BOOT_OK
serial /lib/ld-linux-aarch64.so.1 --library-path /lib /nvdisasm -b SM87 -hex /shader.bin; echo "DISASM_" "END"\r
wait-serial DISASM_ END
quit
S
"${SBC_EMU:-target/release/sbc-emu}" run --board jetson-orin-nano-4gb --ram-mb 768 \
    --kernel "$fixture/Image" --initrd "$work/initramfs.cpio.gz" --script "$work/script.txt" \
    --timeout 300 2>&1 | tr -d '\r' | sed -n '/^ *\/\*[0-9a-f]\{4\}\*\//p'
