// Boot the same NVIDIA fixture and drive its serial console using the native test script.
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';

const context = { WebAssembly, TextDecoder, performance, console };
const worker = await readFile('web/worker.js', 'utf8');
runInNewContext(worker + '\nfunction setExports(exports) { ex = exports; }', context);
const { instance } = await WebAssembly.instantiate(await readFile('web/wasm/sbc-emu.wasm'), {
  env: {
    sbc_jit_compile: context.jitCompile,
    sbc_jit_reset: context.jitReset,
    sbc_host_now_ms: () => performance.now(),
    sbc_disk_read: () => { throw new Error('unexpected disk read'); },
  },
});
const ex = instance.exports;
context.setExports(ex);

const memory = () => new Uint8Array(ex.memory.buffer);
async function load(path) {
  const data = await readFile(path);
  const ptr = ex.sbc_alloc(data.length) >>> 0;
  memory().set(data, ptr);
  return [ptr, data.length];
}

const [kernel, initrd, script, kind = "ioctl"] = process.argv.slice(2);
assert.equal(ex.sbc_boot_orin(768, ...await load(kernel), ...await load(initrd)), 0, 'boot setup failed');
assert.equal(ex.sbc_jit(1), 1, 'AArch64 JIT unavailable');

let serial = '';
let consumed = 0;
const out = new TextDecoder();
const pending = [];
const deadline = performance.now() + 120_000;
for (const line of (await readFile(script, 'utf8')).trim().split('\n')) {
  if (line === 'quit') break;
  if (line.startsWith('serial ')) {
    pending.push(...Buffer.from(line.slice(7).replace(/\\r/g, '\r')));
    continue;
  }
  assert.ok(line.startsWith('wait-serial '), `unrecognized command: ${line}`);
  const expected = line.slice(12);
  let found;
  while ((found = serial.indexOf(expected, consumed)) === -1) {
    if (performance.now() > deadline) throw new Error(`Timed out waiting for ${expected}\n${serial.slice(-4000)}`);
    while (pending.length && ex.sbc_uart_room()) ex.sbc_uart_in(pending.shift());
    const state = ex.sbc_run(2_000_000, ex.sbc_now_ns() + 20e6);
    assert.notEqual(state, 2, `guest stopped at 0x${ex.sbc_stop_pc().toString(16)}\n${serial.slice(-4000)}`);
    const size = ex.sbc_uart_take();
    if (size) {
      const ptr = ex.sbc_uart_ptr() >>> 0;
      serial += out.decode(memory().subarray(ptr, ptr + size));
    }
  }
  consumed = found + expected.length;
}
assert.ok(ex.sbc_jit_blocks() > 0);
if (kind === 'cuda') {
  assert.match(serial, /^CUDA_PROBE_ STATUS=0\s*$/m, serial.slice(-4000));
  const output = serial.match(/^CUDA kernel output=0x24681357 status=0[ \t\r]*$/m);
  assert.ok(output, serial.slice(-4000));
  console.log(`Orin WASM Linux GPU: ${output[0].trim()}`);
} else if (kind === 'egl') {
  assert.match(serial, /^EGL_PROBE_ STATUS=0\s*$/m, serial.slice(-4000));
  const pixel = serial.match(/^GL pixel: 255 0 0 255[ \t\r]*$/m);
  assert.ok(pixel, serial.slice(-4000));
  console.log(`Orin WASM Linux GPU: ${pixel[0].trim()}`);
} else {
  assert.equal(kind, 'ioctl');
  assert.match(serial, /^NVGPU_IOCTL_ STATUS=0\s*$/m, serial.slice(-4000));
  const irq = serial.match(/^\s*\d+:\s+[1-9]\d*\s+GICv3 48[0-7] Level\s+host1x_syncpt/m);
  assert.ok(irq, serial.slice(-4000));
  console.log('Orin WASM Linux GPU: NVGPU_IOCTL_ STATUS=0');
  console.log(irq[0].trim());
}
