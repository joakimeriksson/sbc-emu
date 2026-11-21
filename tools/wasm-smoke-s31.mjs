// Headless check of the ESP32-S31 in the browser build: boots web/wasm/sbc-emu.wasm the way
// web/worker.js does (Espressif's mask ROM ELF and the official Linux flash image of
// scripts/esp32s31-fixture.sh), waits for the root shell on UART0, runs a few commands and
// reports the times.
//   node tools/wasm-smoke-s31.mjs [seconds] [rom.elf] [flash.bin]
//   (default cache/esp32s31/esp32s31_rev0_rom.elf, cache/esp32s31/images/s31_full_flash.bin)
// SBC_JIT=0 turns the JIT off, SBC_WASM picks another build, SBC_PROGRESS=1 reports every 10 s.
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

const secs = Number(process.argv[2] || 600);
const romFile = process.argv[3] || 'cache/esp32s31/esp32s31_rev0_rom.elf', flashFile = process.argv[4] || 'cache/esp32s31/images/s31_full_flash.bin';
const { instance } = await WebAssembly.instantiate(fs.readFileSync(process.env.SBC_WASM ?? 'web/wasm/sbc-emu.wasm'),
  { env: { sbc_disk_read: () => {}, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
ex = instance.exports;
const mem = () => new Uint8Array(ex.memory.buffer);
const load = (f) => { const b = fs.readFileSync(f); const p = ex.sbc_alloc(b.length) >>> 0; mem().set(b, p); return [p, b.length]; };
const [rp, rl] = load(romFile), [fp, fl] = load(flashFile);
if (ex.sbc_boot_s31(rp, rl, fp, fl) !== 0) throw new Error('boot setup failed');
console.log('jit', ex.sbc_jit(process.env.SBC_JIT === '0' ? 0 : 1) ? 'on' : 'off');
if (process.env.SBC_JIT_HOT) ex.sbc_jit_hot(Number(process.env.SBC_JIT_HOT));

const t0 = performance.now(), wall = () => (performance.now() - t0) * 1e6;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const steps = [
  ['~ #', 'uname -a; cat /proc/device-tree/model; echo\r'],
  ['ESP32-S31', 'grep -c ^processor /proc/cpuinfo; echo start; sleep 1; echo SLEPT_$((40+2))\r'],
  ['SLEPT_42', null],
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
