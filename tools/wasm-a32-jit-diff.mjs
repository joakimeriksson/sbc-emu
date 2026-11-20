// The A32/T32 JIT's random differential test with the WebAssembly backend: random A32 and
// Thumb code, straight-line and in loops, runs once interpreted and once compiled in
// web/wasm/sbc-emu.wasm (the page's JIT hooks below); registers, flags, the instruction
// count and memory must match.
//   node tools/wasm-a32-jit-diff.mjs [cases] [seed]
//   node tools/wasm-a32-jit-diff.mjs repro arm|thumb SEED HEX... [loops]  (one case)
import fs from 'node:fs';
let ex, free = [], used = [];
function jitCompile(ptr, len) {
  const t = ex.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: ex.memory, table: t } });
  const idx = free.length ? free.pop() : t.grow(1);
  t.set(idx, inst.exports.f); used.push(idx); return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of used) { t.set(i, null); free.push(i); } used = []; }
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const report = () => new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ex.sbc_report_ptr(), ex.sbc_report_len()));
if (process.argv[2] === 'repro') {
  const thumb = process.argv[3] === 'thumb' ? 1 : 0, seed = Number(process.argv[4]);
  const loops = process.argv.at(-1) === 'loops' ? 1 : 0;
  const words = process.argv.slice(5).filter((a) => a !== 'loops').map((h) => parseInt(h, 16));
  const p = ex.sbc_alloc(4 * words.length);
  new Uint32Array(ex.memory.buffer, p, words.length).set(words);
  const bad = ex.sbc_a32_jit_case(thumb, p, words.length, seed, loops);
  console.log(bad ? `FAIL: ${report()}` : 'ok');
  process.exit(bad ? 1 : 0);
}
const cases = Number(process.argv[2] || 2000), seed = Number(process.argv[3] || 12345);
let failed = 0;
for (const [thumb, loops, name] of [[0, 0, 'A32'], [1, 0, 'Thumb'], [0, 1, 'A32 loops'], [1, 1, 'Thumb loops']]) {
  const t0 = performance.now();
  const bad = ex.sbc_a32_jit_diff(thumb, loops, cases, seed);
  console.log(`${name}: ${cases - bad}/${cases} match (${((performance.now() - t0) / 1000).toFixed(1)} s)`);
  if (bad) { console.log('  first failure: ' + report()); failed++; }
}
process.exit(failed ? 1 : 0);
