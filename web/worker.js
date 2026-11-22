// Runs the emulator off the main thread.
// In:  {type:'start', os:'raspios'|'raspios-snap'|'gpu-demo'|'busybox'|'hello'|'tinker-armbian'|'tinker-busybox'|'orangepi-one'|'orangepi-rv'|'orangepi5'|'jetson-nano'|'esp32s31', ramMb},
//      {type:'start', os:'sd-file', file, ramMb} (a Raspberry Pi SD image the visitor chose: .img, .img.xz, .img.gz),
//      {type:'key', usage, down},
//      {type:'serial', bytes}, {type:'type', text} (typed on the USB keyboard)
// Out: {type:'status', text}, {type:'uart', text}, {type:'frame', w, h, data}, {type:'stats', ...}
let ex;
const mem8 = () => new Uint8Array(ex.memory.buffer);
const t0 = performance.now();
const wallNs = () => (performance.now() - t0) * 1e6;
const dec = new TextDecoder();
const serialQueue = [];

// ---- SD card image, read lazily with synchronous range requests (allowed in workers)
const CHUNK = 1 << 20, MAX_CHUNKS = 384;
let imageUrl = null, chunks = new Map(), fetched = 0, noCardWarned = false;
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
// Pointers into the emulator's memory arrive as i32: `>>> 0` keeps those above 2 GiB positive.
function diskRead(lba, count, ptr) {
  if (card) { card.read(lba * 512, count * 512, ptr >>> 0); return; }
  let off = lba * 512, left = count * 512, dst = ptr >>> 0;
  const m = mem8();
  if (!imageUrl) {
    // a snapshot published without its SD card (the 3D demos never read it): reads see zeros
    m.fill(0, dst, dst + left);
    if (!noCardWarned) { noCardWarned = true; postMessage({ type: 'status', text: 'the guest read its SD card, which this page does not have: programs other than the demos may fail', phase: 'nocard' }); }
    return;
  }
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
// ---- an SD card image the visitor chose (os 'sd-file'). Nothing is uploaded: the worker reads
// the file where it is, synchronously (FileReaderSync), as the guest's SD reads ask for it.
//  - .img: read directly.
//  - .img.xz whose blocks are all one small size (Ubuntu's: 1 MiB): the index at the end of the
//    file maps the card to blocks, and each block is decoded when first read (sbc_xz_block).
//  - other .xz (Raspberry Pi OS's blocks are 184 MiB) and .img.gz: unpacked once into the
//    browser's private file storage (OPFS), which a later visit with the same file reuses.
// Writes stay in the emulator's memory (the card's overlay): the file is never changed.
let card = null;
const fileReader = () => (fileReader.r ??= new FileReaderSync());
const fileBytes = (file, off, len) => new Uint8Array(fileReader().readAsArrayBuffer(file.slice(off, off + len)));

// a card read from fixed-size units (chunks of a file, xz blocks), the recent ones kept
function unitCard(unit, getUnit, keepBytes = 384 * 2 ** 20) {
  const cache = new Map(), keep = Math.max(4, Math.floor(keepBytes / unit));
  let failed = false;
  const get = (i) => {
    let c = cache.get(i);
    if (c) { cache.delete(i); cache.set(i, c); return c; }
    c = getUnit(i);
    cache.set(i, c);
    if (cache.size > keep) cache.delete(cache.keys().next().value);
    return c;
  };
  return { read(off, len, ptr) {
    while (len > 0) {
      const i = Math.floor(off / unit), within = off - i * unit, n = Math.min(len, unit - within);
      let c;
      try { c = get(i); } catch (e) {
        c = new Uint8Array(0);
        if (!failed) { failed = true; postMessage({ type: 'status', text: `reading the card failed: ${e.message}`, phase: 'error' }); }
      }
      const part = c.subarray(within, within + n), m = mem8(); // after get(): decoding may grow memory
      m.set(part, ptr);
      if (part.length < n) m.fill(0, ptr + part.length, ptr + n);
      off += n; ptr += n; len -= n;
    }
  } };
}

// the blocks of an .xz file, from the index of each stream (read backwards from the end)
function xzBlocks(file) {
  const blocks = [];
  let end = file.size;
  while (end > 0) {
    for (;;) { // stream padding: zero words
      const w = fileBytes(file, end - 4, 4);
      if (w[0] | w[1] | w[2] | w[3]) break;
      end -= 4;
    }
    const f = fileBytes(file, end - 12, 12);
    if (f[10] !== 0x59 || f[11] !== 0x5a) throw new Error('not an .xz file (no stream footer)');
    const isize = (((f[4] | f[5] << 8 | f[6] << 16 | f[7] << 24) >>> 0) + 1) * 4;
    const istart = end - 12 - isize, idx = fileBytes(file, istart, isize);
    let p = 1;
    const v = () => { let x = 0, s = 1, c; do { c = idx[p++]; x += (c & 0x7f) * s; s *= 128; } while (c & 0x80); return x; };
    if (idx[0] !== 0) throw new Error('corrupt .xz index');
    const n = v(), recs = [];
    for (let i = 0; i < n; i++) recs.push([v(), v()]);
    const start = istart - recs.reduce((a, [u]) => a + Math.ceil(u / 4) * 4, 0) - 12;
    const h = fileBytes(file, start, 6);
    if (h[0] !== 0xfd || h[1] !== 0x37 || h[2] !== 0x7a) throw new Error('corrupt .xz file');
    let off = start + 12;
    const sb = recs.map(([u, un]) => { const b = { coff: off, csize: u, usize: un }; off += Math.ceil(u / 4) * 4; return b; });
    blocks.unshift(...sb);
    end = start;
  }
  let uoff = 0;
  for (const b of blocks) { b.uoff = uoff; uoff += b.usize; }
  return { blocks, size: uoff };
}
function xzDecode(file, b) {
  const p = ex.sbc_xz_in(b.csize) >>> 0;
  mem8().set(fileBytes(file, b.coff, b.csize), p);
  fetched += b.csize;
  if (ex.sbc_xz_block(b.usize) < 0) throw new Error('an .xz block this page cannot decode (only LZMA2, without BCJ filters)');
  const q = ex.sbc_xz_out_ptr() >>> 0;
  return mem8().slice(q, q + b.usize);
}

// unpack into the browser's file storage, once per file; returns a card over it and its size
async function unpackCard(file, kind, xz) {
  const root = await navigator.storage.getDirectory();
  const name = 'sd-' + `${file.name}-${file.size}-${file.lastModified}`.replace(/[^A-Za-z0-9._-]/g, '_') + '.img';
  const names = [];
  for await (const [n] of root.entries()) names.push(n);
  const reuse = names.includes(name + '.done');
  for (const n of names) if (n.startsWith('sd-') && n !== name && n !== name + '.done') await root.removeEntry(n); // one card at a time
  const h = await (await root.getFileHandle(name, { create: true })).createSyncAccessHandle();
  if (!reuse) {
    const need = xz ? xz.size : file.size * 4;
    const est = await navigator.storage.estimate();
    if (est.quota && est.quota - est.usage < need) { h.close(); await root.removeEntry(name); throw new Error(`the browser allows this page ${((est.quota - est.usage) / 2 ** 30).toFixed(1)} GiB of storage; unpacking needs about ${(need / 2 ** 30).toFixed(1)} GiB`); }
    h.truncate(0);
    const t = performance.now();
    const progress = (done, total) => postMessage({ type: 'status', text: `unpacking ${file.name} into the browser's storage (once): ${(done / 2 ** 30).toFixed(2)} of ${(total / 2 ** 30).toFixed(2)} GiB, ${(done / 2 ** 20 / ((performance.now() - t) / 1000)).toFixed(0)} MiB/s` });
    if (kind === 'xz') {
      for (const b of xz.blocks) {
        h.write(xzDecode(file, b), { at: b.uoff });
        progress(b.uoff + b.usize, xz.size);
      }
    } else {
      let read = 0, at = 0, last = 0;
      const count = new TransformStream({ transform(c, ctl) { read += c.length; ctl.enqueue(c); } });
      const rd = file.stream().pipeThrough(count).pipeThrough(new DecompressionStream('gzip')).getReader();
      for (;;) {
        const { done, value } = await rd.read();
        if (done) break;
        h.write(value, { at }); at += value.length;
        if (performance.now() - last > 250) { last = performance.now(); progress(read, file.size); }
      }
    }
    h.flush();
    await root.getFileHandle(name + '.done', { create: true });
  }
  const size = h.getSize();
  return { size, card: unitCard(CHUNK, (i) => { const b = new Uint8Array(Math.min(CHUNK, size - i * CHUNK)); h.read(b, { at: i * CHUNK }); return b; }) };
}

async function openCard(file) {
  const head = fileBytes(file, 0, 6);
  if (head[0] === 0xfd && head[1] === 0x37 && head[2] === 0x7a && head[3] === 0x58 && head[4] === 0x5a) {
    postMessage({ type: 'status', text: `reading the index of ${file.name}…` });
    const xz = xzBlocks(file), u = xz.blocks[0]?.usize ?? 0;
    if (u && u <= 16 * 2 ** 20 && xz.blocks.slice(0, -1).every((b) => b.usize === u))
      return { size: xz.size, how: `read from the .xz, ${xz.blocks.length} blocks of ${u / 2 ** 20} MiB, each unpacked when first read`, card: unitCard(u, (i) => xzDecode(file, xz.blocks[i])) };
    return { ...(await unpackCard(file, 'xz', xz)), how: 'unpacked in the browser’s storage' };
  }
  if (head[0] === 0x1f && head[1] === 0x8b) return { ...(await unpackCard(file, 'gz')), how: 'unpacked in the browser’s storage' };
  return { size: file.size, how: 'read from the file', card: unitCard(CHUNK, (i) => { const b = fileBytes(file, i * CHUNK, CHUNK); fetched += b.length; return b; }) };
}

// a large file, either whole or (on GitHub Pages, which takes files up to 100 MB) in the parts a
// JSON manifest lists; reports the download's progress
async function fetchParts(url, label) {
  let parts = [url], total = 0, extra = {};
  const man = await fetch(url + '.json');
  if (man.ok) { extra = await man.json(); parts = extra.parts.map((p) => new URL(p, new URL(url, location.href)).href); total = extra.bytes; }
  const bufs = []; let got = 0;
  for (const p of parts) {
    const r = await fetch(p);
    if (!r.ok) throw new Error(`${p}: ${r.status}`);
    total ||= Number(r.headers.get('Content-Length')) || 0;
    const rd = r.body.getReader();
    for (;;) {
      const { done, value } = await rd.read();
      if (done) break;
      bufs.push(value); got += value.length;
      if (total) postMessage({ type: 'status', text: `${label}: ${(got / 1e6).toFixed(0)} of ${(total / 1e6).toFixed(0)} MB`, phase: 'download', got, total });
    }
  }
  const out = new Uint8Array(got); let o = 0;
  for (const b of bufs) { out.set(b, o); o += b.length; }
  return { bytes: out, extra };
}
function copyIn(bytes) { const p = ex.sbc_alloc(bytes.length) >>> 0; mem8().set(bytes, p); return p; }

// ---- JIT: each compiled block is a small module installed in the emulator's function table
let jitFree = [], jitUsed = [];
function jitCompile(ptr, len) {
  ptr >>>= 0;
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

async function start(os, ramMb, jit, file) {
  postMessage({ type: 'status', text: 'loading emulator…' });
  const { instance } = await WebAssembly.instantiateStreaming(fetch('wasm/sbc-emu.wasm'), { env: { sbc_disk_read: diskRead, sbc_jit_compile: jitCompile, sbc_jit_reset: jitReset, sbc_host_now_ms: () => performance.now() } });
  ex = instance.exports;
  if (os === 'orangepi-one') {
    // Orange Pi One (Allwinner H3, four 32-bit Cortex-A7s): Alpine's kernel and device tree and
    // a BusyBox initramfs (scripts/orangepi-one-fixture.sh), shell on the serial console
    postMessage({ type: 'status', text: 'loading the Alpine kernel, device tree and initramfs…' });
    const [kernel, dtb, initrd] = await Promise.all(['zImage', 'sun8i-h3-orangepi-one.dtb', 'initramfs.cpio.gz']
      .map((f) => fetchBytes(`assets/orangepi-one/${f}`, false)));
    if (ex.sbc_boot_opi1(ramMb, copyIn(kernel), kernel.length, copyIn(dtb), dtb.length, copyIn(initrd), initrd.length, 0, 0) !== 0) throw new Error('boot setup failed');
    if (jit) ex.sbc_jit(1);
    frameEvery = 100;
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
  if (os === 'orangepi-rv') {
    // Orange Pi RV (StarFive JH7110, four 64-bit RISC-V U74s): OpenSBI and Alpine's mainline
    // kernel with the stock device tree and a BusyBox initramfs (scripts/orangepi-rv-fixture.sh:
    // web/initramfs.cpio.gz), shell on the serial console; the kernel arrives gzip'd
    postMessage({ type: 'status', text: 'loading OpenSBI, the Alpine kernel, device tree and initramfs…' });
    const [fw, kernel, dtb, initrd] = await Promise.all([['fw_dynamic.bin', false], ['Image.gz', true], ['jh7110-orangepi-rv.dtb', false], ['initramfs.cpio.gz', false]]
      .map(([f, gz]) => fetchBytes(`assets/orangepi-rv/${f}`, gz)));
    if (ex.sbc_boot_opirv(ramMb, copyIn(fw), fw.length, copyIn(kernel), kernel.length, copyIn(dtb), dtb.length, copyIn(initrd), initrd.length, 0, 0) !== 0) throw new Error('boot setup failed');
    if (jit) ex.sbc_jit(1);
    frameEvery = 100;
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
  if (os === 'orangepi5') {
    // Orange Pi 5 (Rockchip RK3588S, four Cortex-A55s and four Cortex-A76s; the first four
    // here): Debian's arm64 kernel with the emulator's device tree and an Alpine/BusyBox
    // initramfs (scripts/orangepi5-fixture.sh), shell on the serial console; the kernel
    // arrives gzip'd
    postMessage({ type: 'status', text: 'loading the Debian kernel and initramfs…' });
    const [kernel, initrd] = await Promise.all([['Image.gz', true], ['initramfs.cpio.gz', false]]
      .map(([f, gz]) => fetchBytes(`assets/orangepi5/${f}`, gz)));
    if (ex.sbc_boot_opi5(ramMb, copyIn(kernel), kernel.length, copyIn(initrd), initrd.length, 0, 0) !== 0) throw new Error('boot setup failed');
    if (jit) ex.sbc_jit(1);
    frameEvery = 100;
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
  if (os === 'jetson-nano') {
    // NVIDIA Jetson Nano (Tegra X1, four Cortex-A57s): Debian's arm64 kernel (gzip'd) with the board's
    // own device tree and an Alpine/BusyBox initramfs with the board's drivers
    // (scripts/jetson-nano-debian-fixture.sh); shells on the serial console and on HDMI, a 1280 x 720
    // monitor, with the USB keyboard
    postMessage({ type: 'status', text: 'loading the Debian kernel, device tree and initramfs…' });
    const [kernel, dtb, initrd] = await Promise.all([['Image.gz', true], ['tegra210-p3450-0000.dtb', false], ['initramfs.cpio.gz', false]]
      .map(([f, gz]) => fetchBytes(`assets/jetson-nano/${f}`, gz)));
    ex.sbc_kms(1280, 720);
    const cmd = new TextEncoder().encode('console=tty0 console=ttyS0,115200 rdinit=/init');
    if (ex.sbc_boot_nano(ramMb, copyIn(kernel), kernel.length, copyIn(dtb), dtb.length, copyIn(initrd), initrd.length, copyIn(cmd), cmd.length) !== 0) throw new Error('boot setup failed');
    if (jit) ex.sbc_jit(1);
    frameEvery = 66;
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
  if (os === 'esp32s31') {
    // ESP32-S31-Function-CoreBoard-1 (two 32-bit RISC-V harts, 16 MiB PSRAM): Espressif's mask
    // ROM and its official Linux flash image (scripts/esp32s31-fixture.sh), booting through the
    // ROM, U-Boot SPL, OpenSBI and U-Boot to Linux; shell on UART0. The image arrives gzip'd
    postMessage({ type: 'status', text: 'loading Espressif’s ROM and the Linux flash image…' });
    const [rom, flash] = await Promise.all([['esp32s31_rev0_rom.elf', false], ['s31_full_flash.bin.gz', true]]
      .map(([f, gz]) => fetchBytes(`assets/esp32s31/${f}`, gz)));
    if (ex.sbc_boot_s31(copyIn(rom), rom.length, copyIn(flash), flash.length) !== 0) throw new Error('boot setup failed');
    ex.sbc_jit(jit ? 1 : 0);
    frameEvery = 100;
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
  if (os === 'tinker-armbian' || os === 'tinker-busybox') {
    // ASUS Tinker Board S (RK3288, four 32-bit Cortex-A12s): no USB yet, so input is the serial
    // console; HDMI shows what the guest's DRM driver puts on the emulated 720p monitor
    ex.sbc_kms(1280, 720);
    if (os === 'tinker-armbian') {
      imageUrl = 'assets/tinker-armbian.img';
      const head = await fetch(imageUrl, { method: 'HEAD' });
      const size = Number(head.headers.get('Content-Length'));
      if (!head.ok || !size) throw new Error('SD image assets/tinker-armbian.img not found (see docs/rk3288-tinker-board-plan.md: the browser)');
      postMessage({ type: 'status', text: `U-Boot: reading armbianEnv.txt, zImage and the device tree from the SD card (${(size / 2 ** 30).toFixed(2)} GiB image)…` });
      if (ex.sbc_boot_tinker_sd(ramMb, Date.now() / 1000, size / 512) !== 0) throw new Error('SD boot failed (no Armbian /boot on the card?)');
    } else {
      postMessage({ type: 'status', text: 'loading the Debian kernel, device tree and BusyBox initramfs…' });
      const kernel = await fetchBytes('assets/tinker/zImage', false);
      const dtb = await fetchBytes('assets/tinker/rk3288-tinker-s.dtb', false);
      const initrd = await fetchBytes('assets/tinker/initramfs.cpio.gz', false);
      if (ex.sbc_boot_tinker(ramMb, Date.now() / 1000, copyIn(kernel), kernel.length, copyIn(dtb), dtb.length, copyIn(initrd), initrd.length, 0, 0) !== 0) throw new Error('boot setup failed');
    }
    if (jit) ex.sbc_jit(1);
    frameEvery = 100;
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
  if (os === 'gpu-demo') {
    // Raspberry Pi OS with Mesa, logged in on HDMI, every demo run once (scripts/gpu-demo-snapshot.txt)
    // the SD card if it is served next to the page (tools/wasm-build.sh); the public site has only
    // the snapshot, whose demos run from the guest's page cache (tools/site-build.sh)
    const head = await fetch('assets/raspios-gpu.img', { method: 'HEAD' });
    let size = Number(head.headers.get('Content-Length'));
    if (head.ok && size) imageUrl = 'assets/raspios-gpu.img';
    postMessage({ type: 'status', text: 'loading the machine snapshot…', phase: 'snapshot' });
    const { bytes: snap, extra } = await fetchParts('assets/gpu-demo.snap', 'downloading the snapshot');
    if (!imageUrl) {
      if (!extra.sectors) throw new Error('no SD image (assets/raspios-gpu.img) and no snapshot manifest (assets/gpu-demo.snap.json): see docs/gpu.md');
      size = extra.sectors * 512;
    }
    postMessage({ type: 'status', text: 'restoring the Pi…', phase: 'restore' });
    const r = ex.sbc_restore(ramMb, size / 512, copyIn(snap), snap.length);
    if (r !== 0) throw new Error(`snapshot restore failed (${r})`);
    if (jit) ex.sbc_jit(1);
    frameEvery = 33;
    postMessage({ type: 'status', text: 'running', phase: 'running' });
    loop();
    return;
  }
  if (os === 'hello') {
    // the Pi kernel and the busybox initramfs, booted to the shell (scripts/web-hello-snapshot.txt):
    // no SD card, so the page works from any static web server
    postMessage({ type: 'status', text: 'loading the machine snapshot…' });
    const snap = await fetchBytes('assets/hello.snap', false);
    const r = ex.sbc_restore(ramMb, 0, copyIn(snap), snap.length);
    if (r !== 0) throw new Error(`snapshot restore failed (${r})`);
    if (jit) ex.sbc_jit(1);
    postMessage({ type: 'status', text: 'running' });
    loop();
    return;
  }
  if (os === 'sd-file') {
    const c = await openCard(file);
    card = c.card;
    postMessage({ type: 'status', text: `firmware: reading config.txt and the kernel from ${file.name} (${(c.size / 2 ** 30).toFixed(2)} GiB card, ${c.how})…` });
    const r = ex.sbc_boot_sd(ramMb, c.size / 512);
    if (r === -2) throw new Error(`${file.name} has no Raspberry Pi boot partition (config.txt and a kernel on a FAT partition): is it an image for the Pi 4?`);
    if (r !== 0) throw new Error(`the kernel on ${file.name} did not start (${r})`);
    if (jit) ex.sbc_jit(1);
    postMessage({ type: 'status', text: 'booting…' });
    loop();
    return;
  }
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
let lastFrame = 0, lastStats = 0, lastInsns = 0, lastStatsT = performance.now(), frameEvery = 66, lastGpu = 0;
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
    const samples = new Int16Array(mem8().buffer.slice(ex.sbc_audio_ptr() >>> 0, (ex.sbc_audio_ptr() >>> 0) + 2 * na));
    postMessage({ type: 'audio', rate: ex.sbc_audio_rate(), samples }, [samples.buffer]);
  }
  const n = ex.sbc_uart_take();
  if (n) postMessage({ type: 'uart', text: dec.decode(mem8().slice(ex.sbc_uart_ptr() >>> 0, (ex.sbc_uart_ptr() >>> 0) + n)) });
  const now = performance.now();
  if (now - lastFrame > frameEvery) {
    lastFrame = now;
    const p = ex.sbc_fb_render() >>> 0;
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
    const gpu = ex.sbc_gpu_frames();
    postMessage({ type: 'stats', mips: (insns - lastInsns) / ((now - lastStatsT) * 1e3), insns, simS: ex.sbc_now_ns() / 1e9, diskMiB: fetched / 2 ** 20, jit: ex.sbc_jit_blocks(),
                  gpuFps: (gpu - lastGpu) / ((now - lastStatsT) / 1e3), gpuFrames: gpu, gpuMs: ex.sbc_gpu_ms() });
    lastInsns = insns; lastStatsT = now; lastStats = now; lastGpu = gpu;
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
  if (m.type === 'start') start(m.os, m.ramMb, m.jit, m.file).catch((err) => postMessage({ type: 'status', text: String(err), phase: 'error' }));
  else if (m.type === 'key' && ex) ex.sbc_key(m.usage, m.down ? 1 : 0);
  else if (m.type === 'pointer' && ex) ex.sbc_pointer(m.buttons, m.x, m.y, m.wheel);
  else if (m.type === 'serial') serialQueue.push(...m.bytes);
  else if (m.type === 'type' && ex) for (const c of new TextEncoder().encode(m.text)) ex.sbc_type(c);
  else if (m.type === 'button' && ex) ex.sbc_gpio_input(27, m.pressed ? 1 : -1);
  else if (m.type === 'sensor' && ex) ex.sbc_set_sensor(m.kind, m.value);
  else if (m.type === 'adc' && ex) ex.sbc_set_adc(m.ch, m.volts);
};
