// Regression probe: the voltage <select> must survive the 400 ms poller.
//
// A native dropdown popup lives inside the element that owns it, so replacing
// that element (innerHTML) while the list is open closes the list. The test
// stamps the node, keeps it focused for several polls, and checks the stamp is
// still there afterwards.
const port = process.argv[2] ?? "9222";
const url = process.argv[3] ?? "http://127.0.0.1:7413/";

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

function send(method, params = {}) {
  const myId = ++id;
  return new Promise((resolve) => {
    waiting.set(myId, resolve);
    ws.send(JSON.stringify({ id: myId, method, params }));
  });
}

async function evalJs(expression) {
  const m = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
  if (m.result?.exceptionDetails) throw new Error(JSON.stringify(m.result.exceptionDetails));
  return m.result?.result?.value;
}

await send("Page.enable");
await send("Page.navigate", { url });

const ready = await evalJs(`
  new Promise((resolve) => {
    const t0 = Date.now();
    const poll = () => {
      if (document.querySelector(".card select.range")) return resolve("ok");
      if (Date.now() - t0 > 8000) return resolve("timeout");
      setTimeout(poll, 100);
    };
    poll();
  })
`);
if (ready !== "ok") {
  console.log(JSON.stringify({ ok: false, reason: "cards never appeared" }, null, 2));
  process.exit(1);
}

// Stamp the node, focus it (an open popup keeps focus on its select).
const stamped = await evalJs(`
  (() => {
    const s = document.querySelector(".card select.range");
    s.__probe = 42;
    s.focus();
    return { options: s.options.length, value: s.value };
  })()
`);

// Four polls of headroom.
await new Promise((r) => setTimeout(r, 1800));

const survived = await evalJs(`
  (() => {
    const s = document.querySelector(".card select.range");
    return {
      stamp: s.__probe ?? null,
      focused: document.activeElement === s,
      rebuiltSinceStart: s.__probe !== 42,
      cardCount: document.querySelectorAll(".card").length,
    };
  })()
`);

// A pick must stick: no poller may snap the control back under the user's hand.
const picked = await evalJs(`
  (async () => {
    const s = document.querySelector(".card select.range");
    s.focus();
    s.value = "0.5";
    s.dispatchEvent(new Event("change", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 1200));
    return { value: s.value, focused: document.activeElement === s, same: s.__probe === 42 };
  })()
`);

const result = {
  ok: survived.stamp === 42 && survived.focused && picked.value === "0.5",
  options: stamped.options,
  startValue: stamped.value,
  afterFourPolls: survived,
  afterPick: picked,
};
console.log(JSON.stringify(result, null, 2));

await fetch(`http://127.0.0.1:${port}/json/close/${target.id}`);
process.exit(result.ok ? 0 : 1);
