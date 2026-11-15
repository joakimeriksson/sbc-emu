#!/usr/bin/env python3
"""Write the Advanced SIMD decode oracle for arm-a32/tests/neon_decode_sweep.rs.

Disassembles every combination of the A32 Advanced SIMD data-processing opcode bits (U,
bits 23:16 and 11:4; 2^17 words) and of the element/structure load/store fields (A, L,
bits 11:4, three first registers) with arm-none-eabi-objdump, and marks the words it
calls UNDEFINED, illegal or UNPREDICTABLE, or decodes as an ARMv8+ instruction.

Usage: python3 tools/neon_decode_sweep.py OUT.txt
       NEON_ORACLE=OUT.txt cargo test --release -p arm-a32 --test neon_decode_sweep -- --ignored --nocapture
"""
import os
import re
import struct
import subprocess
import sys
import tempfile

ARMV8 = re.compile(r"\b(vrint|vcvt[anpm]\.|vmaxnm|vminnm|vsel|aes|sha|vqrdml|vsdot|vudot|vfmal|vfmsl|vcadd|"
                   r"vcmla|vjcvt|vmmla|vusdot|vsudot|vusmmla|vsmmla|vummla|vdot|vbf|vfmab|vfmat)")


def words():
    out = []
    for u in range(2):
        for hi in range(256):
            for lo in range(256):
                out.append(0xf2000000 | u << 24 | hi << 16 | lo << 4 | 0x2)
    for a in range(2):
        for l in range(2):
            for lo in range(256):
                for d in (0, 1, 0x1e):
                    out.append(0xf4000000 | a << 23 | l << 21 | (d >> 4) << 22 | (d & 15) << 12 | lo << 4 | 0xf)
    return out


def main():
    all_words = words()
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "all.bin")
        with open(path, "wb") as f:
            f.write(b"".join(struct.pack("<I", w) for w in all_words))
        listing = subprocess.run(["arm-none-eabi-objdump", "-D", "-b", "binary", "-marm", path],
                                 capture_output=True, text=True, check=True).stdout
    verdict = {}
    for line in listing.splitlines():
        m = re.match(r"\s*[0-9a-f]+:\s+([0-9a-f]{8})\s+(.*)", line)
        if not m:
            continue
        text = m.group(2).strip()
        undefined = ("UNDEFINED" in text or "illegal" in text or "UNPREDICTABLE" in text
                     or ARMV8.search(text) is not None or "p64" in text
                     or (re.search(r"\.f16\b", text) is not None
                         and not text.startswith(("vcvt.f16.f32", "vcvt.f32.f16"))))
        verdict[int(m.group(1), 16)] = (undefined, text)
    with open(sys.argv[1], "w") as f:
        for w in all_words:
            undefined, text = verdict[w]
            f.write("%08x %d %s\n" % (w, int(undefined), text))


if __name__ == "__main__":
    main()
