// Headless check of the browser build: boots web/wasm/sbc-emu.wasm the way web/worker.js does,
// types a command, and writes the framebuffer to a PNG via the native screenshot path's format.
//   node tools/wasm-smoke.mjs [seconds] [out.png]
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
const secs = Number(process.argv[2] || 120), out = process.argv[3];
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'), { env: { sbc_disk_read: () => {}, sbc_jit_compile: (p, l) => jitCompile(p, l), sbc_jit_reset: () => jitReset() } });
const ex = instance.exports;
jitEx = ex;
jitEx = ex;
const mem = () => new Uint8Array(ex.memory.buffer);
const load = (f) => { const b = zlib.gunzipSync(fs.readFileSync(f)); const p = ex.sbc_alloc(b.length); mem().set(b, p); return [p, b.length]; };
const [kp, kl] = load('web/assets/kernel8.img'), [ip, il] = load('web/assets/initramfs.cpio.gz');
if (ex.sbc_boot(512, kp, kl, ip, il, 0, 0) !== 0) throw new Error('boot setup failed');
if (process.env.SBC_JIT !== '0') console.log('jit', ex.sbc_jit(1) ? 'on' : 'unavailable');
if (process.env.SBC_JIT_VERIFY === '1') ex.sbc_jit_verify(1);
const t0 = performance.now(), wall = () => (performance.now() - t0) * 1e6;
let uart = '', keys = [], typed = false, readyAt = 0;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
while (performance.now() - t0 < secs * 1000) {
  while (keys.length) ex.sbc_type(keys.shift());
  const r = ex.sbc_run(2_000_000, wall());
  const n = ex.sbc_uart_take();
  if (n) uart += new TextDecoder().decode(mem().slice(ex.sbc_uart_ptr(), ex.sbc_uart_ptr() + n));
  if (!readyAt && uart.includes('fscrypt-provisioning registered')) { readyAt = performance.now(); console.log(`kernel up after ${((readyAt - t0) / 1000).toFixed(1)} s wall, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns`); }
  if (readyAt && !typed && performance.now() - readyAt > 8000) { typed = true; keys.push(...Buffer.from('\recho hello from wasm; uname -m; cat /proc/cpuinfo | head -3\r')); }
  if (typed && keys.length === 0 && performance.now() - readyAt > 30000) break;
  if (r === 2) { console.log('stopped', ex.sbc_stop_kind(), ex.sbc_stop_pc().toString(16), (ex.sbc_stop_insn() >>> 0).toString(16)); break; }
  if (r === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
}
const el = (performance.now() - t0) / 1000;
if (process.env.SBC_JIT_VERIFY === '1') { const buf = ex.sbc_alloc(8192); const n = ex.sbc_jit_report(buf, 8192); console.log('jit verify:', n ? new TextDecoder().decode(new Uint8Array(ex.memory.buffer, buf, n)) : 'no mismatch'); }
console.log(`wall ${el.toFixed(1)} s, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns, guest ${(ex.sbc_now_ns() / 1e9).toFixed(1)} s`);
console.log(uart.split('\n').slice(-40).join('\n'));
const p = ex.sbc_fb_render();
if (p && out) {
  const w = ex.sbc_fb_w(), h = ex.sbc_fb_h(), px = mem().slice(p, p + w * h * 4);
  // tiny PNG (stored zlib via node's deflate)
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) { raw[y * (w * 4 + 1)] = 0; Buffer.from(px.buffer, y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1); }
  const crcT = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
  const crc = (b) => { let c = ~0; for (const x of b) c = crcT[(c ^ x) & 255] ^ (c >>> 8); return (~c) >>> 0; };
  const chunk = (t, d) => { const len = Buffer.alloc(4); len.writeUInt32BE(d.length); const td = Buffer.concat([Buffer.from(t), d]); const c = Buffer.alloc(4); c.writeUInt32BE(crc(td)); return Buffer.concat([len, td, c]); };
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(w, 0); ihdr.writeUInt32BE(h, 4); ihdr.set([8, 6, 0, 0, 0], 8);
  fs.writeFileSync(out, Buffer.concat([Buffer.from([0x89, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]));
  console.log(`framebuffer ${w}x${h} -> ${out}`);
}
