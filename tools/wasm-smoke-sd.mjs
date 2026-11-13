// Headless check of the browser SD path: web/wasm/sbc-emu.wasm boots an SD image through the
// same sbc_disk_read import the worker provides (here backed by file reads instead of HTTP).
//   node tools/wasm-smoke-sd.mjs IMAGE SECONDS [out.png]
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
const [img, secs = '600', out] = process.argv.slice(2);
const fd = fs.openSync(img, 'r');
const size = fs.fstatSync(fd).size;
let ex;
let readBytes = 0;
const env = { sbc_jit_compile: (p, l) => jitCompile(p, l), sbc_jit_reset: () => jitReset(), sbc_disk_read: (lba, count, ptr) => {
  const buf = Buffer.from(ex.memory.buffer, ptr, count * 512);
  const n = fs.readSync(fd, buf, 0, count * 512, lba * 512);
  if (n < count * 512) buf.fill(0, n);
  readBytes += count * 512;
} };
const { instance } = await WebAssembly.instantiate(fs.readFileSync('web/wasm/sbc-emu.wasm'), { env });
ex = instance.exports;
jitEx = ex;
const r = ex.sbc_boot_sd(1024, size / 512);
if (r !== 0) throw new Error(`sbc_boot_sd: ${r}`);
if (process.env.SBC_JIT !== '0') console.log('jit', ex.sbc_jit(1) ? 'on' : 'unavailable');
const t0 = performance.now(), wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((res) => setTimeout(res, ms));
let uart = '', marks = {};
const mark = (k) => { if (!marks[k]) { marks[k] = true; console.log(`${k}: wall ${((performance.now() - t0) / 1000).toFixed(0)} s, guest ${(ex.sbc_now_ns() / 1e9).toFixed(1)} s, ${(ex.sbc_insns() / 1e9).toFixed(1)} G insns, SD read ${(readBytes / 2 ** 20).toFixed(0)} MiB`); } };
while (performance.now() - t0 < Number(secs) * 1000) {
  const st = ex.sbc_run(4_000_000, wall());
  const n = ex.sbc_uart_take();
  if (n) uart += new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ex.sbc_uart_ptr(), n));
  if (uart.includes('Run /init')) mark('initramfs');
  if (uart.includes('systemd[1]')) mark('systemd');
  if (uart.includes('login:')) { mark('login prompt'); }
  if (st === 2) { console.log('stopped', ex.sbc_stop_kind(), ex.sbc_stop_pc().toString(16), (ex.sbc_stop_insn() >>> 0).toString(16)); break; }
  if (st === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
  if (marks['login prompt'] && !marks.done && performance.now() - t0 > 0) {
    // give the first-boot screen a moment, then stop
    if (!marks.t) marks.t = performance.now();
    if (performance.now() - marks.t > 30000) { marks.done = true; break; }
  }
}
mark('end');
const p = ex.sbc_fb_render();
if (p && out) {
  const w = ex.sbc_fb_w(), h = ex.sbc_fb_h(), px = new Uint8Array(ex.memory.buffer, p, w * h * 4);
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) Buffer.from(px.buffer, p + y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
  const T = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
  const crc = (b) => { let c = ~0; for (const x of b) c = T[(c ^ x) & 255] ^ (c >>> 8); return (~c) >>> 0; };
  const chunk = (t, d) => { const l = Buffer.alloc(4); l.writeUInt32BE(d.length); const td = Buffer.concat([Buffer.from(t), d]); const c = Buffer.alloc(4); c.writeUInt32BE(crc(td)); return Buffer.concat([l, td, c]); };
  const ih = Buffer.alloc(13); ih.writeUInt32BE(w, 0); ih.writeUInt32BE(h, 4); ih.set([8, 6, 0, 0, 0], 8);
  fs.writeFileSync(out, Buffer.concat([Buffer.from([0x89, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ih), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]));
  console.log(`screen ${w}x${h} -> ${out}`);
}
