// Headless check of the Raspberry Pi 3 in the browser build: boots web/wasm/sbc-emu.wasm the way
// web/worker.js does for web/rpi3.html and reports the times.
//   node tools/wasm-smoke-rpi3.mjs [seconds] [dir]          the Pi kernel and the BusyBox
//       initramfs (dir: cache/rpi3 with Image and initramfs.cpio, see scripts/rpi3-shell.txt):
//       the shell on the mini UART, the board's identity, four cores busy at once
//   node tools/wasm-smoke-rpi3.mjs [seconds] --sd IMAGE [out.png]   a Raspberry Pi OS card as the
//       firmware boots it, to the serial login prompt and the first-boot screen on HDMI (KMS)
// SBC_MODEL=3b+|3b|3a+|zero2w picks the board (default 3b+), SBC_JIT=0 turns the JIT off, SBC_WASM
// picks another build.
import fs from 'node:fs';
import zlib from 'node:zlib';

let ex, jitFree = [], jitUsed = new Set();
function jitCompile(ptr, len) {
  const t = ex.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(ex.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: ex.memory, table: t } });
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.add(idx); return idx;
}
function jitReset() { const t = ex.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed.clear(); }

const MODELS = ['3b+', '3b', '3a+', 'zero2w'];
const NAMES = ['Raspberry Pi 3 Model B Plus Rev 1.3', 'Raspberry Pi 3 Model B Rev 1.2', 'Raspberry Pi 3 Model A Plus Rev 1.0', 'Raspberry Pi Zero 2 W Rev 1.0'];
const model = Math.max(0, MODELS.indexOf(process.env.SBC_MODEL ?? '3b+'));
const secs = Number(process.argv[2] || 600);
const sd = process.argv[3] === '--sd' ? process.argv[4] : null;
const dir = sd ? null : process.argv[3] || 'cache/rpi3';
const png = sd ? process.argv[5] : null;
const fd = sd ? fs.openSync(sd, 'r') : null;
let readBytes = 0;
// the SD card, read like the worker's range requests (synchronously, into the emulator's memory)
function diskRead(lba, count, ptr) {
  const buf = new Uint8Array(ex.memory.buffer, ptr >>> 0, count * 512);
  const n = fs.readSync(fd, buf, 0, count * 512, lba * 512);
  if (n < count * 512) buf.fill(0, n);
  readBytes += count * 512;
}
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: diskRead, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const mem = () => new Uint8Array(ex.memory.buffer);
const load = (f) => { const b = fs.readFileSync(f); const p = ex.sbc_alloc(b.length); mem().set(b, p); return [p, b.length]; };
ex.sbc_kms(1280, 720);
if (sd) {
  const r = ex.sbc_boot_pi3_sd(model, 0, fs.fstatSync(fd).size / 512);
  if (r !== 0) throw new Error(`sbc_boot_pi3_sd: ${r}`);
} else {
  const [kp, kl] = load(`${dir}/Image`), [ip, il] = load(`${dir}/initramfs.cpio`);
  if (ex.sbc_boot_pi3(model, 0, kp, kl, ip, il, 0, 0) !== 0) throw new Error('boot setup failed');
}
console.log(`board ${ex.sbc_board()} (${MODELS[model]}), jit`, process.env.SBC_JIT === '0' ? (ex.sbc_jit(0), 'off') : (ex.sbc_jit(1) ? 'on' : 'unavailable'));

const t0 = performance.now(), wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const steps = sd ? [
  [`Machine model: ${NAMES[model]}`, null],
  ['smp: Brought up 1 node, 4 CPUs', null],
  ['EXT4-fs (mmcblk0p2): mounted filesystem', null],
  ['raspberrypi login:', null],
] : [
  ['pi3#', 'cat /proc/device-tree/model; echo; grep -c ^processor /proc/cpuinfo; uname -m\r'],
  [NAMES[model], null],
  ['aarch64', 'for i in 1 2 3 4; do (n=0; while [ $n -lt 3000 ]; do n=$((n+1)); done) & done; wait; echo PAR_$((4 * 10))\r'],
  ['PAR_40', null],
];
let uart = '', seen = 0, step = 0, pending = [], stopped = false;
const marks = [];
while (performance.now() - t0 < secs * 1000 && step < steps.length) {
  while (pending.length && ex.sbc_uart_room()) ex.sbc_uart_in(pending.shift());
  const r = ex.sbc_run(2_000_000, wall());
  const n = ex.sbc_uart_take();
  if (n) { const s = new TextDecoder().decode(mem().slice(ex.sbc_uart_ptr(), ex.sbc_uart_ptr() + n)); uart += s; if (!sd) process.stdout.write(s); }
  const [want, send] = steps[step];
  const at = uart.indexOf(want, seen);
  if (at >= 0) {
    seen = at + want.length;
    marks.push(`${want}: ${((performance.now() - t0) / 1000).toFixed(1)} s wall, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns, guest ${(ex.sbc_now_ns() / 1e9).toFixed(2)} s` + (sd ? `, SD read ${(readBytes / 2 ** 20).toFixed(0)} MiB` : ''));
    if (send) pending.push(...Buffer.from(send));
    step++;
  }
  if (r === 2) { console.log('\nstopped', ex.sbc_stop_kind(), ex.sbc_stop_pc().toString(16), (ex.sbc_stop_insn() >>> 0).toString(16)); stopped = true; break; }
  if (r === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
}
// the screen: HDMI (KMS) for a card, the firmware framebuffer for the direct boot
let lit = 0, size = 'none';
const p = ex.sbc_fb_render() >>> 0;
if (p) {
  const w = ex.sbc_fb_w(), h = ex.sbc_fb_h(), px = new Uint8Array(ex.memory.buffer, p, w * h * 4);
  for (let i = 0; i < px.length; i += 4) if (px[i] | px[i + 1] | px[i + 2]) lit++;
  size = `${w}x${h}, ${(100 * lit / (w * h)).toFixed(1)}% lit`;
  if (png) {
    const raw = Buffer.alloc((w * 4 + 1) * h);
    for (let y = 0; y < h; y++) Buffer.from(px.buffer, p + y * w * 4, w * 4).copy(raw, y * (w * 4 + 1) + 1);
    const T = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
    const crc = (b) => { let c = ~0; for (const x of b) c = T[(c ^ x) & 255] ^ (c >>> 8); return (~c) >>> 0; };
    const chunk = (t, d) => { const l = Buffer.alloc(4); l.writeUInt32BE(d.length); const td = Buffer.concat([Buffer.from(t), d]); const c = Buffer.alloc(4); c.writeUInt32BE(crc(td)); return Buffer.concat([l, td, c]); };
    const ih = Buffer.alloc(13); ih.writeUInt32BE(w, 0); ih.writeUInt32BE(h, 4); ih.set([8, 6, 0, 0, 0], 8);
    fs.writeFileSync(png, Buffer.concat([Buffer.from([0x89, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ih), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]));
  }
}
const el = (performance.now() - t0) / 1000;
const pass = step === steps.length && !stopped && lit > 0;
console.log(`\n--- ${pass ? 'PASS' : 'INCOMPLETE'}: ${MODELS[model]}, wall ${el.toFixed(1)} s, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns (${(ex.sbc_insns() / el / 1e6).toFixed(0)} MIPS), ${ex.sbc_jit_blocks()} blocks compiled; screen ${size}`);
for (const m of marks) console.log('  ' + m);
process.exit(pass ? 0 : 1);
