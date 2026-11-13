// Headless check of the browser resume path: restore a snapshot in web/wasm/sbc-emu.wasm, log in
// on the emulated USB keyboard, run a command, write the screen to a PNG.
//   node tools/wasm-smoke-snap.mjs IMAGE SNAPSHOT [out.png]
import fs from 'node:fs';
import zlib from 'node:zlib';
// the page's JIT hooks (web/worker.js), for headless runs; SBC_JIT=0 disables the JIT
let jitEx, jitFree = [], jitUsed = [];
function jitCompile(ptr, len) {
  const t = jitEx.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(jitEx.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: jitEx.memory, table: t } });
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.push(idx); return idx;
}
function jitReset() { const t = jitEx.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed = []; }
const [img, snapPath, out] = process.argv.slice(2);
const fd = fs.openSync(img, 'r');
const size = fs.fstatSync(fd).size;
let ex;
const env = { sbc_jit_compile: (p, l) => jitCompile(p, l), sbc_jit_reset: () => jitReset(), sbc_disk_read: (lba, count, ptr) => {
  const buf = Buffer.from(ex.memory.buffer, ptr, count * 512);
  const n = fs.readSync(fd, buf, 0, count * 512, lba * 512);
  if (n < count * 512) buf.fill(0, n);
} };
const { instance } = await WebAssembly.instantiate(fs.readFileSync('web/wasm/sbc-emu.wasm'), { env });
ex = instance.exports;
jitEx = ex;
const snap = fs.readFileSync(snapPath);
const t0 = performance.now();
const p = ex.sbc_alloc(snap.length);
new Uint8Array(ex.memory.buffer).set(snap, p);
if (ex.sbc_restore(1024, size / 512, p, snap.length) !== 0) throw new Error('restore failed');
if (process.env.SBC_JIT !== '0') console.log('jit', ex.sbc_jit(1) ? 'on' : 'unavailable');
console.log(`restored in ${((performance.now() - t0) / 1000).toFixed(1)} s (guest time ${(ex.sbc_now_ns() / 1e9).toFixed(1)} s)`);
const wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const steps = [[3, 'pi\r'], [10, 'raspberry\r'], [30, 'echo hello from the browser build; uname -m; uptime\r'], [45, null]];
let k = 0;
while (k < steps.length) {
  const st = ex.sbc_run(4_000_000, wall());
  if (st === 2) { console.log('stopped'); break; }
  if (st === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
  const [at, text] = steps[k];
  if ((performance.now() - t0) / 1000 >= at) {
    if (text) for (const c of Buffer.from(text)) ex.sbc_type(c);
    k++;
  }
}
console.log(`done after ${((performance.now() - t0) / 1000).toFixed(0)} s wall, guest ${(ex.sbc_now_ns() / 1e9).toFixed(1)} s`);
const fp = ex.sbc_fb_render();
if (fp && out) {
  const w = ex.sbc_fb_w(), h = ex.sbc_fb_h();
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) Buffer.from(ex.memory.buffer, fp + y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  const T = Array.from({ length: 256 }, (_, n) => { let c = n; for (let i = 0; i < 8; i++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
  const crc = (b) => { let c = ~0; for (const x of b) c = T[(c ^ x) & 255] ^ (c >>> 8); return (~c) >>> 0; };
  const chunk = (t, d) => { const l = Buffer.alloc(4); l.writeUInt32BE(d.length); const td = Buffer.concat([Buffer.from(t), d]); const c = Buffer.alloc(4); c.writeUInt32BE(crc(td)); return Buffer.concat([l, td, c]); };
  const ih = Buffer.alloc(13); ih.writeUInt32BE(w, 0); ih.writeUInt32BE(h, 4); ih.set([8, 6, 0, 0, 0], 8);
  fs.writeFileSync(out, Buffer.concat([Buffer.from([0x89, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ih), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]));
  console.log(`screen -> ${out}`);
}
