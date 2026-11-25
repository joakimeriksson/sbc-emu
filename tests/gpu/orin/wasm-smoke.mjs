import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';

// Run the site's real worker and JIT in Node, serving build output from disk.
const web = new URL('../../../web/', import.meta.url);
let serial = '';
let position;
let finished = false;
let scheduled;

const timeout = setTimeout(() => {
  throw new Error(`No moving Orin framebuffer: ${serial.trim()}`);
}, 15000);

function postMessage(message) {
  if (message.type === 'status') {
    assert.notEqual(message.phase, 'error', message.text);
    assert.ok(!message.text.startsWith('stopped'), message.text);
  }
  if (message.type === 'uart') serial += message.text;
  if (message.type !== 'frame' || !serial.includes('GA10B_COPY_DEMO_READY')) return;

  const { w, h, data } = message;
  assert.deepEqual([w, h], [640, 360]);
  const pixel = (x, y) => Array.from(data.subarray((y * w + x) * 4, (y * w + x) * 4 + 4));
  assert.deepEqual(pixel(493, 94), [0xf3, 0xad, 0x70, 255]);
  const x = Array.from({ length: w }, (_, i) => i).find(i => {
    const [r, g, b] = pixel(i, 250);
    return r === 0x30 && g === 0xba && b === 0xae;
  });
  assert.notEqual(x, undefined, 'rover sprite missing');
  if (position === undefined) position = x;
  if (position !== x) {
    assert.ok(context.exportsForTest().sbc_jit_blocks() > 0, 'AArch64 JIT did not compile any blocks');
    finished = true;
    clearTimeout(scheduled);
    clearTimeout(timeout);
    console.log(`Orin WASM worker: ${w}x${h}, rover moved from x=${position} to x=${x}`);
  }
}

const context = {
  performance, WebAssembly, TextDecoder, Response,
  fetch: async path => new Response(await readFile(new URL(path, web)), {
    headers: { 'content-type': path.endsWith('.wasm') ? 'application/wasm' : 'application/octet-stream' },
  }),
  setTimeout: (fn, ms) => { if (!finished) scheduled = setTimeout(fn, ms); },
  postMessage,
  console,
};
runInNewContext(await readFile(new URL('worker.js', web), 'utf8') +
  '\nfunction exportsForTest() { return ex; }', context);
context.onmessage({ data: { type: 'start', os: 'jetson-orin-nano-gpu', ramMb: 256, jit: true } });
