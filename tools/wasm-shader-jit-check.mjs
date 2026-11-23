// The Jetson Nano's shader JIT in the browser build against the interpreter: random blocks of the
// instructions it compiles, run both ways in web/wasm/sbc-emu.wasm (maxwell-shader/src/jit/check.rs).
//   node tools/wasm-shader-jit-check.mjs [cases] [seed]     (SBC_WASM picks another build)
import fs from 'node:fs';

let ex, jitFree = [], jitUsed = new Set();
function jitCompile(ptr, len) {
  ptr >>>= 0;
  const t = ex.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: ex.memory, table: t } });
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.add(idx); return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed.clear(); }

const cases = Number(process.argv[2] || 20000), seed = Number(process.argv[3] || 0x12345679);
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const t0 = performance.now();
const r = ex.sbc_shader_jit_check(seed, cases);
const report = new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ex.sbc_shader_jit_report() >>> 0, ex.sbc_shader_jit_report_len()));
console.log(`${r === 0 ? 'PASS' : 'FAIL'}: ${cases} random blocks, ${((performance.now() - t0) / 1000).toFixed(1)} s, ${jitUsed.size} modules`);
console.log(report);
process.exit(r === 0 ? 0 : 1);
