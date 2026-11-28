// The QPU JIT in the browser build: the shared simd-jit backend against its IR (random blocks),
// then the Raspberry Pi 3 front end against its interpreter (random QPU programs), in
// web/wasm/sbc-emu.wasm (simd-jit/src/check.rs, vc4-3d/src/jit_check.rs).
//   node tools/wasm-qpu-jit-check.mjs [cases] [seed]     (SBC_WASM picks another build)
import fs from 'node:fs';

// Each case's module runs only in that case, so slots are taken back from the oldest on (a slot
// per module of tens of thousands would not fit in Node's heap).
let ex, jitFree = [], jitUsed = new Set(), modules = 0;
const KEEP = 64;
function jitCompile(ptr, len) {
  ptr >>>= 0;
  const t = ex.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: ex.memory, table: t } });
  if (jitUsed.size >= KEEP) { const old = jitUsed.values().next().value; jitUsed.delete(old); t.set(old, null); jitFree.push(old); }
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.add(idx); modules++; return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed.clear(); }

const cases = Number(process.argv[2] || 20000), seed = Number(process.argv[3] || 0x12345679);
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const t0 = performance.now();
const r = ex.sbc_qpu_jit_check(seed, cases);
const report = new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ex.sbc_shader_jit_report() >>> 0, ex.sbc_shader_jit_report_len()));
console.log(`${r === 0 ? 'PASS' : 'FAIL'}: ${cases} random blocks and programs, ${((performance.now() - t0) / 1000).toFixed(1)} s, ${modules} modules`);
console.log(report);
process.exit(r === 0 ? 0 : 1);
