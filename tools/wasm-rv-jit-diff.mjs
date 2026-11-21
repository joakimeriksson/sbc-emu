// The RV64 JIT's WebAssembly backend against the interpreter: random programs (riscv64/src/
// jit_check.rs) run both ways in web/wasm/sbc-emu.wasm, in M mode and in S mode with Sv39,
// straight and in loops, compiling at the first execution and at the second.
//   node tools/wasm-rv-jit-diff.mjs [cases] [seed]
import fs from 'node:fs';

let ex, free = [], used = new Set();
function jitCompile(ptr, len) {
  const t = ex.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: ex.memory, table: t } });
  const idx = free.length ? free.pop() : t.grow(1);
  t.set(idx, inst.exports.f); used.add(idx); return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of used) { t.set(i, null); free.push(i); } used.clear(); }
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const cases = Number(process.argv[2] || 2000), seed = Number(process.argv[3] || 12345);
let bad = 0;
for (const [loops, smode, hot] of [[0, 0, 1], [1, 0, 1], [0, 1, 1], [1, 1, 1], [1, 0, 2], [1, 1, 3]]) {
  const t0 = performance.now();
  const f = ex.sbc_rv_jit_diff(loops, smode, cases, seed + loops * 7 + smode * 13 + hot, hot);
  const what = `${loops ? 'loops' : 'straight'}, ${smode ? 'S mode (Sv39)' : 'M mode'}, hot ${hot}`;
  console.log(`${what}: ${f} of ${cases} differ (${((performance.now() - t0) / 1000).toFixed(1)} s)`);
  if (f) {
    bad += f;
    console.log(new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ex.sbc_report_ptr(), ex.sbc_report_len())));
  }
}
process.exit(bad ? 1 : 0);
