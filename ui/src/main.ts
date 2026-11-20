import "./style.css";

window.addEventListener("error", (e) => {
  (window as unknown as { __err?: string }).__err = String(e.message);
});
window.addEventListener("unhandledrejection", (e) => {
  (window as unknown as { __err?: string }).__err = String(e.reason);
});

/**
 * Осциллограф Хелтег да Винчи — клиент.
 *
 * Вся тяжёлая работа (демультиплексирование, калибровка, min/max по колонкам)
 * уже сделана в Rust; сюда приходит готовая огибающая: на каждый пиксель —
 * настоящие минимум и максимум. Поэтому пик нельзя потерять, а «квадратики»
 * не появляются: рисуется не один отсчёт на пиксель, а полоса от min до max.
 */

type ChannelInfo = {
  ch: number;
  enabled: boolean;
  range_id: number;
  range: string;
  resolution: number;
  min: number | null;
  max: number | null;
  pp: number | null;
  mean: number | null;
  rms: number | null;
  rms_ac: number | null;
  freq: number | null;
  duty: number | null;
  peak_min: number | null;
  peak_max: number | null;
};

type Info = {
  running: boolean;
  link: string;
  sample_rate: number;
  block_samples: number;
  blocks_per_s: number;
  time_div: string;
  error: string | null;
  log: { path: string | null; bytes: number; seconds: number };
  blocks_total: number;
};

const COLORS = [
  "#34d399", // CH1 teal
  "#fbbf24", // CH2 amber
  "#f472b6", // CH3 pink
  "#60a5fa", // CH4 blue
  "#a78bfa", // CH5 violet
  "#f87171", // CH6 red
  "#22d3ee", // CH7 cyan
  "#a3e635", // CH8 lime
];

const RANGE_LABELS = ["±0,4 В", "±2,5 В", "±20 В"];

const state = {
  span: 0.2,
  running: true,
  channels: [] as ChannelInfo[],
  info: null as Info | null,
  frame: null as Frame | null,
  mouseX: -1,
  logOn: false,
};

type Frame = {
  width: number;
  mask: number;
  dtNs: number;
  chans: number[];
  /** per channel: Float32Array of [lo, hi] pairs in volts */
  data: Float32Array[];
};

const app = document.getElementById("app")!;
app.innerHTML = `
  <header class="bar">
    <div class="brand">
      <span class="logo">∿</span>
      <div>
        <h1>Хелтег да Винчи</h1>
        <p>8 каналов · 12 бит · честные вольты</p>
      </div>
    </div>
    <div class="chips" id="chips"></div>
    <div class="actions">
      <button id="btn-run" class="btn primary">Стоп</button>
      <button id="btn-reset" class="btn">Сброс пиков</button>
      <button id="btn-log" class="btn">Лог</button>
    </div>
  </header>
  <main>
    <section class="scope">
      <canvas id="plot"></canvas>
      <div class="hint" id="hint">колесо — масштаб времени · клик по имени канала — вкл/выкл · клик по пределу — диапазон</div>
      <div id="dbg" style="display:none"></div>
    </section>
    <aside class="cards" id="cards"></aside>
  </main>
`;

const canvas = document.getElementById("plot") as HTMLCanvasElement;
const ctx = canvas.getContext("2d", { alpha: false })!;
const cards = document.getElementById("cards")!;
const chips = document.getElementById("chips")!;
const hint = document.getElementById("hint")!;
const btnRun = document.getElementById("btn-run") as HTMLButtonElement;
const btnReset = document.getElementById("btn-reset") as HTMLButtonElement;
const btnLog = document.getElementById("btn-log") as HTMLButtonElement;

function fmtV(v: number | null): string {
  if (v === null || !isFinite(v)) return "—";
  const a = Math.abs(v);
  if (a < 1) return `${(v * 1000).toFixed(0)} мВ`;
  return `${v.toFixed(3)} В`;
}

function fmtFreq(f: number | null): string {
  if (!f || !isFinite(f)) return "—";
  if (f >= 1000) return `${(f / 1000).toFixed(2)} кГц`;
  return `${f.toFixed(1)} Гц`;
}

function fmtBytes(n: number): string {
  if (n > 1e9) return `${(n / 1e9).toFixed(2)} ГБ`;
  if (n > 1e6) return `${(n / 1e6).toFixed(1)} МБ`;
  if (n > 1e3) return `${(n / 1e3).toFixed(0)} КБ`;
  return `${n} Б`;
}

function fmtSpan(s: number): string {
  if (s >= 1) return `${s.toFixed(1)} с`;
  if (s >= 1e-3) return `${(s * 1e3).toFixed(1)} мс`;
  return `${(s * 1e6).toFixed(0)} мкс`;
}

// ---------------------------------------------------------------- data fetch

