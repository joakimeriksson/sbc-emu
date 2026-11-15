#!/usr/bin/env python3
"""Generate arm-a32/tests/thumb_diff_cases.rs: each instruction assembled as A32 and as
T32 by arm-none-eabi-as, for the Thumb-vs-ARM differential test (thumb_diff.rs).

Usage: python3 tools/gen_thumb_diff.py > arm-a32/tests/thumb_diff_cases.rs
Registers: r0-r7 low operands, r8-r12 high operands, r9 is set to a memory buffer by
the test (memory forms use [r9, ...]); r13/r14/r15 are not used as operands.
"""
import os
import subprocess
import sys
import tempfile

CASES = """
adds r0, r1, r2 | adds r0, r1, #7 | adds r3, #200 | add r8, r10, r11 | add.w r0, r1, #0x1fe00
subs r0, r1, r2 | subs r0, r1, #5 | subs r3, #200 | sub.w r8, r10, #0xff00ff00
adcs r0, r1 | adcs.w r8, r10, r12, lsl #3 | sbcs r0, r1 | sbcs.w r8, r10, #0x8f000000
rsbs r0, r1, #0 | rsb.w r8, r10, r11, asr #7 | rsb r8, r10, #0x00ab00ab
ands r0, r1 | and.w r8, r10, #0xff00ff00 | ands.w r8, r10, r11, ror #9 | bics r0, r1 | bic.w r8, r10, #0x3f0
orrs r0, r1 | orr.w r8, r10, r11, lsr #1 | eors r0, r1 | eors.w r8, r10, #0xabababab
mvns r0, r1 | mvn.w r8, r10, lsl #4 | movs r0, #200 | mov.w r8, #0x00ff0000 | mov r8, r10 | movs r0, r1
mvn r8, #0xff000000 | movs.w r8, #0x80000001
lsls r0, r1, #5 | lsrs r0, r1, #32 | asrs r0, r1, #31 | lsls r0, r1 | lsrs r0, r1 | asrs r0, r1 | rors r0, r1
lsl.w r8, r10, r11 | asrs.w r8, r10, r11 | ror.w r8, r10, #13 | rrxs r8, r10
tst r0, r1 | tst.w r8, #0x80000000 | teq.w r8, r10, lsl #2 | cmp r0, r1 | cmp r0, #77 | cmp r8, r10
cmn r0, r1 | cmn.w r8, #0xff | cmp.w r8, r10, asr #2
muls r0, r1, r0 | mul r8, r10, r11 | mla r8, r10, r11, r12 | mls r8, r10, r11, r12
smull r8, r10, r11, r12 | umull r8, r10, r11, r12 | smlal r8, r10, r11, r12 | umlal r8, r10, r11, r12
umaal r8, r10, r11, r12 | smlabt r8, r10, r11, r12 | smultb r8, r10, r11 | smlawt r8, r10, r11, r12
smulwb r8, r10, r11 | smlad r8, r10, r11, r12 | smuadx r8, r10, r11 | smlsd r8, r10, r11, r12
smmul r8, r10, r11 | smmlar r8, r10, r11, r12 | smmls r8, r10, r11, r12 | smlalbb r8, r10, r11, r12
smlald r8, r10, r11, r12 | smlsldx r8, r10, r11, r12 | usad8 r8, r10, r11 | usada8 r8, r10, r11, r12
sdiv r8, r10, r11 | udiv r8, r10, r11
qadd r8, r10, r11 | qsub r8, r10, r11 | qdadd r8, r10, r11 | qdsub r8, r10, r11
sadd16 r8, r10, r11 | qasx r8, r10, r11 | shsax r8, r10, r11 | ssub16 r8, r10, r11 | sadd8 r8, r10, r11
ssub8 r8, r10, r11 | uadd16 r8, r10, r11 | uqsub8 r8, r10, r11 | uhadd8 r8, r10, r11 | usax r8, r10, r11
sel r8, r10, r11
sxtb r0, r1 | uxth r0, r1 | sxth.w r8, r10, ror #16 | uxtb.w r8, r10, ror #8 | sxtab r8, r10, r11
uxtah r8, r10, r11, ror #24 | sxtb16 r8, r10 | uxtab16 r8, r10, r11, ror #8
rev r0, r1 | rev16 r0, r1 | revsh r0, r1 | rev.w r8, r10 | rbit r8, r10 | clz r8, r10
ssat r8, #9, r10, asr #3 | usat r8, #9, r10, lsl #2 | ssat16 r8, #7, r10 | usat16 r8, #5, r10
sbfx r8, r10, #3, #9 | ubfx r8, r10, #7, #20 | bfi r8, r10, #4, #12 | bfc r8, #9, #3
pkhbt r8, r10, r11, lsl #8 | pkhtb r8, r10, r11, asr #12
movw r8, #0xbeef | movt r8, #0xdead
ldr r0, [r9, #8] | ldrb r0, [r9, #3] | ldrh r0, [r9, #6] | ldrsb r0, [r9, r1] | ldrsh r0, [r9, r2]
ldr r0, [r9, r1] | ldr.w r8, [r9, #0x404] | ldrb.w r8, [r9, #-12] | ldrsh.w r8, [r9, #-6]
ldr r8, [r9, #8]! | ldr r8, [r9], #-8 | ldrh r8, [r9, r1, lsl #1] | ldrsb.w r8, [r9, #0x7ff]
str r0, [r9, #8] | strb r0, [r9, #3] | strh r0, [r9, #6] | str.w r8, [r9, #0x404] | strh r8, [r9, #-6]!
strb r8, [r9], #5 | str r8, [r9, r1, lsl #2]
ldrd r0, r1, [r9, #8] | strd r0, r1, [r9, #-16] | ldrd r0, r1, [r9], #8 | strd r0, r1, [r9, #8]!
ldm r9!, {r0, r1, r2} | stm r9!, {r0, r1, r2} | ldmdb r9, {r0, r8, r10} | stmdb r9!, {r1, r2, r10}
""".replace("\n", "|")

