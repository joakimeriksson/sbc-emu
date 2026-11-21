// The browser GPU demos without a browser: restore web/assets/gpu-demo.snap in the WebAssembly
// build (V8, like Chrome), run each demo for a while on the emulated V3D, report its frame rate
// and save the HDMI output.
//   node tools/wasm-gpu-bench.mjs [seconds-per-demo] [demo ...]
// SBC_SNAP and SBC_SD pick other files. Without the SD image (the public site has none) the card
// reads as zeros, with SBC_SECTORS sectors (default 8 GiB), and every read is counted: the demos
// should run from the snapshot's page cache alone (SBC_DISKLOG=1 lists each read). Exits 1 if a
// demo drew no frames.
import fs from 'node:fs';
import zlib from 'node:zlib';
let ex, jitFree = [], jitUsed = [];
function jitCompile(ptr, len) {
  const t = ex.__indirect_function_table;
  const inst = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len)), { env: { memory: ex.memory, table: t } });
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.push(idx); return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed = []; }
const img = process.env.SBC_SD ?? 'web/assets/raspios-gpu.img';
const fd = fs.existsSync(img) ? fs.openSync(img, 'r') : null;
const size = fd !== null ? fs.fstatSync(fd).size : Number(process.env.SBC_SECTORS ?? 16777216) * 512;
let diskBytes = 0;
const env = { sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now(),
  sbc_disk_read: (lba, count, ptr) => {
    const b = Buffer.from(ex.memory.buffer, ptr, count * 512); diskBytes += count * 512;
    if (process.env.SBC_DISKLOG) console.log(`SD read: sector ${lba}, ${count} sectors`);
    const n = fd !== null ? fs.readSync(fd, b, 0, count * 512, lba * 512) : 0; if (n < count * 512) b.fill(0, n);
  } };
ex = (await WebAssembly.instantiate(fs.readFileSync('web/wasm/sbc-emu.wasm'), { env })).instance.exports;
const snap = fs.readFileSync(process.env.SBC_SNAP ?? 'web/assets/gpu-demo.snap');
const p = ex.sbc_alloc(snap.length);
new Uint8Array(ex.memory.buffer).set(snap, p);
if (ex.sbc_restore(1024, size / 512, p, snap.length) !== 0) throw new Error('restore failed');
ex.sbc_jit(1);
const t0 = performance.now();
const wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function run(seconds) {
  const end = performance.now() + seconds * 1000;
  while (performance.now() < end) {
    const st = ex.sbc_run(4_000_000, wall());
    if (st === 2) throw new Error('machine stopped');
    if (st === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
  }
}
const type = (s) => { for (const c of Buffer.from(s)) ex.sbc_type(c); };
function png(out) {
  const fp = ex.sbc_fb_render(); if (!fp) return;
  const w = ex.sbc_fb_w(), h = ex.sbc_fb_h();
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) Buffer.from(ex.memory.buffer, fp + y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  const T = Array.from({ length: 256 }, (_, n) => { let c = n; for (let i = 0; i < 8; i++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
  const crc = (b) => { let c = ~0; for (const x of b) c = T[(c ^ x) & 255] ^ (c >>> 8); return (~c) >>> 0; };
  const chunk = (t, d) => { const l = Buffer.alloc(4); l.writeUInt32BE(d.length); const td = Buffer.concat([Buffer.from(t), d]); const c = Buffer.alloc(4); c.writeUInt32BE(crc(td)); return Buffer.concat([l, td, c]); };
  const ih = Buffer.alloc(13); ih.writeUInt32BE(w, 0); ih.writeUInt32BE(h, 4); ih.set([8, 6, 0, 0, 0], 8);
  fs.writeFileSync(out, Buffer.concat([Buffer.from([0x89, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ih), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]));
}
const DEMOS = { cube: 'kmscube', horse: 'glmark2-es2-drm --run-forever -b build:duration=30', phong: 'glmark2-es2-drm --run-forever -b shading:shading=phong:duration=30',
  cel: 'glmark2-es2-drm --run-forever -b shading:shading=cel:duration=30', jelly: 'glmark2-es2-drm --run-forever -b jellyfish:duration=30',
  desktop: 'glmark2-es2-drm --run-forever -b desktop:effect=shadow:duration=30', refract: 'glmark2-es2-drm --run-forever -b refract:duration=30' };
const secs = Number(process.argv[2] ?? 25);
const which = process.argv.length > 3 ? process.argv.slice(3) : Object.keys(DEMOS);
await run(2);
fs.mkdirSync('out', { recursive: true });
const failed = [];
for (const d of which) {
  type('\x03'); await run(1);
  type(`clear; ${DEMOS[d]}\n`);
  await run(6); // start-up: loading the program and Mesa, compiling shaders
  const f0 = ex.sbc_gpu_frames(), g0 = ex.sbc_gpu_ms(), w0 = performance.now(), i0 = ex.sbc_insns();
  await run(secs);
  const dt = (performance.now() - w0) / 1000;
  if (ex.sbc_gpu_frames() === f0) failed.push(d);
  console.log(`${d.padEnd(8)} ${((ex.sbc_gpu_frames() - f0) / dt).toFixed(1).padStart(5)} frames/s   GPU busy ${(100 * (ex.sbc_gpu_ms() - g0) / 1000 / dt).toFixed(0)}%   CPU ${((ex.sbc_insns() - i0) / dt / 1e6).toFixed(0)} M instructions/s`);
  png(`out/wasm-${d}.png`);
}
type('\x03');
console.log(`SD card: ${(diskBytes / 1e6).toFixed(1)} MB read${fd === null ? ' (no image: zeros)' : ''}`);
if (failed.length) { console.error(`no frames from: ${failed.join(' ')}`); process.exit(1); }
