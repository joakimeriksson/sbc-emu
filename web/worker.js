// Runs the emulator off the main thread.
// In:  {type:'start', os:'raspios'|'busybox', ramMb}, {type:'key', usage, down}, {type:'serial', bytes}
// Out: {type:'status', text}, {type:'uart', text}, {type:'frame', w, h, data}, {type:'stats', ...}
let ex;
const mem8 = () => new Uint8Array(ex.memory.buffer);
const t0 = performance.now();
const wallNs = () => (performance.now() - t0) * 1e6;
const dec = new TextDecoder();
const serialQueue = [];

// ---- SD card image, read lazily with synchronous range requests (allowed in workers)
const CHUNK = 1 << 20, MAX_CHUNKS = 384;
let imageUrl = null, chunks = new Map(), fetched = 0;
function chunk(idx) {
  let c = chunks.get(idx);
  if (c) { chunks.delete(idx); chunks.set(idx, c); return c; }
  const xhr = new XMLHttpRequest();
  xhr.open('GET', imageUrl, false);
  xhr.responseType = 'arraybuffer';
  xhr.setRequestHeader('Range', `bytes=${idx * CHUNK}-${idx * CHUNK + CHUNK - 1}`);
  xhr.send();
  if (xhr.status !== 206 && xhr.status !== 200) throw new Error(`disk read ${xhr.status}`);
  c = new Uint8Array(xhr.response);
  fetched += c.length;
  chunks.set(idx, c);
  if (chunks.size > MAX_CHUNKS) chunks.delete(chunks.keys().next().value);
  return c;
}
function diskRead(lba, count, ptr) {
  let off = lba * 512, left = count * 512, dst = ptr;
  const m = mem8();
  while (left > 0) {
    const idx = Math.floor(off / CHUNK), within = off % CHUNK;
    const c = chunk(idx);
    const n = Math.min(left, CHUNK - within);
    const part = c.subarray(within, within + n);
    m.set(part, dst);
    if (part.length < n) m.fill(0, dst + part.length, dst + n);
    off += n; dst += n; left -= n;
  }
}

