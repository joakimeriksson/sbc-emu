// Headless check of the Tinker Board S in the browser build: boots web/wasm/sbc-emu.wasm the
// way web/worker.js does (direct boot: zImage, the stock DTB, the BusyBox initramfs of
// scripts/tinker-board-fixture.sh), waits for the shell on the serial console, runs a few
// commands on all four cores and reports the times.
//   node tools/wasm-smoke-tinker.mjs [seconds] [dir]     (dir: cache/tinker-board)
//   node tools/wasm-smoke-tinker.mjs [seconds] --sd IMAGE  (an Armbian image: to `login:`)
// SBC_JIT=0 turns the JIT off, SBC_JIT_HOT=n sets the compile threshold, SBC_WASM picks another
// build. SBC_BOARD=orangepi-one runs the same check on the Orange Pi One instead (direct boot of
// scripts/orangepi-one-fixture.sh's Alpine kernel, DTB and initramfs; dir: cache/orangepi-one).
import fs from 'node:fs';

let jitEx, jitFree = [], jitUsed = new Set();
function jitCompile(ptr, len) {
  const t = jitEx.__indirect_function_table;
  const mod = new WebAssembly.Module(new Uint8Array(jitEx.memory.buffer).slice(ptr, ptr + len));
  const inst = new WebAssembly.Instance(mod, { env: { memory: jitEx.memory, table: t } });
  const idx = jitFree.length ? jitFree.pop() : t.grow(1);
  t.set(idx, inst.exports.f); jitUsed.add(idx); return idx;
}
function jitReset() { const t = jitEx.__indirect_function_table; for (const i of jitUsed) { t.set(i, null); jitFree.push(i); } jitUsed.clear(); }

const secs = Number(process.argv[2] || 600);
const opi1 = process.env.SBC_BOARD === 'orangepi-one';
const sd = process.argv[3] === '--sd' ? process.argv[4] : null, dir = sd ? null : process.argv[3] || (opi1 ? 'cache/orangepi-one' : 'cache/tinker-board');
const prompt = opi1 ? 'orangepi-one#' : 'tinker-board#';
let fd = sd ? fs.openSync(sd, 'r') : null;
// the SD card, read like the worker's range requests (synchronously, into the emulator's memory)
function diskRead(lba, count, ptr) { fs.readSync(fd, new Uint8Array(jitEx.memory.buffer, ptr, count * 512), 0, count * 512, lba * 512); }
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: diskRead, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
const ex = instance.exports;
jitEx = ex;
const mem = () => new Uint8Array(ex.memory.buffer);
const load = (f) => { const b = fs.readFileSync(f); const p = ex.sbc_alloc(b.length); mem().set(b, p); return [p, b.length]; };
if (sd) {
  if (ex.sbc_boot_tinker_sd(1024, Date.now() / 1000, fs.fstatSync(fd).size / 512) !== 0) throw new Error('SD boot setup failed');
} else if (opi1) {
  const [kp, kl] = load(`${dir}/zImage`), [dp, dl] = load(`${dir}/sun8i-h3-orangepi-one.dtb`), [ip, il] = load(`${dir}/initramfs.cpio.gz`);
  if (ex.sbc_boot_opi1(512, kp, kl, dp, dl, ip, il, 0, 0) !== 0) throw new Error('boot setup failed');
} else {
  const [kp, kl] = load(`${dir}/zImage`), [dp, dl] = load(`${dir}/rk3288-tinker-s.dtb`), [ip, il] = load(`${dir}/initramfs.cpio.gz`);
  if (ex.sbc_boot_tinker(1024, Date.now() / 1000, kp, kl, dp, dl, ip, il, 0, 0) !== 0) throw new Error('boot setup failed');
}
console.log('jit', process.env.SBC_JIT === '0' ? (ex.sbc_jit(0), 'off') : (ex.sbc_jit(1) ? 'on' : 'unavailable'));
if (process.env.SBC_JIT_HOT) ex.sbc_jit_hot(Number(process.env.SBC_JIT_HOT));

const t0 = performance.now(), wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const steps = sd ? [
  ['login:', null],
] : [
  [prompt, 'uname -m; grep -c ^processor /proc/cpuinfo\r'],
  [prompt, 'for i in 1 2 3 4; do (n=0; while [ $n -lt 3000 ]; do n=$((n+1)); done) & done; wait; echo PAR_DONE\r'],
  ['PAR_DONE', null],
];
let uart = '', seen = 0, step = 0, pending = [], lastProgress = 0;
const marks = [];
while (performance.now() - t0 < secs * 1000 && step < steps.length) {
  while (pending.length && ex.sbc_uart_room()) ex.sbc_uart_in(pending.shift());
  const r = ex.sbc_run(2_000_000, wall());
  const n = ex.sbc_uart_take();
  if (n) { const s = new TextDecoder().decode(mem().slice(ex.sbc_uart_ptr(), ex.sbc_uart_ptr() + n)); uart += s; process.stdout.write(s); }
  const [want, send] = steps[step];
  const at = uart.indexOf(want, seen);
  if (at >= 0) {
    seen = at + want.length;
    marks.push(`${want}: ${((performance.now() - t0) / 1000).toFixed(1)} s wall, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns, guest ${(ex.sbc_now_ns() / 1e9).toFixed(2)} s`);
    if (send) pending.push(...Buffer.from(send));
    step++;
  }
  if (process.env.SBC_PROGRESS && performance.now() - lastProgress > 10000) {
    lastProgress = performance.now();
    console.error(`[${((lastProgress - t0) / 1000).toFixed(0)} s] ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns, guest ${(ex.sbc_now_ns() / 1e9).toFixed(1)} s, pc ${ex.sbc_stop_pc().toString(16)}, ${ex.sbc_jit_blocks()} blocks`);
  }
  if (r === 2) { console.log('\nstopped', ex.sbc_stop_kind(), ex.sbc_stop_pc().toString(16), (ex.sbc_stop_insn() >>> 0).toString(16)); break; }
  if (r === 1) { await sleep(Math.min(ex.sbc_wait_ms(), 5)); ex.sbc_wake(wall()); }
}
const el = (performance.now() - t0) / 1000;
console.log(`\n--- ${step === steps.length ? 'PASS' : 'INCOMPLETE'}: wall ${el.toFixed(1)} s, ${(ex.sbc_insns() / 1e9).toFixed(2)} G insns (${(ex.sbc_insns() / el / 1e6).toFixed(0)} MIPS), ${ex.sbc_jit_blocks()} blocks compiled`);
for (const m of marks) console.log('  ' + m);
process.exit(step === steps.length ? 0 : 1);