async function fetchInfo() {
  try {
    const r = await fetch("/api/info");
    state.info = await r.json();
    state.running = state.info!.running;
    state.logOn = !!state.info!.log.path;
    renderChips();
  } catch {
    /* daemon restarting */
  }
}

async function fetchChannels() {
  try {
    const r = await fetch("/api/channels");
    state.channels = await r.json();
    renderCards();
  } catch {
    /* ignore */
  }
}

async function fetchFrame() {
  const width = Math.max(64, Math.round(canvas.clientWidth - 96 - 8));
  try {
    const r = await fetch(`/api/frame?width=${width}&span=${state.span}`);
    const buf = await r.arrayBuffer();
    state.frame = parseFrame(buf);
  } catch {
    /* ignore */
  }
}

function parseFrame(buf: ArrayBuffer): Frame | null {
  const dv = new DataView(buf);
  // magic u32 | width u32 | mask u32 | dt_ns f32 | t0_ns u64 | nch u32 = 28 bytes
  if (buf.byteLength < 28) return null;
  const magic = String.fromCharCode(dv.getUint8(0), dv.getUint8(1), dv.getUint8(2), dv.getUint8(3));
  if (magic !== "DVF1") return null;
  const width = dv.getUint32(4, true);
  const mask = dv.getUint32(8, true);
  const dtNs = dv.getFloat32(12, true);
  const nch = dv.getUint32(24, true);
  const chans: number[] = [];
  for (let c = 0; c < 8; c++) if (mask & (1 << c)) chans.push(c);
  const data: Float32Array[] = [];
  let off = 28;
  for (let i = 0; i < nch; i++) {
    const arr = new Float32Array(width * 2);
    for (let x = 0; x < width; x++) {
      arr[x * 2] = dv.getInt16(off, true) / 1000;
      arr[x * 2 + 1] = dv.getInt16(off + 2, true) / 1000;
      off += 4;
    }
    data.push(arr);
  }
  return { width, mask, dtNs, chans, data };
}

// ------------------------------------------------------------------ rendering

function resize() {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  canvas.width = Math.round(w * dpr);
  canvas.height = Math.round(h * dpr);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}

const GUTTER = 96;
const RIGHT = 8;
const RULER = 26;
const LANES = 8;

function laneHeight(): number {
  return (canvas.clientHeight - RULER) / LANES;
}

function vppOf(resolution: number): number {
  // 4096 кодов АЦП на полную шкалу
  return 4096 * resolution;
}

