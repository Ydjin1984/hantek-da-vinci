// Drives the real interface in a real browser over the DevTools protocol.
//
// The point is to test what a person does — click "Удержать экран", drag the
// trigger marker, type a level, press "Стоп" — and then ask the server whether it
// believes the same thing. Assertions that only read the DOM cannot tell whether
// the command ever reached the engine.
//
//   node tools/ui-test.mjs http://127.0.0.1:9223 http://127.0.0.1:7504 docs

import { writeFileSync } from "node:fs";

const CDP = process.argv[2] ?? "http://127.0.0.1:9223";
const APP = process.argv[3] ?? "http://127.0.0.1:7504";
const OUT = process.argv[4] ?? "docs";

const version = await (await fetch(`${CDP}/json/version`)).json();
const socket = new WebSocket(version.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  socket.addEventListener("open", resolve, { once: true });
  socket.addEventListener("error", reject, { once: true });
});

let nextId = 1;
const pending = new Map();
const events = [];

socket.addEventListener("message", (event) => {
  const msg = JSON.parse(event.data);
  if (msg.id && pending.has(msg.id)) {
    const { resolve, reject } = pending.get(msg.id);
    pending.delete(msg.id);
    if (msg.error) reject(new Error(`${msg.error.message} (${JSON.stringify(msg.error.data ?? "")})`));
    else resolve(msg.result);
    return;
  }
  if (msg.method) events.push(msg);
});

function send(method, params = {}, sessionId) {
  const id = nextId++;
  const payload = { id, method, params };
  if (sessionId) payload.sessionId = sessionId;
  socket.send(JSON.stringify(payload));
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

const target = await send("Target.createTarget", { url: "about:blank" });
const attached = await send("Target.attachToTarget", { targetId: target.targetId, flatten: true });
const session = attached.sessionId;

await send("Page.enable", {}, session);
await send("Runtime.enable", {}, session);
await send("Log.enable", {}, session);
await send("Emulation.setDeviceMetricsOverride", { width: 1500, height: 950, deviceScaleFactor: 1, mobile: false }, session);

const results = [];
function record(name, ok, detail = "") {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? ` — ${detail}` : ""}`);
}

async function evaluate(expression) {
  const result = await send(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true, userGesture: true },
    session,
  );
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
  }
  return result.result.value;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function shot(name) {
  const { data } = await send("Page.captureScreenshot", { format: "png" }, session);
  const path = `${OUT}/${name}.png`;
  writeFileSync(path, Buffer.from(data, "base64"));
  return path;
}

async function center(selector) {
  return evaluate(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2, w: r.width, h: r.height };
  })()`);
}

async function click(selector) {
  const at = await center(selector);
  if (!at) throw new Error(`нет ${selector}`);
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: at.x, y: at.y, button: "left", clickCount: 1 }, session);
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: at.x, y: at.y, button: "left", clickCount: 1 }, session);
  await sleep(700);
}

async function drag(from, to, steps = 8) {
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: from.x, y: from.y, button: "left", clickCount: 1 }, session);
  for (let i = 1; i <= steps; i++) {
    const x = from.x + ((to.x - from.x) * i) / steps;
    const y = from.y + ((to.y - from.y) * i) / steps;
    await send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, button: "left" }, session);
  }
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: to.x, y: to.y, button: "left", clickCount: 1 }, session);
  await sleep(700);
}

const info = () => evaluate(`fetch('/api/info').then(r => r.json())`);
const capture = () => evaluate(`fetch('/api/capture/v2?width=400').then(r => r.json())`);

