// Differential test of the WebAssembly JIT backend: A64 snippets (assembled with the host clang)
// run in web/wasm/sbc-emu.wasm with the JIT off and on; x0..x30 and NZCV must match.
//   node tools/wasm-jit-selftest.mjs
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
const SNIPPETS = [
  'mov x0, #5\n add x3, x0, #7\n subs x4, x3, #12\n cset x5, eq\n adds w6, w0, w0\n',
  'ldr x0, [x1]\n ldr w3, [x1, #8]\n ldrb w4, [x1, #3]\n ldrsb x5, [x1, #7]\n ldrh w6, [x1, #2]\n ldrsw x7, [x1, #12]\n',
  'ldr x0, [x1, #16]!\n ldr x3, [x1], #8\n ldp x4, x5, [x2]\n ldp w6, w7, [x2, #8]\n ldpsw x8, x9, [x2, #16]\n mov x10, x1\n',
  'mov x3, #24\n ldr x0, [x1, x3]\n ldr x4, [x1, x3, lsl #3]\n mov w5, #2\n ldr x6, [x1, w5, uxtw #3]\n',
  'str x1, [x2]\n ldr x0, [x2]\n stp x1, x2, [x2, #16]\n ldp x3, x4, [x2, #16]\n strb w1, [x2, #40]\n ldrb w5, [x2, #40]\n',
  'ldr x0, [x1]\n add x0, x0, #1\n str x0, [x1]\n ldr x3, [x1]\n cmp x3, x0\n cset x4, eq\n',
  'mov x0, #0\n mov x3, #10\n1: ldr x4, [x1, x0, lsl #3]\n add x5, x5, x4\n add x0, x0, #1\n cmp x0, x3\n b.ne 1b\n',
  'mov x9, sp\n stp x1, x2, [sp, #-16]!\n ldp x3, x4, [sp], #16\n mov x10, sp\n str x2, [sp, #-8]\n ldur x5, [sp, #-8]\n ldr x6, [x2, #-16]!\n',
  'ldrsb w0, [x1, #7]\n ldrsh w3, [x1, #6]\n ldr w4, [x1, #4]\n ldr xzr, [x1]\n ldrsw x5, [x1, #-4]!\n ldur w6, [x1, #3]\n mov x7, x1\n',
  'add x1, x1, #0xff8\n ldr x0, [x1]\n ldr x3, [x1, #8]\n ldp w4, w5, [x1, #4]\n',
  'mov x3, #-8\n ldr x0, [x1, x3]\n mov w4, #-4\n ldr w5, [x1, w4, sxtw]\n ldrb w6, [x1, x3]\n',
  'ldp x29, x30, [x1]\n ldr x0, [x1, #8]\n ldr x0, [x0, #0]\n',
  // ccmp (register data processing, bits 29:27 = 111) must not be taken for a load
  'mov x19, #5\n ldr x0, [x1]\n cmp x19, #3\n ccmp x19, #0, #0, ne\n cset x3, ls\n ccmp x19, x0, #2, eq\n cset x4, hi\n',
  // the kernel's strchr: post-index byte loads with cmp/ccmp/b.ne
  'mov x0, x2\n mov w9, #0x41\n strb w9, [x2, #5]\n strb wzr, [x2, #9]\n mov w1, #0x41\n and w1, w1, #0xff\n1: ldrb w3, [x0], #1\n cmp w3, w1\n ccmp w3, wzr, #4, ne\n b.ne 1b\n sub x0, x0, #1\n cmp w3, w1\n csel x0, x0, xzr, eq\n',
];
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'wasm-jit-'));
function assemble(src) {
  fs.writeFileSync(path.join(tmp, 't.s'), `.text\n${src}\n brk #0\n`);
  execFileSync('clang', ['--target=aarch64-none-elf', '-c', path.join(tmp, 't.s'), '-o', path.join(tmp, 't.o')]);
  const e = fs.readFileSync(path.join(tmp, 't.o'));
  const shoff = Number(e.readBigUInt64LE(0x28)), shentsize = e.readUInt16LE(0x3a), shnum = e.readUInt16LE(0x3c), shstrndx = e.readUInt16LE(0x3e);
  const strOff = Number(e.readBigUInt64LE(shoff + shstrndx * shentsize + 0x18));
  for (let i = 0; i < shnum; i++) {
    const h = shoff + i * shentsize;
    const nameAt = strOff + e.readUInt32LE(h);
    const name = e.toString('latin1', nameAt, e.indexOf(0, nameAt));
    if (name === '.text') { const off = Number(e.readBigUInt64LE(h + 0x18)), size = Number(e.readBigUInt64LE(h + 0x20)); return e.subarray(off, off + size); }
  }
  throw new Error('no .text');
}
let ex, free = [], used = [];
const env = {
  sbc_disk_read: () => {},
  sbc_jit_compile: (ptr, len) => {
    const t = ex.__indirect_function_table;
    const inst = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len)), { env: { memory: ex.memory, table: t } });
    const idx = free.length ? free.pop() : t.grow(1);
    t.set(idx, inst.exports.f); used.push(idx); return idx;
  },
  sbc_jit_reset: () => { for (const i of used) free.push(i); used = []; },
};
({ instance: { exports: ex } } = await WebAssembly.instantiate(fs.readFileSync('web/wasm/sbc-emu.wasm'), { env }));
let fails = 0;
for (const [k, s] of SNIPPETS.entries()) {
  const code = assemble(s);
  const p = ex.sbc_alloc(code.length);
  new Uint8Array(ex.memory.buffer).set(code, p);
  const run = (jit) => { const q = ex.sbc_selftest(p, code.length, jit); return Array.from(new BigUint64Array(ex.memory.buffer, q, 32)); };
  const a = run(0), b = run(1);
  const diff = a.map((v, i) => v !== b[i] ? `${i === 31 ? 'nzcv' : 'x' + i}: interp ${v.toString(16)} jit ${b[i].toString(16)}` : null).filter(Boolean);
  if (diff.length) { fails++; console.log(`snippet ${k} FAIL: ${s.trim().replace(/\n/g, ';')}\n  ${diff.join('\n  ')}`); }
}
console.log(fails ? `${fails} failing snippets` : `all ${SNIPPETS.length} snippets match`);
process.exit(fails ? 1 : 0);