function draw() {
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  const dbg = document.getElementById("dbg");
  if (dbg) {
    dbg.textContent = `canvas=${w}x${h} px=${canvas.width}x${canvas.height} frame=${state.frame?.width ?? 0} chans=${state.channels.length}`;
  }
  if (w === 0 || h === 0) return;
  if (canvas.width !== Math.round(w * (window.devicePixelRatio || 1))) resize();
  const lh = laneHeight();
  ctx.fillStyle = "#0b0f14";
  ctx.fillRect(0, 0, w, h);

  const plotW = w - GUTTER - RIGHT;

  for (let lane = 0; lane < LANES; lane++) {
    const y0 = lane * lh;
    const ch = state.channels[lane];
    const enabled = ch ? ch.enabled : false;
    const color = COLORS[lane];

    ctx.fillStyle = lane % 2 ? "#0d1218" : "#0b0f14";
    ctx.fillRect(0, y0, w, lh);
    ctx.strokeStyle = "#161d26";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(0, Math.round(y0) + 0.5);
    ctx.lineTo(w, Math.round(y0) + 0.5);
    ctx.stroke();

    if (!enabled || !ch) {
      ctx.fillStyle = "#2a3442";
      ctx.font = "500 12px ui-monospace, monospace";
      ctx.textAlign = "left";
      ctx.fillText(`CH${lane + 1}`, 14, y0 + lh / 2 + 4);
      ctx.fillText("выключен", 14 + 44, y0 + lh / 2 + 4);
      continue;
    }

    // Vertical grid: 8 divisions across the lane, like the hardware screen.
    const vpp = vppOf(ch.resolution);
    const mid = y0 + lh / 2;
    const pxPerV = (lh - 8) / vpp;
    ctx.strokeStyle = "#151c25";
    for (let d = 1; d < 8; d++) {
      const y = y0 + (lh * d) / 8;
      ctx.beginPath();
      ctx.moveTo(GUTTER, Math.round(y) + 0.5);
      ctx.lineTo(w - RIGHT, Math.round(y) + 0.5);
      ctx.stroke();
    }
    ctx.save();
    ctx.setLineDash([3, 4]);
    ctx.strokeStyle = "#26313f";
    ctx.beginPath();
    ctx.moveTo(GUTTER, Math.round(mid) + 0.5);
    ctx.lineTo(w - RIGHT, Math.round(mid) + 0.5);
    ctx.stroke();
    ctx.restore();

    const idx = state.frame?.chans.indexOf(lane) ?? -1;
    if (idx >= 0 && state.frame) {
      const arr = state.frame.data[idx];
      const n = state.frame.width;

      // Peak-hold: never reset unless asked, so a one-off spike stays visible.
      if (ch.peak_min !== null && ch.peak_max !== null) {
        ctx.strokeStyle = color + "55";
        ctx.setLineDash([2, 6]);
        for (const v of [ch.peak_min, ch.peak_max]) {
          const y = mid - v * pxPerV;
          if (y > y0 + 1 && y < y0 + lh - 1) {
            ctx.beginPath();
            ctx.moveTo(GUTTER, y);
            ctx.lineTo(w - RIGHT, y);
            ctx.stroke();
          }
        }
        ctx.setLineDash([]);
      }

      // The envelope: one vertical band per pixel column, min..max.
      ctx.fillStyle = color + "cc";
      const step = plotW / n;
      for (let x = 0; x < n; x++) {
        const lo = arr[x * 2];
        const hi = arr[x * 2 + 1];
        const yl = mid - hi * pxPerV;
        const yh = mid - lo * pxPerV;
        const top = Math.max(y0 + 1, Math.min(yl, yh));
        const bot = Math.min(y0 + lh - 1, Math.max(yl, yh));
        const px = GUTTER + x * step;
        const pw = Math.max(1, step);
        if (bot - top < 1.5) {
          ctx.fillRect(px, top, pw, 1.2);
        } else {
          ctx.globalAlpha = 0.55;
          ctx.fillRect(px, top, pw, bot - top);
          ctx.globalAlpha = 1;
        }
      }

      // Gutter: name, range, per-division scale.
      ctx.fillStyle = color;
      ctx.font = "700 13px ui-monospace, monospace";
      ctx.textAlign = "left";
      ctx.fillText(`CH${lane + 1}`, 12, y0 + 20);
      ctx.fillStyle = "#7d8da3";
      ctx.font = "500 10px ui-monospace, monospace";
      ctx.fillText(RANGE_LABELS[ch.range_id - 1] ?? ch.range, 12, y0 + 34);
      ctx.fillText(`${(vpp / 8).toFixed(vpp / 8 < 1 ? 2 : 1)} В/дел`, 12, y0 + 47);

      // Right: the number that matters — the value right now.
      const now = arr[(n - 1) * 2 + 1];
      ctx.textAlign = "right";
      ctx.fillStyle = color;
      ctx.font = "700 15px ui-monospace, monospace";
      ctx.fillText(fmtV(now), w - RIGHT, y0 + 20);
      ctx.fillStyle = "#6b7c93";
      ctx.font = "500 10px ui-monospace, monospace";
      ctx.fillText(`пик ${fmtV(ch.peak_max)}`, w - RIGHT, y0 + 34);
    }

    // Cursor
    if (state.mouseX > GUTTER && state.mouseX < w - RIGHT) {
      ctx.strokeStyle = "#3d4d63";
      ctx.beginPath();
      ctx.moveTo(state.mouseX, y0);
      ctx.lineTo(state.mouseX, y0 + lh);
      ctx.stroke();
    }
  }

  // Time ruler
  const ry = h - RULER;
  ctx.fillStyle = "#0a0e13";
  ctx.fillRect(0, ry, w, RULER);
  ctx.strokeStyle = "#1c2531";
  ctx.beginPath();
  ctx.moveTo(0, ry + 0.5);
  ctx.lineTo(w, ry + 0.5);
  ctx.stroke();
  ctx.fillStyle = "#5f7089";
  ctx.font = "500 10px ui-monospace, monospace";
  ctx.textAlign = "left";
  ctx.fillText(`окно ${fmtSpan(state.span)} · деление ${fmtSpan(state.span / 10)}`, GUTTER, ry + 17);
  ctx.textAlign = "right";
  ctx.fillText(state.running ? "захват идёт" : "остановлено", w - RIGHT, ry + 17);
}

function renderChips() {
  const i = state.info;
  if (!i) return;
  const rate = i.sample_rate > 0 ? `${(i.sample_rate / 1000).toFixed(1)} квыб/с` : "—";
  const parts = [
    `<span class="chip ${i.error ? "bad" : "ok"}">${i.error ? "нет связи" : "прибор"}</span>`,
    `<span class="chip">${rate}</span>`,
    `<span class="chip">${i.blocks_per_s > 0 ? i.blocks_per_s.toFixed(0) + " блок/с" : "—"}</span>`,
    `<span class="chip">${i.time_div}</span>`,
  ];
  if (i.log.path) {
    parts.push(
      `<span class="chip log">лог ${fmtBytes(i.log.bytes)} · ${i.log.seconds.toFixed(0)} с</span>`,
    );
  }
  chips.innerHTML = parts.join("");
}

