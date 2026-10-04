// Runs containers/test-stall.html in headless Chrome against the relay on
// localhost:8080, prints one summary line per phase and writes the full JSON
// report (plus the snapshot, if requested). Usage (Windows, Node 22+):
//   node containers/test-stall.js [query-string] [report.json]
// e.g. "machine=default&movePattern=burst&phases=idle:10,mouse:45,idle:10"
// with containers/test-stall-source.sh providing the video, and
// containers/test-stall-relay.sh for the relay's drop/recovery counts.
const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

const query = process.argv[2] || '';
const reportPath = process.argv[3];
const page = 'file:///' + path.resolve(__dirname, 'test-stall.html').replace(/\\/g, '/') + (query ? '?' + query : '');
const chromePath = process.env.CHROME || 'C:/Program Files/Google/Chrome/Application/chrome.exe';
const port = 9334;
const chrome = spawn(chromePath, [
  '--headless=new', '--disable-gpu', '--autoplay-policy=no-user-gesture-required',
  '--disable-background-timer-throttling', '--disable-renderer-backgrounding',
  `--remote-debugging-port=${port}`, `--user-data-dir=${path.join(require('os').tmpdir(), 'flux-stall-profile')}`,
  '--no-first-run', page,
], { stdio: 'ignore' });
const sleep = ms => new Promise(r => setTimeout(r, ms));

(async () => {
  await sleep(2000);
  const tabs = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  const ws = new WebSocket(tabs.find(t => t.type === 'page').webSocketDebuggerUrl);
  await new Promise(r => (ws.onopen = r));
  let id = 0;
  const evaluate = expression => new Promise(resolve => {
    const callId = ++id;
    const handler = m => {
      const d = JSON.parse(m.data);
      if (d.id === callId) { ws.removeEventListener('message', handler); resolve(d.result.result.value); }
    };
    ws.addEventListener('message', handler);
    ws.send(JSON.stringify({ id: callId, method: 'Runtime.evaluate', params: { expression } }));
  });
  const deadline = Date.now() + 10 * 60 * 1000;
  while (Date.now() < deadline && (await evaluate('document.title')) !== 'done') await sleep(1000);
  const report = JSON.parse(await evaluate('document.getElementById("out").textContent'));
  if (report.snapshot && reportPath) {
    fs.writeFileSync(reportPath.replace(/\.json$/, '') + '-snapshot.png', Buffer.from(report.snapshot.split(',')[1], 'base64'));
  }
  delete report.snapshot;
  if (reportPath) fs.writeFileSync(reportPath, JSON.stringify(report, null, 1));
  const d = x => (x && x.n ? `${x.p50}/${x.p95}/${x.p99}/${x.max}` : '-');
  for (const p of report.phases) {
    const s = p.stats || {};
    console.log([
      p.phase.padEnd(5), `moves/s=${p.moveRate}`, `rx fps=${(s.framesReceived / p.secs).toFixed(1)}`,
      `content fps=${p.contentFps}`, `content ms p50/p95/p99/max=${d(p.contentMs)}`,
      `gaps>100ms=${p.contentGapsOver100ms} >250ms=${p.contentGapsOver250ms} frozen=${p.frozenMsTotal}ms`,
      `recv ms=${d(p.recvDeltaMs)}`, `rtp ms=${d(p.rtpDeltaMs)}`, `jb=${p.jitterBufferMsPerFrame}ms`,
      `kf=${s.keyFramesDecoded} pli=${s.pliCount} nack=${s.nackCount} lost=${s.packetsLost}`,
      `freezes=${s.freezeCount}/${s.totalFreezesDuration}s dropped=${s.framesDropped}`,
      `cursor msgs=${p.cursorMsgs} (${p.cursorPerSec}/s) bitmaps=${p.cursorBitmaps} last=${JSON.stringify(p.lastCursor)}`,
      `video=${p.videoSize} resStatus=${(p.resolutionStatuses || []).map(r => r.state + ":" + r.width + "x" + r.height).join(",") || "-"}`,
    ].join(' | '));
  }
  console.log(report.events.filter(e => !e.startsWith('error={"error":"No active')).join('\n'));
  chrome.kill();
  process.exit(0);
})().catch(e => { console.error(e); chrome.kill(); process.exit(1); });