# Thumb-only encodings without an A32 counterpart are compared against an A32 sequence
# with the same effect, written here as (thumb, arm).
EQUIVALENTS = [
    ("orn r8, r10, r11", "mvn r12, r11\n orr r8, r10, r12"),
    ("orns r8, r10, #0xff", "mvn r12, #0xff\n orrs r8, r10, r12"),
    ("ldrd r1, r4, [r9, #8]", "ldr r1, [r9, #8]\n ldr r4, [r9, #12]"),
    ("addw r8, r10, #0xfff", "movw r12, #0xfff\n add r8, r10, r12"),
    ("subw r8, r10, #0xabc", "movw r12, #0xabc\n sub r8, r10, r12"),
    ("add.w r0, r1, #0x1fe00", "movw r12, #0xfe00\n movt r12, #1\n add r0, r1, r12"),
    ("sub.w r8, r10, #0xff00ff00", "movw r12, #0xff00\n movt r12, #0xff00\n sub r8, r10, r12"),
    ("rsb.w r8, r10, #0x00ab00ab", "movw r12, #0xab\n movt r12, #0xab\n rsb r8, r10, r12"),
    ("and.w r8, r10, #0xff00ff00", "movw r12, #0xff00\n movt r12, #0xff00\n and r8, r10, r12"),
    ("eors.w r8, r10, #0xabababab", "movw r12, #0xabab\n movt r12, #0xabab\n eors r8, r10, r12"),
    ("ldrh.w r8, [r9, r1, lsl #1]", "lsl r12, r1, #1\n ldrh r8, [r9, r12]"),
    ("ldrsb.w r8, [r9, #0x7ff]", "movw r12, #0x7ff\n ldrsb r8, [r9, r12]"),
]


def assemble(lines, thumb):
    with tempfile.TemporaryDirectory() as tmp:
        src = os.path.join(tmp, "t.s")
        obj = os.path.join(tmp, "t.o")
        binf = os.path.join(tmp, "t.bin")
        with open(src, "w") as f:
            f.write(".syntax unified\n.fpu vfpv4\n%s\n" % (".thumb" if thumb else ".arm"))
            f.write(lines + "\n")
        subprocess.run(["arm-none-eabi-as", "-march=armv7ve", "-mfpu=vfpv4", "-o", obj, src],
                       check=True, stderr=subprocess.DEVNULL)
        subprocess.run(["arm-none-eabi-objcopy", "-O", "binary", obj, binf], check=True)
        data = open(binf, "rb").read()
    if thumb:
        words, i = [], 0
        while i < len(data):
            hw = int.from_bytes(data[i:i + 2], "little")
            if hw >> 11 >= 0b11101:
                hw2 = int.from_bytes(data[i + 2:i + 4], "little")
                words.append((hw << 16 | hw2, 4))
                i += 4
            else:
                words.append((hw, 2))
                i += 2
        return words
    return [(int.from_bytes(data[i:i + 4], "little"), 4) for i in range(0, len(data), 4)]


def main():
    out = sys.stdout
    out.write("// Generated by tools/gen_thumb_diff.py with arm-none-eabi-as; do not edit.\n")
    out.write("// (name, A32 words, T32 instructions as (encoding, size)).\n")
    out.write("pub const CASES: &[(&str, &[u32], &[(u32, u32)])] = &[\n")
    pairs = [(c.strip(), c.strip()) for c in CASES.split("|") if c.strip()]
    pairs += EQUIVALENTS
    for thumb_src, arm_src in pairs:
        try:
            arm = assemble(arm_src.replace(".w ", " "), False)
            thumb = assemble(thumb_src, True)
        except subprocess.CalledProcessError:
            sys.stderr.write("skipped (not encodable in both sets): %s\n" % thumb_src)
            continue
        arm_words = ", ".join("0x%08x" % w for w, _ in arm)
        thumb_words = ", ".join("(0x%x, %d)" % (w, n) for w, n in thumb)
        out.write('    ("%s", &[%s], &[%s]),\n' % (thumb_src, arm_words, thumb_words))
    out.write("];\n")


if __name__ == "__main__":
    main()