async function fetchBytes(url, gz) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: ${r.status}`);
  const body = gz ? r.body.pipeThrough(new DecompressionStream('gzip')) : r.body;
  return new Uint8Array(await new Response(body).arrayBuffer());
}
function copyIn(bytes) { const p = ex.sbc_alloc(bytes.length); mem8().set(bytes, p); return p; }

// ---- JIT: each compiled block is a small module installed in the emulator's function table
let jitFree = [], jitUsed = [];
function jitCompile(ptr, len) {
  try {
    const mod = new WebAssembly.Module(mem8().slice(ptr, ptr + len));
    const t = ex.__indirect_function_table;
    const inst = new WebAssembly.Instance(mod, { env: { memory: ex.memory, table: t } });
    const idx = jitFree.length ? jitFree.pop() : t.grow(1);
    t.set(idx, inst.exports.f);
    jitUsed.push(idx);
    return idx;
  } catch (e) { console.warn('jit', e); return -1; }
}
function jitReset() {
  const t = ex.__indirect_function_table;
  for (const idx of jitUsed) { t.set(idx, null); jitFree.push(idx); }
  jitUsed = [];
}

async function start(os, ramMb, jit) {
  postMessage({ type: 'status', text: 'loading emulator…' });
  const { instance } = await WebAssembly.instantiateStreaming(fetch('wasm/sbc-emu.wasm'), { env: { sbc_disk_read: diskRead, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset } });
  ex = instance.exports;
  if (os === 'raspios' || os === 'raspios-snap') {
    imageUrl = 'assets/raspios-lite.img';
    const head = await fetch(imageUrl, { method: 'HEAD' });
    const size = Number(head.headers.get('Content-Length'));
    if (!head.ok || !size) throw new Error('SD image assets/raspios-lite.img not found (see README: tools/wasm-build.sh)');
    if (os === 'raspios-snap') {
      postMessage({ type: 'status', text: 'loading machine snapshot (Raspberry Pi OS after first-boot setup)…' });
      const snap = await fetchBytes('assets/raspios-lite.snap', false);
      const r = ex.sbc_restore(ramMb, size / 512, copyIn(snap), snap.length);
      if (r !== 0) throw new Error(`snapshot restore failed (${r})`);
      if (jit) ex.sbc_jit(1);
      postMessage({ type: 'status', text: 'resumed' });
      loop();
      return;
    }
    postMessage({ type: 'status', text: `firmware: reading config.txt and kernel8.img from the SD card (${(size / 2 ** 30).toFixed(2)} GiB image)…` });
    const r = ex.sbc_boot_sd(ramMb, size / 512);
    if (r !== 0) throw new Error(`SD boot failed (${r})`);
  } else {
    postMessage({ type: 'status', text: 'loading Raspberry Pi kernel (kernel8.img)…' });
    const kernel = await fetchBytes('assets/kernel8.img', true);
    const initrd = await fetchBytes('assets/initramfs.cpio.gz', true);
    if (ex.sbc_boot(ramMb, copyIn(kernel), kernel.length, copyIn(initrd), initrd.length, 0, 0) !== 0) throw new Error('boot setup failed');
  }
  if (jit) ex.sbc_jit(1);
  postMessage({ type: 'status', text: 'booting…' });
  loop();
}

let lastLed = -1;
let lastFrame = 0, lastStats = 0, lastInsns = 0, lastStatsT = performance.now();
function loop() {
  const sliceEnd = performance.now() + 40;
  let r = 0;
  while (performance.now() < sliceEnd) {
    while (serialQueue.length && ex.sbc_uart_room()) ex.sbc_uart_in(serialQueue.shift());
    r = ex.sbc_run(2_000_000, wallNs());
    if (r !== 0) break;
  }
  // what the guest played on the I²S DAC goes to the page's WebAudio
  const na = ex.sbc_audio_take();
  if (na) {
    const samples = new Int16Array(mem8().buffer.slice(ex.sbc_audio_ptr(), ex.sbc_audio_ptr() + 2 * na));
    postMessage({ type: 'audio', rate: ex.sbc_audio_rate(), samples }, [samples.buffer]);
  }
  const n = ex.sbc_uart_take();
  if (n) postMessage({ type: 'uart', text: dec.decode(mem8().slice(ex.sbc_uart_ptr(), ex.sbc_uart_ptr() + n)) });
  const now = performance.now();
  if (now - lastFrame > 66) {
    lastFrame = now;
    const p = ex.sbc_fb_render();
    if (p) {
      const w = ex.sbc_fb_w(), h = ex.sbc_fb_h();
      const data = mem8().slice(p, p + w * h * 4);
      postMessage({ type: 'frame', w, h, data }, [data.buffer]);
    }
  }
  // the LED on GPIO17 (sampled every slice; brief pulses shorter than a slice may be missed)
  const led = ex.sbc_gpio_level(17);
  if (led !== lastLed) { lastLed = led; postMessage({ type: 'gpio', pin: 17, level: led }); }
  if (now - lastStats > 500) {
    const insns = ex.sbc_insns();
    postMessage({ type: 'stats', mips: (insns - lastInsns) / ((now - lastStatsT) * 1e3), insns, simS: ex.sbc_now_ns() / 1e9, diskMiB: fetched / 2 ** 20, jit: ex.sbc_jit_blocks() });
    lastInsns = insns; lastStatsT = now; lastStats = now;
  }
  if (r === 2) {
    const kind = ex.sbc_stop_kind();
    const pc = ex.sbc_stop_pc().toString(16), insn = (ex.sbc_stop_insn() >>> 0).toString(16).padStart(8, '0');
    postMessage({ type: 'status', text: kind === 1 ? `stopped: unimplemented instruction ${insn} at pc 0x${pc}` : `stopped (powered off) at pc 0x${pc}` });
    return;
  }
  if (r === 1) {
    const wait = Math.max(0, ex.sbc_wait_ms());
    setTimeout(() => { ex.sbc_wake(wallNs()); loop(); }, serialQueue.length ? 0 : wait);
  } else {
    setTimeout(loop, 0);
  }
}

onmessage = (e) => {
  const m = e.data;
  if (m.type === 'start') start(m.os, m.ramMb, m.jit).catch((err) => postMessage({ type: 'status', text: String(err) }));
  else if (m.type === 'key' && ex) ex.sbc_key(m.usage, m.down ? 1 : 0);
  else if (m.type === 'pointer' && ex) ex.sbc_pointer(m.buttons, m.x, m.y, m.wheel);
  else if (m.type === 'serial') serialQueue.push(...m.bytes);
  else if (m.type === 'button' && ex) ex.sbc_gpio_input(27, m.pressed ? 1 : -1);
  else if (m.type === 'sensor' && ex) ex.sbc_set_sensor(m.kind, m.value);
  else if (m.type === 'adc' && ex) ex.sbc_set_adc(m.ch, m.volts);
};
