// The Raspberry Pi 3's GPU in the browser build without a browser: replay captured VideoCore IV
// jobs (SBC_VC4_CAPTURE's jobs.bin) in web/wasm/sbc-emu.wasm (V8, like Chrome), with the QPU JIT
// (WebAssembly code) and without, and compare the frames.
//   node tools/wasm-vc4-replay.mjs dir/jobs.bin [rounds]     (SBC_WASM picks another build)
import fs from 'node:fs';

let ex, jitFree = [], jitUsed = [];
function jitCompile(ptr, len) {
  ptr >>>= 0;
  const t = ex.__indirect_function_table;
  const inst = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len)), { env: { memory: ex.memory, table: t } });
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.push(idx); return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed = []; }

const file = process.argv[2], rounds = Number(process.argv[3] || 2);
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const data = fs.readFileSync(file);
const ptr = ex.sbc_alloc(data.length) >>> 0;
new Uint8Array(ex.memory.buffer, ptr, data.length).set(data);
const best = { 0: Infinity, 1: Infinity }, sums = { 0: new Set(), 1: new Set() };
for (let r = 0; r < rounds; r++) {
  for (const jit of [0, 1]) {
    const t0 = performance.now();
    const sum = ex.sbc_vc4_replay(ptr, data.length, jit) >>> 0;
    const ms = performance.now() - t0;
    best[jit] = Math.min(best[jit], ms);
    sums[jit].add(sum);
    console.log(`jit ${jit}: ${ms.toFixed(0)} ms, frames ${sum.toString(16)}`);
  }
}
const same = sums[0].size === 1 && sums[1].size === 1 && [...sums[0]][0] === [...sums[1]][0];
console.log(`best: interpreter ${best[0].toFixed(0)} ms, JIT ${best[1].toFixed(0)} ms (${(best[0] / best[1]).toFixed(2)}x), ${ex.sbc_vc4_jit_blocks()} blocks; frames ${same ? 'identical' : 'DIFFER'}`);
process.exit(same ? 0 : 1);
