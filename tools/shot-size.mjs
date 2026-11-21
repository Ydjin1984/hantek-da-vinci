// Dev helper: open the UI in Chrome at a known window size and save a PNG.
//   node tools/shot-size.mjs http://127.0.0.1:7411/ docs/x.png 1440 1000
//
// `tools/shot.mjs` keeps the browser's default viewport, which is too small to
// judge a panel that has to fit eight channel cards. This one sets the metrics
// first and waits for the first frame to arrive, so the picture is the real
// interface and not a half-built shell.
const url = process.argv[2] ?? "http://127.0.0.1:7411/";
const out = process.argv[3] ?? "docs/shot.png";
const width = Number(process.argv[4] ?? 1440);
const height = Number(process.argv[5] ?? 1000);
const port = process.argv[6] ?? "9222";
const fs = await import("node:fs");

const created = await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(url)}`, { method: "PUT" });
const target = await created.json();
const ws = new WebSocket(target.webSocketDebuggerUrl);
let id = 0;
const waiting = new Map();
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  const r = waiting.get(m.id);
  if (r) {
    waiting.delete(m.id);
    r(m);
  }
};
await new Promise((res) => (ws.onopen = res));
const send = (method, params = {}) => {
  const myId = ++id;
  return new Promise((resolve) => {
    waiting.set(myId, resolve);
    ws.send(JSON.stringify({ id: myId, method, params }));
  });
};

await send("Page.enable");
await send("Emulation.setDeviceMetricsOverride", {
  width,
  height,
  deviceScaleFactor: 1,
  mobile: false,
});
await send("Page.navigate", { url });
const waitMs = Number(process.env.SHOT_WAIT_MS ?? 5000);
await new Promise((r) => setTimeout(r, waitMs));
const shot = await send("Page.captureScreenshot", { format: "png" });
fs.writeFileSync(out, Buffer.from(shot.result.data, "base64"));
console.log(`saved ${out} (${width}x${height})`);
await fetch(`http://127.0.0.1:${port}/json/close/${target.id}`);
process.exit(0);