try {
  await send("Page.navigate", { url: APP }, session);
  await sleep(2500);

  const errors = events
    .filter(
      (e) =>
        e.method === "Runtime.exceptionThrown" ||
        (e.method === "Log.entryAdded" && e.params.entry.level === "error"),
    )
    .map((e) => e.params?.exceptionDetails?.text ?? e.params?.entry?.text ?? "?");
  record("страница загрузилась без ошибок JS", errors.length === 0, errors.join("; ").slice(0, 300));

  const shell = await evaluate(`({
    cards: document.querySelectorAll('.channel').length,
    canvas: !!document.getElementById('plot'),
    mode: document.getElementById('trig-mode')?.value,
    enabled: [...document.querySelectorAll('.channel')].filter(c => c.querySelector('[data-role=on]').checked).length,
  })`);
  record("8 карточек каналов и canvas", shell.cards === 8 && shell.canvas, JSON.stringify(shell));

  await shot("ui-1-free");

  // --- The frame the UI draws is the frame the server publishes.
  const c1 = await capture();
  const drawn = await evaluate(`document.getElementById('readout').textContent`);
  const shown = Number(/кадр\s+(\d+)/.exec(drawn)?.[1] ?? -1);
  // The poll runs at ~90 ms, so the interface may legitimately be one or two
  // frames behind a capture requested at this instant; what matters is that it is
  // not stale and that the id it shows is a frame the engine really published.
  record(
    "кадр на экране совпадает с кадром сервера",
    shown > 0 && Math.abs(c1.frame_id - shown) <= 3,
    `сервер ${c1.frame_id}, экран ${shown}: ${drawn.slice(0, 60)}`,
  );

  const metrics = await evaluate(`(() => {
    const card = document.querySelectorAll('.channel')[0];
    return [...card.querySelectorAll('.metrics span')].map(s => s.textContent);
  })()`);
  record("карточка CH1 показывает измерения кадра", metrics.some((m) => /В/.test(m)), metrics.slice(0, 6).join(" | "));

  // --- Wheel changes the time base.
  const before = (await info()).time_div_id;
  const canvasBox = await center("#plot");
  await send("Input.dispatchMouseEvent", { type: "mouseWheel", x: canvasBox.x, y: canvasBox.y, deltaX: 0, deltaY: 120 }, session);
  await sleep(900);
  const after = (await info()).time_div_id;
  record("колесо меняет развёртку", after !== before, `${before} → ${after}`);

  // --- Cursor drag changes Δt.
  const rect = await evaluate(`(() => { const r = document.getElementById('plot').getBoundingClientRect();
    return { left: r.left, top: r.top, width: r.width, height: r.height }; })()`);
  const midY = rect.top + rect.height * 0.35;
  await drag(
    { x: rect.left + rect.width * 0.4, y: midY },
    { x: rect.left + rect.width * 0.55, y: midY },
  );
  const readout = await evaluate(`document.getElementById('readout').textContent`);
  record("курсор A двигается перетаскиванием", /Δt/.test(readout) && readout !== "", readout.slice(0, 90));

  // --- Hold freezes the frame while acquisition continues.
  await click("#hold");
  await sleep(900);
  const heldInfo = await info();
  const heldFrame = await capture();
  await sleep(1200);
  const stillFrame = await capture();
  const laterInfo = await info();
  record("Hold фиксирует кадр", heldInfo.held && heldFrame.frame_id === stillFrame.frame_id,
    `id ${heldFrame.frame_id} → ${stillFrame.frame_id}, held=${heldInfo.held}`);
  record("захват под Hold продолжается", laterInfo.blocks_total > heldInfo.blocks_total,
    `${heldInfo.blocks_total} → ${laterInfo.blocks_total}`);
  await shot("ui-2-hold");
  const csvStatus = await evaluate(`fetch('/api/frame.csv?frame_id=${heldFrame.frame_id}').then(r => r.status)`);
  record("CSV зафиксированного кадра отдаётся", csvStatus === 200, `HTTP ${csvStatus}`);
  const pngSize = await evaluate(`new Promise(resolve => document.getElementById('plot').toBlob(b => resolve(b ? b.size : 0)))`);
  record("PNG экрана собирается", pngSize > 2000, `${pngSize} байт`);
  await click("#hold");
  await sleep(600);
  const released = await info();
  record("Hold снимается повторным нажатием", released.held === false, `held=${released.held}`);

  // --- Run / Stop.
  await click("#run");
  await sleep(900);
  const stopped = await info();
  const stopText = await evaluate(`document.getElementById('run').textContent`);
  record("Стоп останавливает захват", stopped.running === false && stopText.trim() === "Пуск", `running=${stopped.running}, кнопка="${stopText}"`);
  const lastFrame = await capture();
  record("в Stop последний кадр доступен", lastFrame && lastFrame.frame_id > 0, `кадр ${lastFrame.frame_id}`);
  await click("#run");
  await sleep(900);
  const resumed = await info();
  record("Пуск возобновляет захват", resumed.running === true, `running=${resumed.running}`);

  // --- Trigger through the controls, not through curl.
  await evaluate(`(() => {
    const s = document.getElementById('trig-mode'); s.value = 'normal'; s.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await sleep(600);
  await evaluate(`(() => {
    const l = document.getElementById('trig-level'); l.value = '-0.17';
    l.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await sleep(2000);
  const normal = await info();
  const normalFrame = await capture();
  record("Normal синхронизируется по уровню", normal.trigger.mode === "normal" && normalFrame.trigger.free === false && normalFrame.trigger.pct > 20,
    `state=${normal.state} pct=${normalFrame.trigger.pct} free=${normalFrame.trigger.free}`);
  await shot("ui-3-normal");

  // --- Single: one frame per arm.
  await click("#arm");
  await sleep(1600);
  const single = await info();
  const singleFrame = await capture();
  await sleep(1500);
  const singleLater = await capture();
  record("Single ловит ровно один кадр", singleFrame.frame_id === singleLater.frame_id && single.state === "captured",
    `id ${singleFrame.frame_id} → ${singleLater.frame_id}, state=${single.state}`);
  record("Single ставит фронт на заданную долю", Math.abs(singleFrame.trigger.pct - singleFrame.trigger.pretrigger_pct) < 3,
    `pct=${singleFrame.trigger.pct.toFixed(1)} при заданных ${singleFrame.trigger.pretrigger_pct}`);
  await shot("ui-4-single");

  // --- Eight channels at once.
  await evaluate(`fetch('/api/control?cmd=channels&list=1,2,3,4,5,6,7,8').then(r => r.status)`);
  await evaluate(`(() => { const s = document.getElementById('trig-mode'); s.value = 'free'; s.dispatchEvent(new Event('change', { bubbles: true })); })()`);
  await sleep(2500);
  const eight = await capture();
  record("восемь каналов в одном кадре", eight.channels.length === 8, `каналов: ${eight.channels.length}`);
  const laneLabels = await evaluate(`document.querySelectorAll('.channel').length`);
  record("все карточки на месте при 8 каналах", laneLabels === 8, `${laneLabels}`);
  await shot("ui-5-eight");

  // --- Back to two channels for the remaining checks.
  await evaluate(`fetch('/api/control?cmd=channels&list=1,2')`);
  await sleep(1500);

  // --- Cursors in engineering units: drag A to 20 % and B to 40 % of a 20 ms
  // window and the readout must say 4 ms / 250 Hz.
  // Placed by keyboard, not by a pixel drag: 50 steps left put A on the left edge
  // and 40 steps right put B on 40 % of the window, whatever the layout is. A
  // pointer drag lands wherever the lanes happen to be, and the number under test
  // here is the conversion into milliseconds, not the gesture (that one is checked
  // by the drag test above).
  const plot = await evaluate(`(() => { const r = document.getElementById('plot').getBoundingClientRect();
    return { left: r.left + 88, right: r.left + r.width - 42, top: r.top, height: r.height }; })()`);
  const pw = plot.right - plot.left;
  // The arrow keys are handled by the canvas, so the events go there and it gets
  // the focus first — dispatching on window proves nothing.
  const key = `(() => {
    const cv = document.getElementById('plot');
    cv.focus();
    const send = (k) => cv.dispatchEvent(new KeyboardEvent('keydown', { key: k, bubbles: true }));
    send('a'); for (let i = 0; i < 100; i++) send('ArrowLeft');
    send('b'); for (let i = 0; i < 100; i++) send('ArrowLeft');
    for (let i = 0; i < 40; i++) send('ArrowRight');
  })()`;
  await evaluate(key);
  await sleep(400);
  const cur = await evaluate(`document.getElementById('readout').textContent`);
  const dtMatch = /Δt\s*([\d.,]+)\s*мкс|Δt\s*([\d.,]+)\s*мс/.exec(cur);
  const dtMs = dtMatch ? Number((dtMatch[2] ?? dtMatch[1]).replace(",", ".")) / (dtMatch[2] ? 1 : 1000) : NaN;
  const freqMatch = /1\/\|Δt\|\s*([\d.,]+)\s*Гц|1\/\|Δt\|\s*([\d.,]+)\s*кГц/.exec(cur);
  const freq = freqMatch ? Number((freqMatch[2] ?? freqMatch[1]).replace(",", ".")) * (freqMatch[2] ? 1000 : 1) : NaN;
  // The window comes from the same readout the Δt came from: the frame on screen
  // is one acquisition, and the base is clamped, so a later capture() can describe
  // a slightly different window and make a correct conversion look wrong.
  const winMatch = /окно\s*([\d.,]+)\s*(мс|мкс)/.exec(cur);
  const windowMs = winMatch
    ? Number(winMatch[1].replace(",", ".")) * (winMatch[2] === "мс" ? 1 : 0.001)
    : (await capture()).window_s * 1000;
  const expectedDt = 0.4 * windowMs;
  record(
    "курсоры показывают Δt в единицах кадра",
    Math.abs(dtMs - expectedDt) < Math.max(0.15, windowMs * 0.03),
    `Δt ${dtMs} мс, ожидалось ${expectedDt.toFixed(2)} мс (окно ${windowMs.toFixed(2)} мс)`,
  );
  record(
    "1/|Δt| соответствует Δt",
    Math.abs(freq - 1000 / dtMs) < Math.max(2, (1000 / dtMs) * 0.05),
    `${freq} Гц против ${(1000 / dtMs).toFixed(1)} Гц`,
  );

  await evaluate(`(() => {
    const s = document.getElementById('cursor-mode'); s.value = 'voltage'; s.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await sleep(500);
  const laneBox = await evaluate(`(() => { const r = document.getElementById('plot').getBoundingClientRect();
    return { left: r.left, top: r.top, height: r.height }; })()`);
  const volTop = laneBox.top + 40;
  await drag({ x: laneBox.left + 400, y: volTop + 30 }, { x: laneBox.left + 400, y: volTop + 20 }, 3);
  await drag({ x: laneBox.left + 400, y: volTop + 150 }, { x: laneBox.left + 400, y: volTop + 120 }, 4);
  const volReadout = await evaluate(`document.getElementById('readout').textContent`);
  record("курсоры напряжения показывают ΔV", /ΔV/.test(volReadout) && /[-\d]/.test(volReadout), volReadout.slice(0, 90));
  await evaluate(`(() => {
    const s = document.getElementById('cursor-mode'); s.value = 'time'; s.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);

  // --- Centre is a view offset: typed into the card it must reach the server
  // and survive the polling repaint with the focus still in the field.
  const centreSet = await evaluate(`(async () => {
    const card = document.querySelectorAll('.channel')[0];
    const input = card.querySelector('[data-role=center]');
    input.focus();
    input.value = '1';
    input.dispatchEvent(new Event('change', { bubbles: true }));
    await new Promise(r => setTimeout(r, 900));
    const meta = await (await fetch('/api/capture/v2?width=200')).json();
    return { focus: document.activeElement === input, value: input.value, center: meta.channel_meta[0].center_v };
  })()`);
  record(
    "центр канала доходит до сервера и поле не теряет фокус",
    Math.abs(centreSet.center - 1) < 1e-3 && centreSet.focus && centreSet.value === "1",
    JSON.stringify(centreSet),
  );
  await evaluate(`(() => {
    const card = document.querySelectorAll('.channel')[0];
    const input = card.querySelector('[data-role=center]');
    input.value = '0'; input.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);

  // --- One channel at a time.
  await evaluate(`fetch('/api/control?cmd=channels&list=1')`);
  await sleep(1500);
  const one = await capture();
  record("один канал в кадре", one.channels.length === 1 && one.sample_rate > 0, `каналов ${one.channels.length}, ${one.sample_rate} Гц`);

  // --- Windows scales the window; the canvas must follow the DPI, not blur.
  await send("Emulation.setDeviceMetricsOverride", { width: 1500, height: 950, deviceScaleFactor: 1.5, mobile: false }, session);
  await sleep(1200);
  const dpi = await evaluate(`(() => { const c = document.getElementById('plot');
    return { w: c.width, css: c.clientWidth }; })()`);
  record("canvas учитывает DPI", Math.abs(dpi.w - dpi.css * 1.5) <= 2, `${dpi.w} против ${dpi.css}*1.5`);
  await send("Emulation.setDeviceMetricsOverride", { width: 1500, height: 950, deviceScaleFactor: 1, mobile: false }, session);
  await sleep(700);

  // --- Resize with the screen held: same frame, redrawn.
  await click("#hold");
  await sleep(700);
  const heldBefore = await capture();
  await send("Emulation.setDeviceMetricsOverride", { width: 1150, height: 780, deviceScaleFactor: 1, mobile: false }, session);
  await sleep(1200);
  const heldAfter = await capture();
  await send("Emulation.setDeviceMetricsOverride", { width: 1500, height: 950, deviceScaleFactor: 1, mobile: false }, session);
  await sleep(900);
  record(
    "смена размера окна не меняет удержанный кадр",
    heldBefore.frame_id === heldAfter.frame_id,
    `${heldBefore.frame_id} → ${heldAfter.frame_id}`,
  );
  await click("#hold");
  await sleep(500);

  // --- The old client keeps working: DVF1 unchanged.
  const dvf1 = await evaluate(`fetch('/api/frame?width=64').then(async r => ({ status: r.status, magic: String.fromCharCode(...new Uint8Array(await r.arrayBuffer()).slice(0, 4)) }))`);
  record("старый клиент DVF1 получает кадр", dvf1.status === 200 && dvf1.magic === "DVF1", JSON.stringify(dvf1));

  // --- A bad command is reported, not swallowed.
  await evaluate(`(() => {
    const l = document.getElementById('trig-level'); l.value = '999';
    l.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  // The status line keeps the error for one poll and then goes back to the live
  // numbers, so look for it for a while instead of at one instant.
  let statusText = "";
  for (let i = 0; i < 20; i++) {
    await sleep(100);
    statusText = await evaluate(`document.getElementById('status').textContent`);
    if (/ошибка/i.test(statusText)) break;
  }
  record("ошибка команды видна в статусе", /ошибка/i.test(statusText), statusText.slice(0, 120));

  await shot("ui-6-final");
} catch (error) {
  record("прогон завершился без исключения", false, error.message);
} finally {
  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} проверок прошло`);
  writeFileSync(`${OUT}/ui-test.json`, JSON.stringify({ results, events: events.length }, null, 2));
  socket.close();
  process.exit(failed.length ? 1 : 0);
}