function renderCards() {
  cards.innerHTML = state.channels
    .map((c) => {
      const color = COLORS[c.ch - 1];
      const off = !c.enabled;
      return `
      <article class="card ${off ? "off" : ""}" data-ch="${c.ch}" style="--c:${color}">
        <div class="card-top">
          <button class="dot" title="включить/выключить" data-act="toggle">${c.ch}</button>
          <span class="range" data-act="range" title="клик — сменить диапазон">${c.range}</span>
        </div>
        <div class="big">${fmtV(c.max)}</div>
        <div class="row"><span>Vmin</span><b>${fmtV(c.min)}</b></div>
        <div class="row"><span>Vmax</span><b>${fmtV(c.max)}</b></div>
        <div class="row"><span>Vpp</span><b>${fmtV(c.pp)}</b></div>
        <div class="row"><span>RMS</span><b>${fmtV(c.rms)}</b></div>
        <div class="row"><span>Частота</span><b>${fmtFreq(c.freq)}</b></div>
        <div class="row"><span>Скважн.</span><b>${c.duty !== null ? (c.duty * 100).toFixed(0) + " %" : "—"}</b></div>
        <div class="row peak"><span>Пик-холд</span><b>${fmtV(c.peak_max)}</b></div>
      </article>`;
    })
    .join("");
}

// ------------------------------------------------------------------ controls

canvas.addEventListener("wheel", (e) => {
  e.preventDefault();
  const k = e.deltaY > 0 ? 1.25 : 0.8;
  state.span = Math.min(5, Math.max(0.002, state.span * k));
  fetchFrame();
}, { passive: false });

canvas.addEventListener("mousemove", (e) => {
  const rect = canvas.getBoundingClientRect();
  state.mouseX = e.clientX - rect.left;
});

canvas.addEventListener("mouseleave", () => {
  state.mouseX = -1;
});

canvas.addEventListener("click", (e) => {
  const rect = canvas.getBoundingClientRect();
  const x = e.clientX - rect.left;
  const lane = Math.floor((e.clientY - rect.top) / laneHeight());
  if (x > GUTTER || lane < 0 || lane > 7) return;
  const ch = lane + 1;
  if (x < 44) toggleChannel(ch);
  else cycleRange(ch);
});

cards.addEventListener("click", (e) => {
  const el = (e.target as HTMLElement).closest("[data-act]") as HTMLElement | null;
  const card = (e.target as HTMLElement).closest(".card") as HTMLElement | null;
  if (!el || !card) return;
  const ch = Number(card.dataset.ch);
  if (el.dataset.act === "toggle") toggleChannel(ch);
  if (el.dataset.act === "range") cycleRange(ch);
});

function toggleChannel(ch: number) {
  const enabled = state.channels.filter((c) => c.enabled).map((c) => c.ch);
  const next = enabled.includes(ch) ? enabled.filter((c) => c !== ch) : [...enabled, ch].sort((a, b) => a - b);
  if (next.length === 0) return;
  fetch(`/api/control?cmd=channels&list=${next.join(",")}`);
  state.channels = state.channels.map((c) => ({ ...c, enabled: next.includes(c.ch) }));
  renderCards();
}

function cycleRange(ch: number) {
  const c = state.channels.find((x) => x.ch === ch);
  if (!c) return;
  const next = (c.range_id % 3) + 1;
  fetch(`/api/control?cmd=range&ch=${ch}&r=${next}`);
  c.range_id = next;
  c.range = RANGE_LABELS[next - 1];
  renderCards();
}

btnRun.addEventListener("click", () => {
  fetch(`/api/control?cmd=${state.running ? "stop" : "run"}`);
  state.running = !state.running;
  btnRun.textContent = state.running ? "Стоп" : "Пуск";
  btnRun.classList.toggle("primary", state.running);
});

btnReset.addEventListener("click", () => fetch("/api/control?cmd=reset"));

btnLog.addEventListener("click", () => {
  fetch(`/api/control?cmd=${state.logOn ? "log_stop" : "log_start"}`);
  state.logOn = !state.logOn;
  btnLog.classList.toggle("on", state.logOn);
});

// ---------------------------------------------------------------------- loop

let busy = false;
async function tick() {
  if (!busy) {
    busy = true;
    await fetchFrame();
    busy = false;
  }
  draw();
  requestAnimationFrame(tick);
}

window.addEventListener("resize", () => {
  resize();
  draw();
});

resize();
fetchInfo();
fetchChannels().then(() => {
  draw();
  tick();
});
setInterval(fetchInfo, 1000);
setInterval(fetchChannels, 400);
setInterval(() => {
  hint.style.opacity = "0";
}, 8000);
