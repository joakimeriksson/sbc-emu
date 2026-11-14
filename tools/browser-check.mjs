// Drive headless Chrome over the DevTools protocol: open a page, echo its (and its workers')
// console, and save screenshots at given times.  node tools/browser-check.mjs URL out-prefix secs...
import { spawn } from 'node:child_process';
import fs from 'node:fs';
const [url, out, ...times] = process.argv.slice(2);
const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new', '--remote-debugging-port=9333', '--window-size=1440,1150', '--hide-scrollbars', '--user-data-dir=/tmp/sbc-chrome', 'about:blank'], { stdio: 'ignore' });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let tgt;
for (let i = 0; i < 50 && !tgt; i++) { await sleep(200); try { tgt = (await (await fetch('http://127.0.0.1:9333/json')).json()).find((t) => t.type === 'page'); } catch {} }
const ws = new WebSocket(tgt.webSocketDebuggerUrl);
await new Promise((r) => ws.onopen = r);
let id = 0; const pending = new Map();
const send = (method, params = {}, sessionId) => new Promise((r) => { const i = ++id; pending.set(i, r); ws.send(JSON.stringify({ id: i, method, params, sessionId })); });
const t0 = Date.now();
ws.onmessage = async (e) => {
  const m = JSON.parse(e.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m.result ?? m.error); pending.delete(m.id); return; }
  if (m.method === 'Runtime.consoleAPICalled') console.log(`${((Date.now() - t0) / 1000).toFixed(1)}s ${m.sessionId ? 'worker' : 'page'} ${m.params.type}:`, m.params.args.map((a) => a.value ?? a.description).join(' '));
  if (m.method === 'Runtime.exceptionThrown') console.log(`${((Date.now() - t0) / 1000).toFixed(1)}s exception:`, m.params.exceptionDetails.exception?.description ?? m.params.exceptionDetails.text);
  if (m.method === 'Target.attachedToTarget') { const s = m.params.sessionId; await send('Runtime.enable', {}, s); await send('Runtime.runIfWaitingForDebugger', {}, s); }
};
await send('Runtime.enable');
await send('Page.enable');
await send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true });
await send('Page.navigate', { url });
let last = 0;
for (const t of times.map(Number)) {
  await sleep((t - last) * 1000); last = t;
  const st = await send('Runtime.evaluate', { expression: `[...document.querySelectorAll('.readout span')].map(s=>s.textContent).join(' | ')`, returnByValue: true });
  console.log(`${t}s readout: ${st.result?.value}`);
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(`${out}-${t}.png`, Buffer.from(shot.data, 'base64'));
}
chrome.kill();
process.exit(0);
