// Headless check of the Jetson Nano in the browser build: boots web/wasm/sbc-emu.wasm the way
// web/worker.js does (Debian's kernel, the board's device tree and the initramfs of
// scripts/jetson-nano-debian-fixture.sh), waits for the shell on the serial console, runs a few
// commands on all four cores, checks the screen and the GPU's driver, reboots and reports the times.
//   node tools/wasm-smoke-nano.mjs [seconds] [dir]     (dir: cache/jetson-nano/debian)
// SBC_GPU=FILE loads the GPU demos (scripts/jetson-nano-gpu-demo-fixture.sh's gpu-demo.cpio.xz) behind
// the initramfs, unpacked as the page does, and instead runs the demos, stops one with Ctrl-C, and reports their frames per second
// (SBC_SHOTS=DIR: and writes a picture of each).
// SBC_JIT=0 turns the JIT off, SBC_JIT_HOT=n sets the compile threshold, SBC_WASM picks another
// build, SBC_RAM_MB the guest's RAM (1024, as the page), SBC_PROGRESS=1 reports every 10 s.
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';

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

const secs = Number(process.argv[2] || 600), dir = process.argv[3] || 'cache/jetson-nano/debian';
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const mem = () => new Uint8Array(ex.memory.buffer);
const put = (b) => { const p = ex.sbc_alloc(b.length) >>> 0; mem().set(b, p); return [p, b.length]; };
const load = (f) => put(fs.readFileSync(f));
const [kp, kl] = load(`${dir}/vmlinuz`), [dp, dl] = load(`${dir}/tegra210-p3450-0000.dtb`), [ip, il] = (() => {
  const a = fs.readFileSync(`${dir}/initramfs.cpio.gz`);
  if (!process.env.SBC_GPU) return put(a);
  return put(Buffer.concat([a, Buffer.alloc(-a.length & 3), execFileSync('xz', ['-dc', process.env.SBC_GPU], { maxBuffer: 2 ** 28 })]));
})();
const cmd = Buffer.from('console=tty0 console=ttyS0,115200 rdinit=/init'), cp = ex.sbc_alloc(cmd.length) >>> 0;
mem().set(cmd, cp);
ex.sbc_kms(1280, 720);
if (ex.sbc_boot_nano(Number(process.env.SBC_RAM_MB ?? 1024), Date.now() / 1000, kp, kl, dp, dl, ip, il, cp, cmd.length) !== 0) throw new Error('boot setup failed');
console.log('jit', process.env.SBC_JIT === '0' ? (ex.sbc_jit(0), 'off') : (ex.sbc_jit(1) ? 'on' : 'unavailable'));
if (process.env.SBC_JIT_HOT) ex.sbc_jit_hot(Number(process.env.SBC_JIT_HOT));

const t0 = performance.now(), wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const demos = ['cubes', 'planet', 'swarm', 'fur', 'tunnel', 'shadow'];
// a picture of the screen: how many different colours it has
function shot(name) {
  const p = ex.sbc_fb_render() >>> 0;
  if (!p) return 0;
  const w = ex.sbc_fb_w(), h = ex.sbc_fb_h(), px = new Uint32Array(ex.memory.buffer, p, w * h);
  if (process.env.SBC_SHOTS) {
    const rgb = Buffer.alloc(w * h * 3);
    for (let i = 0; i < w * h; i++) for (let k = 0; k < 3; k++) rgb[3 * i + k] = px[i] >> 8 * k;
    fs.writeFileSync(`${process.env.SBC_SHOTS}/${name}.ppm`, Buffer.concat([Buffer.from(`P6 ${w} ${h} 255\n`), rgb]));
  }
  return w === 640 ? new Set(px).size : 0;
}
let dull = 0;
const steps = process.env.SBC_GPU ? [
  ['jetson-nano#', 'gpu-demo --seconds 6\r'],
  ...demos.map((d) => ['frames/s', () => { const n = shot(d); console.log(`[${d}: ${n} colours]`); if (n < 100) dull++; }]),
  ['frames/s', () => '\x03printf "STOPPED_%s\\n" OK\r'],
  ['STOPPED_OK', 'gpu-demo planet\r'],
  ['planet\r\n', '\x03printf "STOPPED_%s\\n" OK\r'],
  ['STOPPED_OK', null],
] : [
  ['jetson-nano#', 'uname -m; grep -c ^processor /proc/cpuinfo\r'],
  ['jetson-nano#', "awk 'BEGIN { x = atan2(1,1)*4; print \"fp\", x, sqrt(2) }'\r"],
  ['fp 3.14159 1.41421', 'for i in 1 2 3 4; do (n=0; while [ $n -lt 20000 ]; do n=$((n+1)); done; echo done$i) & done; wait; echo PAR_$((40+2))\r'],
  ['PAR_42', 'ls -l /sys/class/drm/renderD*/device/driver | grep nouveau && cat /sys/class/graphics/fb0/virtual_size && printf "DRM_%s\\n" OK\r'],
  ['DRM_OK', 'reboot -f\r'],
  ['TEGRA210_BOOT_OK', null],
  ['jetson-nano#', 'printf "REBOOT_%s\\n" OK\r'],
  ['REBOOT_OK', null],
];
let uart = '', seen = 0, step = 0, pending = [], lastProgress = 0;
const marks = [];
while (performance.now() - t0 < secs * 1000 && step < steps.length) {
  while (pending.length && ex.sbc_uart_room()) ex.sbc_uart_in(pending.shift());
  const r = ex.sbc_run(2_000_000, wall());
  const n = ex.sbc_uart_take();
  if (n) { const s = new TextDecoder().decode(mem().slice(ex.sbc_uart_ptr() >>> 0, (ex.sbc_uart_ptr() >>> 0) + n)); uart += s; process.stdout.write(s); }
  const [want, send] = steps[step];
  const at = uart.indexOf(want, seen);
  if (at >= 0) {
    seen = at + want.length;
    marks.push(`${want}: ${((performance.now() - t0) / 1000).toFixed(1)} s wall, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns, guest ${(ex.sbc_now_ns() / 1e9).toFixed(2)} s`);
    const text = typeof send === 'function' ? send() : send;
    if (text) pending.push(...Buffer.from(text));
    step++;
  }
  if (process.env.SBC_PROGRESS && performance.now() - lastProgress > 10000) {
    lastProgress = performance.now();
    console.error(`[${((lastProgress - t0) / 1000).toFixed(0)} s] ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns, guest ${(ex.sbc_now_ns() / 1e9).toFixed(1)} s, pc ${ex.sbc_stop_pc().toString(16)}, ${ex.sbc_jit_blocks()} blocks`);
  }
  if (r === 2) { console.log('\nstopped', ex.sbc_stop_kind(), ex.sbc_stop_pc().toString(16), (ex.sbc_stop_insn() >>> 0).toString(16)); break; }
  if (r === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
}
const p = ex.sbc_fb_render() >>> 0;
console.log(`\nscreen ${p ? `${ex.sbc_fb_w()} x ${ex.sbc_fb_h()}` : 'off'}`);
const ok = step === steps.length && p !== 0 && !dull && ex.sbc_fb_w() === 1280;
const el = (performance.now() - t0) / 1000;
console.log(`\n--- ${ok ? 'PASS' : 'INCOMPLETE'}: wall ${el.toFixed(1)} s, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns (${(ex.sbc_insns() / el / 1e6).toFixed(0)} MIPS), ${ex.sbc_jit_blocks()} blocks compiled`);
for (const m of marks) console.log('  ' + m);
process.exit(ok ? 0 : 1);
