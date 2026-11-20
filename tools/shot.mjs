// Dev helper: open the UI in Chrome, let a few frames land, save a screenshot.
//   node tools/shot.mjs http://127.0.0.1:7411/ docs/live3.png
const port = process.argv[4] ?? "9222";
const url = process.argv[2] ?? "http://127.0.0.1:7411/";
const out = process.argv[3] ?? "docs/shot.png";
const fs = await import("node:fs");

const created = await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(url)}`, { method: "PUT" });
const target = await created.json();
const ws = new WebSocket(target.webSocketDebuggerUrl);
let id = 0;
const waiting = new Map();
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  const r = waiting.get(m.id);
  if (r) { waiting.delete(m.id); r(m); }
};
await new Promise((res) => (ws.onopen = res));
const send = (method, params = {}) => {
  const myId = ++id;
  return new Promise((resolve) => { waiting.set(myId, resolve); ws.send(JSON.stringify({ id: myId, method, params })); });
};

await send("Page.enable");
await send("Page.navigate", { url });
await new Promise((r) => setTimeout(r, 4000));
const shot = await send("Page.captureScreenshot", { format: "png" });
fs.writeFileSync(out, Buffer.from(shot.result.data, "base64"));
console.log(`saved ${out}`);
await fetch(`http://127.0.0.1:${port}/json/close/${target.id}`);
process.exit(0);
