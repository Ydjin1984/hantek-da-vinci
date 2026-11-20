// Dev helper: evaluate an expression in the running page over CDP.
// Usage: node probe.mjs "expression" [port]
const expr = process.argv[2] ?? "1+1";
const port = process.argv[3] ?? "9222";

const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
const page = list.find((t) => t.type === "page");
if (!page) {
  console.error("no page target");
  process.exit(1);
}
const ws = new WebSocket(page.webSocketDebuggerUrl);
const done = new Promise((resolve) => {
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id === 1) resolve(msg);
  };
});
ws.onopen = () => {
  ws.send(JSON.stringify({ id: 1, method: "Runtime.evaluate", params: { expression: expr, returnByValue: true, awaitPromise: true } }));
};
const msg = await done;
console.log(JSON.stringify(msg.result?.result?.value ?? msg.result, null, 2));
process.exit(0);
