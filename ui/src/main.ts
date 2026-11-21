// Interface for the Hantek 1008 scope.
//
// The plot is drawn by hand on a canvas: eight lanes, one transform per lane, and
// every number on screen (grid, ticks, trigger, cursors, peaks) goes through that
// same transform. That is the only way the labels and the trace can disagree —
// and the reason the previous version's scale numbers did not line up with the
// volts they claimed.
//
// The server owns the truth: it publishes a frame with an id, its own metadata and
// the envelope of that exact capture. The UI only ever draws what came back in one
// answer, so a frozen screen, its measurements, CSV and PNG all describe the same
// samples.

type Mode = "free" | "auto" | "normal" | "single";
type Edge = "rise" | "fall";
type CursorMode = "time" | "voltage" | "off";

import "./style.css";

interface TriggerCfg {
  mode: Mode;
  source: number;
  edge: Edge;
  level_v: number;
  pretrigger_pct: number;
  timeout_ms: number;
  hysteresis_v: number | null;
}

interface Info {
  running: boolean;
  link: string;
  sample_rate: number;
  block_samples: number;
  blocks_per_s: number;
  time_div: string;
  time_div_id: number;
  window_s: number;
  window_clamped: boolean;
  error: string | null;
  state: string;
  frame_id: number;
  held: boolean;
  rec: boolean;
  trigger: TriggerCfg;
  /** Extremes of the trigger source in the latest block, volts. */
  source_span: [number, number] | null;
  /** Set for a while after the engine moved a level no edge could reach. */
  level_rescue: { min_v: number; max_v: number; level_v: number; age_ms: number } | null;
  log: { path: string | null; bytes: number; seconds: number };
  blocks_total: number;
}

interface ChannelMeta {
  ch: number;
  range_id: number;
  full_scale_v: number;
  center_v: number;
  autoscale: boolean;
  saturated: boolean;
  dropouts: number;
  peak_min: number | null;
  peak_max: number | null;
}

interface Stat {
  ch: number;
  vmin: number | null;
  vmax: number | null;
  vpp: number | null;
  mean: number | null;
  rms: number | null;
  rms_ac: number | null;
  freq: number | null;
  duty: number | null;
  period_s: number | null;
  /** Samples the frame has per period of the frequency it measured. */
  points_per_period: number | null;
  /** True when that is too few to draw the waveform: the trace is an alias. */
  aliased: boolean;
  n: number;
  window_s: number;
}

interface Capture {
  format: string;
  frame_id: number;
  generation: number;
  held: boolean;
  running: boolean;
  state: string;
  dt_s: number;
  sample_rate: number;
  window_s: number;
  window_samples: number;
  start_sample: number;
  n_block: number;
  channels: number[];
  trigger: {
    mode: Mode;
    edge: Edge;
    source: number;
    level_v: number;
    pretrigger_pct: number;
    timeout_ms: number;
    pct: number;
    free: boolean;
    clipped_pre: boolean;
    clipped_post: boolean;
    t0_s: number;
    hysteresis_v: number;
  };
  channel_meta: ChannelMeta[];
  stats: Stat[];
  width: number;
  columns_b64: string;
  columns_bytes: number;
}

const COLORS = [
  "#46e1aa",
  "#ffd16c",
  "#ff91be",
  "#7bacff",
  "#bba5ff",
  "#ff8181",
  "#55d8ef",
  "#c4e97b",
];

const HW_RANGE = ["±0,41 В", "±2,5 В", "±20 В"];

/** Unit for the time axis: one unit for the whole row, as a scope does. */
function axisUnit(span: number): { scale: number; suffix: string } {
  if (span >= 1) return { scale: 1, suffix: "с" };
  if (span >= 1e-3) return { scale: 1e-3, suffix: "мс" };
  if (span >= 1e-6) return { scale: 1e-6, suffix: "мкс" };
  return { scale: 1e-9, suffix: "нс" };
}

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`нет элемента #${id}`);
  return el as T;
};

function fmt(v: number | null | undefined, digits = 2): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  return v.toLocaleString("ru-RU", { maximumFractionDigits: digits, minimumFractionDigits: 0 });
}

function volts(v: number | null | undefined, digits = 2): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  const a = Math.abs(v);
  if (a > 0 && a < 1) return `${fmt(v * 1000, digits)} мВ`;
  return `${fmt(v, digits)} В`;
}

function hz(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return "—";
  const a = Math.abs(v);
  if (a >= 1e6) return `${fmt(v / 1e6, 3)} МГц`;
  if (a >= 1e3) return `${fmt(v / 1e3, 3)} кГц`;
  return `${fmt(v, 1)} Гц`;
}

function seconds(v: number): string {
  const a = Math.abs(v);
  if (a >= 1) return `${fmt(v, 3)} с`;
  if (a >= 1e-3) return `${fmt(v * 1e3, 3)} мс`;
  if (a >= 1e-6) return `${fmt(v * 1e6, 1)} мкс`;
  return `${fmt(v * 1e9, 0)} нс`;
}

function nsPerDiv(id: number): number {
  const mantissa = [1, 2, 5][id % 3];
  return mantissa * Math.pow(10, Math.floor(id / 3));
}

function timeDivLabel(id: number): string {
  const ns = nsPerDiv(id);
  if (ns >= 1e6) return `${fmt(ns / 1e6, 3)} мс/дел`;
  if (ns >= 1e3) return `${fmt(ns / 1e3, 3)} мкс/дел`;
  return `${fmt(ns, 0)} нс/дел`;
}

/** Russian plural for the channel count, without a "канал(ов)" hedge. */
function channelWord(n: number): string {
  const mod100 = n % 100;
  const mod10 = n % 10;
  if (mod100 >= 11 && mod100 <= 14) return "каналов";
  if (mod10 === 1) return "канал";
  if (mod10 >= 2 && mod10 <= 4) return "канала";
  return "каналов";
}

function stateLabel(info: Info, cap: Capture | null): string {
  const mode = info.trigger.mode;
  if (!info.running) return "Захват остановлен";
  if (mode === "single") {
    if (cap && cap.state === "captured") return "Single · захвачено";
    if (info.state === "captured") return "Single · захвачено";
    return "Single · взведён, ждёт фронт";
  }
  switch (info.state) {
    case "free":
      return "Свободная развёртка";
    case "waiting":
      return "Ожидание фронта";
    case "captured":
      return "Синхронизировано";
    case "auto_free":
      return "Auto · свободный кадр";
    case "stopped":
      return "Захват остановлен";
    default:
      return info.state;
  }
}

/** State the UI needs. The frame is replaced only by an answer from the server. */
const ui = {
  info: null as Info | null,
  cap: null as Capture | null,
  columns: null as Int16Array | null,
  cfg: null as TriggerCfg | null,
  enabled: [0, 1] as number[],
  activeCh: 0,
  cursors: { mode: "time" as CursorMode, a: 0.38, b: 0.63 },
  drag: null as null | "level" | "a" | "b",
  dragTarget: "a" as "a" | "b",
  levelPreview: null as number | null,
  message: "",
  error: "",
  timeDivId: 17,
  width: 1200,
};

interface Lane {
  ch: number;
  y: number;
  lane: number;
  mid: number;
  vpd: number;
  center: number;
  yV: (v: number) => number;
  vY: (y: number) => number;
}

let layout: Lane[] = [];
let canvas: HTMLCanvasElement;
let ctx: CanvasRenderingContext2D;
/** Plot area of the last draw, so hit-testing and drawing cannot disagree. */
const geom = { left: 88, right: 0, pw: 1, top: 14, bottom: 0 };

// ---------------------------------------------------------------- API helpers

async function getJSON<T>(url: string): Promise<T> {
  const r = await fetch(url, { cache: "no-store" });
  if (!r.ok) {
    let reason = `${r.status}`;
    try {
      const body = (await r.json()) as { error?: string };
      if (body.error) reason = body.error;
    } catch {
      /* not JSON */
    }
    throw new Error(reason);
  }
  return (await r.json()) as T;
}

async function postJSON<T>(url: string, body: unknown): Promise<T> {
  const r = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    cache: "no-store",
  });
  const data = (await r.json()) as T & { ok?: boolean; error?: string };
  if (!r.ok) throw new Error(data.error ?? `${r.status}`);
  return data;
}

async function command(params: string): Promise<Record<string, unknown>> {
  return getJSON<Record<string, unknown>>(`/api/control?${params}`);
}

function say(text: string): void {
  ui.message = text;
  ui.error = "";
  // The status line is a log, not a permanent label: a stale "триггер обновлён"
  // next to the live state reads as if something is still happening.
  window.setTimeout(() => {
    if (ui.message === text) ui.message = "";
  }, 2500);
}

function fail(e: unknown): void {
  ui.error = e instanceof Error ? e.message : String(e);
  ui.message = "";
}

// ---------------------------------------------------------------- page layout

function buildShell(): void {
  const app = $("app");
  app.innerHTML = `
  <header>
    <div class="brand">
      <span class="logo" aria-hidden="true">∿</span>
      <div>
        <h1>Хелтег да Винчи</h1>
        <p id="subtitle">8 каналов · 12 бит · ждём прибор</p>
      </div>
    </div>
    <div class="actions">
      <span id="pill-link" class="pill">связь…</span>
      <span id="pill-rate" class="pill">—</span>
      <span id="pill-rec" class="pill">REC выкл</span>
      <button id="run" class="live" type="button">Стоп</button>
      <button id="hold" type="button" aria-pressed="false">Удержать экран</button>
      <button id="peaks" type="button">Сброс пиков</button>
      <button id="rec" type="button" aria-pressed="false">Запись</button>
      <button id="csv-frame" type="button">CSV кадра</button>
      <button id="png" type="button">PNG</button>
      <button id="csv-history" type="button">CSV истории</button>
    </div>
  </header>
  <main>
    <section class="panel scope">
      <div class="toolbar">
        <span id="status" class="status" role="status" aria-live="polite"></span>
        <span class="spacer"></span>
        <label>Развёртка
          <select id="time-div"></select>
        </label>
        <span id="window-info" class="hint"></span>
      </div>
      <div class="plot-wrap">
        <canvas id="plot" tabindex="0" aria-label="Осциллограммы с осями напряжения и времени"></canvas>
      </div>
      <div class="readout" id="readout"></div>
      <div class="foot">Тяните красный маркер T справа от дорожки источника — он задаёт уровень. Колесо мыши меняет развёртку. Курсоры A и B тянутся мышью и двигаются стрелками.</div>
    </section>
    <aside>
      <section class="panel section">
        <h2>Триггер</h2>
        <div class="trigger-state" id="trigger-state"></div>
        <label class="field">Режим
          <select id="trig-mode">
            <option value="free">Свободный</option>
            <option value="auto">Auto</option>
            <option value="normal">Normal</option>
            <option value="single">Single</option>
          </select>
        </label>
        <label class="field">Источник <select id="trig-source"></select></label>
        <label class="field">Фронт
          <select id="trig-edge">
            <option value="rise">↑ Нарастающий</option>
            <option value="fall">↓ Спадающий</option>
          </select>
        </label>
        <label class="field">Уровень, В <input id="trig-level" type="number" step="0.05" /></label>
        <label class="field">Предыстория
          <input id="trig-pre" type="range" min="10" max="90" step="1" value="30" />
          <output id="trig-pre-out">30%</output>
        </label>
        <label class="field">Таймаут Auto, мс <input id="trig-timeout" type="number" min="0" max="10000" step="50" /></label>
        <button id="arm" class="wide" type="button">Взвести Single</button>
        <div class="hint" id="trig-hint"></div>
      </section>
      <section class="panel section">
        <h2>Курсоры</h2>
        <label class="field">Измерение
          <select id="cursor-mode">
            <option value="time">Время (A, B)</option>
            <option value="voltage">Напряжение (A, B)</option>
            <option value="off">Выкл.</option>
          </select>
        </label>
        <div class="hint">Клавиши ← → двигают активный курсор, A и B выбирают его.</div>
      </section>
      <section class="panel section">
        <h2>Каналы</h2>
        <div class="channels" id="channels"></div>
      </section>
    </aside>
  </main>`;

  canvas = $<HTMLCanvasElement>("plot");
  const context = canvas.getContext("2d");
  if (!context) throw new Error("canvas 2d недоступен");
  ctx = context;

  const timeSelect = $<HTMLSelectElement>("time-div");
  for (let id = 0; id <= 25; id++) {
    const option = document.createElement("option");
    option.value = String(id);
    option.textContent = timeDivLabel(id);
    timeSelect.append(option);
  }

  buildCards();

  timeSelect.addEventListener("change", () => {
    void applyTimeDiv(Number(timeSelect.value));
  });
  $("run").addEventListener("click", () => void toggleRun());
  $("hold").addEventListener("click", () => void toggleHold());
  $("peaks").addEventListener("click", () => void resetPeaks());
  $("rec").addEventListener("click", () => void toggleRec());
  $("csv-frame").addEventListener("click", () => downloadFrameCsv());
  $("png").addEventListener("click", () => downloadPng());
  $("csv-history").addEventListener("click", () => downloadHistoryCsv());
  $("arm").addEventListener("click", () => void arm());

  const mode = $<HTMLSelectElement>("trig-mode");
  const source = $<HTMLSelectElement>("trig-source");
  const edge = $<HTMLSelectElement>("trig-edge");
  const level = $<HTMLInputElement>("trig-level");
  const pre = $<HTMLInputElement>("trig-pre");
  const timeout = $<HTMLInputElement>("trig-timeout");
  const cursorMode = $<HTMLSelectElement>("cursor-mode");

  mode.addEventListener("change", () => void applyTrigger({ mode: mode.value as Mode }));
  edge.addEventListener("change", () => void applyTrigger({ edge: edge.value as Edge }));
  source.addEventListener("change", () =>
    void applyTrigger({ source: Number(source.value) }),
  );
  level.addEventListener("change", () => {
    const v = Number(level.value);
    if (Number.isFinite(v)) void applyTrigger({ level_v: v });
  });
  timeout.addEventListener("change", () => {
    const v = Math.round(Number(timeout.value));
    if (Number.isFinite(v)) void applyTrigger({ timeout_ms: v });
  });
  pre.addEventListener("input", () => {
    $("trig-pre-out").textContent = `${pre.value}%`;
  });
  pre.addEventListener("change", () => void applyTrigger({ pretrigger_pct: Number(pre.value) }));
  cursorMode.addEventListener("change", () => {
    ui.cursors.mode = cursorMode.value as CursorMode;
    draw();
  });

  canvas.addEventListener("pointerdown", onPointerDown);
  canvas.addEventListener("pointermove", onPointerMove);
  for (const event of ["pointerup", "pointercancel", "lostpointercapture"]) {
    canvas.addEventListener(event, () => {
      finishDrag();
    });
  }
  canvas.addEventListener("wheel", onWheel, { passive: false });
  canvas.addEventListener("keydown", onKeyDown);
  new ResizeObserver(() => draw()).observe(canvas);
  new ResizeObserver(() => {
    const width = Math.round(canvas.clientWidth);
    if (width > 0) ui.width = Math.min(2400, Math.max(200, width));
  }).observe(canvas);
}

// ---------------------------------------------------------------- channel cards

interface CardEls {
  root: HTMLElement;
  on: HTMLInputElement;
  scale: HTMLInputElement;
  center: HTMLInputElement;
  auto: HTMLInputElement;
  range: HTMLElement;
  active: HTMLInputElement;
  metrics: Record<string, HTMLElement>;
}

const cards: CardEls[] = [];

function buildCards(): void {
  const host = $("channels");
  host.textContent = "";
  cards.length = 0;
  for (let ch = 0; ch < 8; ch++) {
    const root = document.createElement("div");
    root.className = "channel";
    root.style.setProperty("--color", COLORS[ch]);
    root.innerHTML = `
      <div class="channel-head">
        <label><input type="checkbox" data-role="on" /><span class="dot" style="background:${COLORS[ch]}"></span>CH${ch + 1}</label>
        <span class="range" data-role="range">—</span>
        <label title="Активный канал для курсоров напряжения"><input type="radio" name="active-ch" data-role="active" /> изм.</label>
      </div>
      <div class="settings">
        <label>В/дел <input type="number" step="0.05" min="0.0125" max="10" data-role="scale" /></label>
        <label>Центр, В <input type="number" step="0.1" data-role="center" /></label>
        <label><input type="checkbox" data-role="auto" /> авто</label>
      </div>
      <div class="metrics"></div>`;

    const on = root.querySelector<HTMLInputElement>('[data-role="on"]')!;
    const scale = root.querySelector<HTMLInputElement>('[data-role="scale"]')!;
    const center = root.querySelector<HTMLInputElement>('[data-role="center"]')!;
    const auto = root.querySelector<HTMLInputElement>('[data-role="auto"]')!;
    const active = root.querySelector<HTMLInputElement>('[data-role="active"]')!;
    const range = root.querySelector<HTMLElement>('[data-role="range"]')!;
    const metricsHost = root.querySelector<HTMLElement>(".metrics")!;

    // Settings are separated from the numbers on purpose: polling rewrites only
    // the metric spans, so an open select or a half-typed field keeps its focus
    // and its value.
    const metrics: Record<string, HTMLElement> = {};
    const rows: Array<[string, string]> = [
      ["vmin", "Vmin"],
      ["vmax", "Vmax"],
      ["vpp", "Vpp"],
      ["rms", "RMS"],
      ["rms_ac", "RMS AC"],
      ["freq", "Частота"],
      ["duty", "Скважн."],
      ["period", "Период"],
    ];
    for (const [key, label] of rows) {
      const l = document.createElement("span");
      l.className = "label";
      l.textContent = label;
      const v = document.createElement("span");
      v.textContent = "—";
      v.dataset["key"] = key;
      metricsHost.append(l, v);
      metrics[key] = v;
    }
    const peak = document.createElement("span");
    peak.className = "peak";
    peak.textContent = "пики: —";
    const sat = document.createElement("span");
    sat.className = "sat";
    sat.hidden = true;
    const alias = document.createElement("span");
    alias.className = "alias";
    alias.hidden = true;
    metricsHost.append(peak, sat, alias);
    metrics["peak"] = peak;
    metrics["sat"] = sat;
    metrics["alias"] = alias;

    on.addEventListener("change", () => {
      if (on.checked) ui.enabled = [...new Set([...ui.enabled, ch])].sort((a, b) => a - b);
      else ui.enabled = ui.enabled.filter((c) => c !== ch);
      if (ui.enabled.length === 0) {
        ui.enabled = [ch];
        on.checked = true;
      }
      void applyChannels();
    });

    scale.addEventListener("change", () => {
      const v = Number(scale.value);
      if (Number.isFinite(v) && v > 0) void command(`cmd=range_v&ch=${ch + 1}&v=${4 * v}`).catch(fail);
    });
    center.addEventListener("change", () => {
      const v = Number(center.value);
      if (Number.isFinite(v)) void command(`cmd=center&ch=${ch + 1}&v=${v}`).catch(fail);
    });
    auto.addEventListener("change", () => {
      void command(`cmd=autoscale&ch=${ch + 1}&on=${auto.checked ? 1 : 0}`).catch(fail);
    });
    active.addEventListener("change", () => {
      ui.activeCh = ch;
      syncActive();
      draw();
    });

    host.append(root);
    cards.push({ root, on, scale, center, auto, range, active, metrics });
  }
}

function syncActive(): void {
  cards.forEach((card, ch) => {
    card.active.checked = ch === ui.activeCh;
    card.root.classList.toggle("active", ch === ui.activeCh);
  });
}

// -------------------------------------------------------------------- commands

async function applyTimeDiv(id: number): Promise<void> {
  try {
    await command(`cmd=time_div&id=${id}`);
    say(`развёртка ${timeDivLabel(id)}`);
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function toggleRun(): Promise<void> {
  const running = ui.info?.running ?? false;
  try {
    await command(`cmd=run&on=${running ? 0 : 1}`);
    say(running ? "захват остановлен" : "захват запущен");
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function toggleHold(): Promise<void> {
  const on = !(ui.info?.held ?? false);
  try {
    await postJSON(`/api/hold?on=${on ? 1 : 0}`, { on });
    say(on ? "экран удержан" : "экран обновляется");
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function resetPeaks(): Promise<void> {
  try {
    await command("cmd=peaks");
    say("пики сброшены");
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function toggleRec(): Promise<void> {
  const rec = ui.info?.rec ?? false;
  try {
    await command(rec ? "cmd=log_stop" : "cmd=log_start");
    say(rec ? "запись остановлена" : "запись идёт");
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function arm(): Promise<void> {
  try {
    await postJSON("/api/trigger/arm", {});
    say("Single взведён: ждём фронт");
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function applyTrigger(patch: Partial<TriggerCfg>): Promise<void> {
  const base = ui.cfg;
  if (!base) return;
  const next = { ...base, ...patch };
  try {
    const answer = await postJSON<{ ok: boolean; trigger: TriggerCfg }>("/api/trigger", next);
    ui.cfg = answer.trigger;
    syncTriggerFields();
    say("триггер обновлён");
    await poll();
  } catch (e) {
    fail(e);
    syncTriggerFields();
  }
}

async function applyChannels(): Promise<void> {
  try {
    await command(`cmd=channels&list=${ui.enabled.map((c) => c + 1).join(",")}`);
    // Disabling the trigger source must pick a replacement explicitly rather than
    // leaving the server waiting for an edge nobody can produce.
    if (ui.cfg && !ui.enabled.includes(ui.cfg.source)) {
      const next = ui.enabled[0];
      await applyTrigger({ source: next });
      say(`источник триггера переключён на CH${next + 1}`);
    }
    await poll();
  } catch (e) {
    fail(e);
  }
}

function syncTriggerFields(): void {
  const cfg = ui.cfg;
  if (!cfg) return;
  const mode = $<HTMLSelectElement>("trig-mode");
  const edge = $<HTMLSelectElement>("trig-edge");
  const level = $<HTMLInputElement>("trig-level");
  const pre = $<HTMLInputElement>("trig-pre");
  const timeout = $<HTMLInputElement>("trig-timeout");
  const source = $<HTMLSelectElement>("trig-source");

  mode.value = cfg.mode;
  edge.value = cfg.edge;
  if (document.activeElement !== level) level.value = String(Number(cfg.level_v.toFixed(4)));
  if (document.activeElement !== pre) {
    pre.value = String(Math.round(cfg.pretrigger_pct));
    $("trig-pre-out").textContent = `${pre.value}%`;
  }
  if (document.activeElement !== timeout) timeout.value = String(cfg.timeout_ms);

  const wanted = ui.enabled;
  const current = Array.from(source.options).map((o) => Number(o.value));
  if (current.join(",") !== wanted.join(",")) {
    source.textContent = "";
    for (const ch of wanted) {
      const option = document.createElement("option");
      option.value = String(ch);
      option.textContent = `CH${ch + 1}`;
      source.append(option);
    }
  }
  source.value = String(cfg.source);
  paintTriggerHint();
}

/**
 * The one line under the trigger controls. Its job is to answer "why is nothing
 * moving?" before the user has to ask: a level outside the signal, an input that
 * does not change, a window the block was too short for, and then — only when
 * none of those apply — what the selected mode is doing.
 */
function paintTriggerHint(): void {
  const info = ui.info;
  const cap = ui.cap;
  const hint = $("trig-hint");
  const cfg = ui.cfg;
  if (!info || !cfg) return;
  hint.classList.remove("warn");

  const rescue = info.level_rescue;
  if (rescue && rescue.age_ms < 8000) {
    hint.classList.add("warn");
    hint.textContent =
      `Уровень был вне сигнала (${volts(rescue.min_v)}…${volts(rescue.max_v)}) — ` +
      `поставил на середину: ${volts(rescue.level_v)}.`;
    return;
  }

  const span = info.source_span;
  if (span && cfg.mode !== "free") {
    const [lo, hi] = span;
    if (hi - lo < 0.01) {
      hint.classList.add("warn");
      hint.textContent = `Источник CH${cfg.source + 1} не меняется (${volts(lo)}) — фронта не будет.`;
      return;
    }
    if (cfg.level_v < lo || cfg.level_v > hi) {
      hint.classList.add("warn");
      hint.textContent =
        `Уровень ${volts(cfg.level_v)} вне сигнала CH${cfg.source + 1} ` +
        `(${volts(lo)}…${volts(hi)}) — фронта не будет.`;
      return;
    }
  }

  if (cap && (cap.trigger.clipped_pre || cap.trigger.clipped_post)) {
    const side = cap.trigger.clipped_pre ? "до фронта" : "после фронта";
    hint.textContent =
      `Блоку не хватило данных ${side}: окно ${fmt(cap.trigger.pct, 1)}% перед фронтом вместо ` +
      `${fmt(cap.trigger.pretrigger_pct, 0)}%.`;
    return;
  }

  hint.textContent =
    cfg.mode === "single"
      ? "Один кадр на взвод; после захвата экран держится до следующего взвода."
      : cfg.mode === "normal"
        ? "Ждёт фронт и держит последний кадр, пока его нет."
        : cfg.mode === "auto"
          ? `Ждёт фронт до ${cfg.timeout_ms} мс, затем показывает свободный кадр.`
          : "Публикует каждый блок без синхронизации.";
}

// ------------------------------------------------------------------ polling

let polling = false;

async function poll(): Promise<void> {
  if (polling) return;
  polling = true;
  try {
    const info = await getJSON<Info>("/api/info");
    ui.info = info;
    ui.cfg = info.trigger;
    ui.timeDivId = info.time_div_id;

    const width = Math.min(2400, Math.max(200, Math.round(canvas.clientWidth || 1200)));
    if (width !== ui.width) ui.width = width;
    // One answer carries the frame and its metadata together, so whatever was
    // measured belongs to whatever is drawn.
    const cap = await getJSON<Capture>(`/api/capture/v2?width=${width}`);
    ui.cap = cap;
    ui.columns = decodeColumns(cap);
    if (cap.channels.length && !cap.channels.includes(ui.activeCh)) {
      ui.activeCh = cap.channels[0];
    }
    ui.enabled = [...cap.channels];
    syncControls(info, cap);
    paintCards(cap);
    draw();
  } catch (e) {
    // The last frame stays on screen and the status says why: losing the link
    // must not erase the measurement the user was reading.
    fail(e);
    draw();
  } finally {
    polling = false;
  }
}

function decodeColumns(cap: Capture): Int16Array | null {
  if (!cap.columns_b64) return null;
  const raw = atob(cap.columns_b64);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  // The server writes little-endian i16; a plain Int16Array needs the host's
  // byte order, which on Windows and in every browser we ship for is little-endian
  // but is not guaranteed by the language.
  const view = new DataView(bytes.buffer);
  const out = new Int16Array(bytes.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = view.getInt16(i * 2, true);
  return out;
}

function syncControls(info: Info, cap: Capture): void {
  const run = $("run");
  run.textContent = info.running ? "Стоп" : "Пуск";
  run.classList.toggle("live", info.running);

  const hold = $("hold");
  hold.setAttribute("aria-pressed", String(info.held));
  hold.textContent = info.held ? "Вернуть экран" : "Удержать экран";

  const rec = $("rec");
  rec.setAttribute("aria-pressed", String(info.rec));
  rec.textContent = info.rec ? "Остановить запись" : "Запись";

  $("pill-rec").textContent = info.rec ? "REC идёт" : "REC выкл";
  $("pill-rec").className = `pill${info.rec ? " rec" : ""}`;

  const link = $("pill-link");
  if (info.error) {
    link.textContent = "нет связи";
    link.className = "pill bad";
    link.title = info.error;
  } else {
    link.textContent = info.running ? "прибор" : "прибор · стоп";
    link.className = `pill${info.running ? " ok" : ""}`;
    link.title = info.link;
  }

  $("pill-rate").textContent = `${fmt(cap.sample_rate / 1000, 1)} квыб/с · ${fmt(info.blocks_per_s, 1)} блок/с`;
  $("subtitle").textContent = `${cap.channels.length} ${channelWord(cap.channels.length)} · 12 бит · ${info.time_div}`;

  const timeSelect = $<HTMLSelectElement>("time-div");
  if (document.activeElement !== timeSelect) timeSelect.value = String(info.time_div_id);

  const windowInfo = $("window-info");
  const actual = cap.window_s;
  const requested = nsPerDiv(info.time_div_id) * 10 * 1e-9;
  windowInfo.textContent =
    info.window_clamped || actual + 1e-12 < requested
      ? `окно ${seconds(actual)} (запрошено ${seconds(requested)})`
      : `окно ${seconds(actual)} · ${timeDivLabel(info.time_div_id)}`;

  $("trigger-state").innerHTML = `<b>${stateLabel(info, cap)}</b>`;
  syncTriggerFields();

  const status = $("status");
  if (ui.error) {
    status.textContent = `ошибка: ${ui.error}`;
    status.className = "status error";
  } else {
    const bits = [
      stateLabel(info, cap),
      `кадр ${cap.frame_id}`,
      info.held ? "экран удержан" : "",
      ui.message,
    ].filter(Boolean);
    status.textContent = bits.join(" · ");
    status.className = "status";
  }
}

// --------------------------------------------------------------------- cards

function paintCards(cap: Capture): void {
  cards.forEach((card, ch) => {
    const on = cap.channels.includes(ch);
    card.root.classList.toggle("off", !on);
    if (card.on.checked !== on && document.activeElement !== card.on) card.on.checked = on;

    const meta = cap.channel_meta.find((m) => m.ch === ch);
    const stat = cap.stats.find((s) => s.ch === ch);
    if (meta) {
      const vpd = Math.max(0.0125, meta.full_scale_v / 4);
      if (document.activeElement !== card.scale) {
        card.scale.value = String(Number(vpd.toFixed(4)));
      }
      if (document.activeElement !== card.center) {
        card.center.value = String(Number(meta.center_v.toFixed(3)));
      }
      if (document.activeElement !== card.auto) card.auto.checked = meta.autoscale;
      card.range.textContent = HW_RANGE[meta.range_id - 1] ?? "—";
    } else {
      card.range.textContent = "—";
    }

    const set = (key: string, text: string) => {
      card.metrics[key].textContent = text;
    };
    if (on && stat) {
      set("vmin", volts(stat.vmin));
      set("vmax", volts(stat.vmax));
      set("vpp", volts(stat.vpp));
      set("rms", volts(stat.rms));
      set("rms_ac", volts(stat.rms_ac));
      // A frequency read off an aliased frame is arithmetic on unrelated points.
      // "≈" says the number is there and is not to be trusted.
      set("freq", stat.aliased ? `≈ ${hz(stat.freq)}` : hz(stat.freq));
      set("duty", stat.duty === null ? "—" : `${stat.aliased ? "≈ " : ""}${fmt(stat.duty * 100, 1)} %`);
      set("period", stat.period_s === null ? "—" : seconds(stat.period_s));
      const peakLow = meta?.peak_min ?? null;
      const peakHigh = meta?.peak_max ?? null;
      set("peak", `пики за сеанс: ${volts(peakLow)} … ${volts(peakHigh)}`);
      const sat = card.metrics["sat"];
      sat.hidden = !(meta?.saturated ?? false);
      sat.textContent = meta?.saturated ? "АЦП на пределе" : "";
      const alias = card.metrics["alias"];
      alias.hidden = !stat.aliased;
      alias.textContent = stat.aliased
        ? `мало точек: ${fmt(stat.points_per_period, 1)} отсчёта на период — нужна развёртка быстрее`
        : "";
    } else {
      for (const key of ["vmin", "vmax", "vpp", "rms", "rms_ac", "freq", "duty", "period"]) {
        set(key, "—");
      }
      set("peak", "пики: —");
      card.metrics["sat"].hidden = true;
      card.metrics["alias"].hidden = true;
    }
  });
}

// ---------------------------------------------------------------------- draw

function draw(): void {
  if (!canvas || !ctx) return;
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (w <= 0 || h <= 0) return;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = "#0d1016";
  ctx.fillRect(0, 0, w, h);
  ctx.font = "11px ui-monospace, 'Cascadia Mono', monospace";
  ctx.textBaseline = "alphabetic";

  const cap = ui.cap;
  const channels = cap?.channels ?? ui.enabled;
  const left = 88;
  const right = w - 42;
  const top = 14;
  const bottom = h - 42;
  const pw = right - left;
  const ph = bottom - top;
  const n = Math.max(1, channels.length);
  const laneH = ph / n;
  geom.left = left;
  geom.right = right;
  geom.pw = pw;
  geom.top = top;
  geom.bottom = bottom;

  layout = channels.map((ch, k) => {
    const y = top + k * laneH;
    const meta = cap?.channel_meta.find((m) => m.ch === ch);
    const stat = cap?.stats.find((s) => s.ch === ch);
    const auto = meta?.autoscale ?? false;
    const span = stat && stat.vmin !== null && stat.vmax !== null ? stat.vmax - stat.vmin : 0;
    const vpd = auto && span > 0 ? Math.max(0.0125, span / 6) : Math.max(0.0125, (meta?.full_scale_v ?? 20) / 4);
    const center = auto && stat && stat.vmin !== null && stat.vmax !== null ? (stat.vmin + stat.vmax) / 2 : (meta?.center_v ?? 0);
    const mid = y + laneH / 2;
    const yV = (v: number) => mid - ((v - center) * laneH) / (8 * vpd);
    const vY = (yy: number) => center + ((mid - yy) * 8 * vpd) / laneH;
    return { ch, y, lane: laneH, mid, vpd, center, yV, vY };
  });

  // Lanes: background, grid, labels.
  layout.forEach((lane, k) => {
    const color = COLORS[lane.ch] ?? "#fff";
    ctx.fillStyle = k % 2 ? "#10141b" : "#0e1219";
    ctx.fillRect(left, lane.y, pw, lane.lane);

    for (let d = 0; d <= 8; d++) {
      const yy = lane.y + (d * lane.lane) / 8;
      ctx.strokeStyle = d === 4 ? "#323b4c" : "#222936";
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(left, Math.round(yy) + 0.5);
      ctx.lineTo(right, Math.round(yy) + 0.5);
      ctx.stroke();
    }
    for (let d = 0; d <= 10; d++) {
      const xx = left + (d * pw) / 10;
      ctx.strokeStyle = "#242b38";
      ctx.beginPath();
      ctx.moveTo(Math.round(xx) + 0.5, lane.y);
      ctx.lineTo(Math.round(xx) + 0.5, lane.y + lane.lane);
      ctx.stroke();
    }

    // Voltage labels: every division when there is room, every other or every
    // fourth when eight lanes squeeze the height. The bottom label belongs to the
    // last lane only — otherwise it collides with the next lane's channel name.
    const step = lane.lane >= 170 ? 1 : lane.lane >= 60 ? 2 : 4;
    const lastLane = k === layout.length - 1;
    ctx.textAlign = "right";
    for (let d = 0; d <= 8; d += step) {
      if (d === 8 && !lastLane) continue;
      const value = lane.center + (4 - d) * lane.vpd;
      const yy = lane.y + (d * lane.lane) / 8 + 3.5;
      ctx.fillStyle = color;
      ctx.fillText(volts(value, 2), left - 8, yy);
    }

    // Channel name and scale live inside the plot: the gutter is entirely the
    // voltage scale, so nothing can overlap a tick label.
    ctx.textAlign = "left";
    ctx.fillStyle = color;
    ctx.fillText(`CH${lane.ch + 1}`, left + 6, lane.y + 14);
    ctx.fillStyle = "#7f8899";
    ctx.fillText(`${volts(lane.vpd, 2)}/дел`, left + 6, lane.y + 28);

    // Zero volts, where it actually is; an arrow when it is off the lane.
    const zeroY = lane.yV(0);
    if (zeroY >= lane.y && zeroY <= lane.y + lane.lane) {
      ctx.strokeStyle = `${color}66`;
      ctx.setLineDash([4, 5]);
      ctx.beginPath();
      ctx.moveTo(left, Math.round(zeroY) + 0.5);
      ctx.lineTo(right, Math.round(zeroY) + 0.5);
      ctx.stroke();
      ctx.setLineDash([]);
    } else {
      const up = zeroY < lane.y;
      const yy = up ? lane.y + 8 : lane.y + lane.lane - 4;
      ctx.fillStyle = `${color}aa`;
      ctx.textAlign = "left";
      ctx.fillText(up ? `↓ 0 В` : `↑ 0 В`, left + 4, yy);
    }
  });

  // Traces.
  if (cap && ui.columns) {
    const columns = ui.columns;
    const width = cap.width;
    layout.forEach((lane, k) => {
      const color = COLORS[lane.ch] ?? "#fff";
      ctx.save();
      ctx.beginPath();
      ctx.rect(left, lane.y, pw, lane.lane);
      ctx.clip();

      // Peaks are cumulative and therefore drawn apart from the trace.
      const meta = cap.channel_meta.find((m) => m.ch === lane.ch);
      for (const peak of [meta?.peak_min ?? null, meta?.peak_max ?? null]) {
        if (peak === null) continue;
        const yy = lane.yV(peak);
        if (yy < lane.y - 2 || yy > lane.y + lane.lane + 2) continue;
        ctx.strokeStyle = `${color}4d`;
        ctx.setLineDash([2, 5]);
        ctx.beginPath();
        ctx.moveTo(left, Math.round(yy) + 0.5);
        ctx.lineTo(right, Math.round(yy) + 0.5);
        ctx.stroke();
        ctx.setLineDash([]);
      }

      ctx.strokeStyle = color;
      ctx.lineWidth = 1.3;
      ctx.beginPath();
      const base = k * width * 2;
      let previous: { x: number; y: number } | null = null;
      for (let col = 0; col < width; col++) {
        const lo = columns[base + col * 2] / 1000;
        const hi = columns[base + col * 2 + 1] / 1000;
        if (!(lo <= hi)) {
          // No sample in this column: the trace simply continues from the last
          // real point, it never dips to zero.
          continue;
        }
        const x = left + ((col + 0.5) * pw) / width;
        const yLo = lane.yV(lo);
        const yHi = lane.yV(hi);
        ctx.moveTo(x, yHi);
        ctx.lineTo(x, Math.max(yLo, yHi + 0.6));
        if (previous) {
          // Join consecutive samples across gaps, which is what "connect the raw
          // points" means for a window narrower than the plot.
          ctx.moveTo(previous.x, previous.y);
          ctx.lineTo(x, (yLo + yHi) / 2);
        }
        previous = { x, y: (yLo + yHi) / 2 };
      }
      ctx.stroke();
      ctx.lineWidth = 1;
      ctx.restore();

      const metaSat = cap.channel_meta.find((m) => m.ch === lane.ch);
      if (metaSat?.saturated) {
        ctx.fillStyle = "#ffd16c";
        ctx.textAlign = "right";
        ctx.fillText("АЦП ▲", right - 2, lane.y + 12);
      }
    });
  } else {
    ctx.fillStyle = "#c6cdd8";
    ctx.textAlign = "center";
    ctx.fillText("Ждём кадр от прибора", left + pw / 2, top + ph / 2);
  }

  // Trigger level and T0.
  if (cap) {
    const trig = cap.trigger;
    // The marker is a control, so it shows the level the instrument will use on
    // the next block. The frame's own level is only a fallback: a held or stale
    // frame keeps the level it was captured with, and drawing that while the
    // panel shows the current one made the two disagree on screen.
    const shownLevel = ui.levelPreview ?? ui.cfg?.level_v ?? trig.level_v;
    const lane = layout.find((l) => l.ch === (ui.cfg?.source ?? trig.source));
    if (lane && (ui.cfg?.mode ?? trig.mode) !== "free") {
      const level = shownLevel;
      const yLevel = lane.yV(level);
      const inside = yLevel >= lane.y && yLevel <= lane.y + lane.lane;
      const yy = Math.min(lane.y + lane.lane - 10, Math.max(lane.y + 10, yLevel));
      if (inside) {
        ctx.strokeStyle = "#ff6a70";
        ctx.setLineDash([5, 4]);
        ctx.beginPath();
        ctx.moveTo(left, Math.round(yLevel) + 0.5);
        ctx.lineTo(right, Math.round(yLevel) + 0.5);
        ctx.stroke();
        ctx.setLineDash([]);
      } else {
        ctx.fillStyle = "#ff9095";
        ctx.textAlign = "right";
        ctx.fillText(`${yLevel < lane.y ? "↑" : "↓"} ${volts(level, 2)}`, right - 6, yLevel < lane.y ? lane.y + 12 : lane.y + lane.lane - 4);
      }
      ctx.fillStyle = "#c83345";
      ctx.fillRect(right, yy - 10, 30, 20);
      ctx.fillStyle = "#fff";
      ctx.textAlign = "center";
      ctx.fillText(trig.edge === "rise" ? "T↑" : "T↓", right + 15, yy + 4);
    }

    if (!trig.free && cap.window_s > 0) {
      const x0 = left + (trig.t0_s / cap.window_s) * pw;
      if (x0 >= left && x0 <= right) {
        ctx.strokeStyle = "#ff777c";
        ctx.setLineDash([4, 5]);
        ctx.beginPath();
        ctx.moveTo(Math.round(x0) + 0.5, top);
        ctx.lineTo(Math.round(x0) + 0.5, bottom);
        ctx.stroke();
        ctx.setLineDash([]);
        ctx.fillStyle = "#ff9095";
        ctx.textAlign = "center";
        ctx.fillText("T₀", x0, top - 3);
      }
    }
  }

  // Time axis.
  if (cap && cap.window_s > 0) {
    const t0 = cap.trigger.free ? 0 : cap.trigger.t0_s;
    const unit = axisUnit(cap.window_s);
    ctx.textAlign = "center";
    ctx.fillStyle = "#a5aebd";
    const stride = w < 620 ? 2 : 1;
    for (let d = 0; d <= 10; d += stride) {
      const t = (d / 10) * cap.window_s - t0;
      const x = left + (d * pw) / 10;
      ctx.fillText(fmt(t / unit.scale, 2), x, bottom + 16);
    }
    ctx.textAlign = "left";
    ctx.fillStyle = "#7f8899";
    ctx.fillText(unit.suffix, left - 4, bottom + 32);
    ctx.textAlign = "right";
    ctx.fillText(`окно ${seconds(cap.window_s)} · ${timeDivLabel(ui.timeDivId)}`, right, bottom + 32);
  }

  // Cursors.
  const mode = ui.cursors.mode;
  if (mode === "time") {
    for (const [name, f] of [
      ["A", ui.cursors.a],
      ["B", ui.cursors.b],
    ] as Array<[string, number]>) {
      const x = left + f * pw;
      ctx.strokeStyle = "#bda2ff";
      ctx.setLineDash([4, 4]);
      ctx.beginPath();
      ctx.moveTo(Math.round(x) + 0.5, top);
      ctx.lineTo(Math.round(x) + 0.5, bottom);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = "#dfcaff";
      ctx.textAlign = "center";
      ctx.fillText(name, x, top - 3);
    }
  } else if (mode === "voltage") {
    const lane = layout.find((l) => l.ch === ui.activeCh) ?? layout[0];
    if (lane) {
      for (const [name, f] of [
        ["A", ui.cursors.a],
        ["B", ui.cursors.b],
      ] as Array<[string, number]>) {
        const y = lane.y + 2 + f * (lane.lane - 4);
        ctx.strokeStyle = "#c7a1ff";
        ctx.setLineDash([5, 4]);
        ctx.beginPath();
        ctx.moveTo(left, Math.round(y) + 0.5);
        ctx.lineTo(right, Math.round(y) + 0.5);
        ctx.stroke();
        ctx.setLineDash([]);
        ctx.fillStyle = "#dfcaff";
        ctx.textAlign = "left";
        ctx.fillText(name, left + 5, y - 3);
      }
    }
  }

  paintReadout(cap);
}

function paintReadout(cap: Capture | null): void {
  const host = $("readout");
  const bits: string[] = [];
  if (!cap) {
    host.textContent = "нет кадра";
    return;
  }
  const mode = ui.cursors.mode;
  if (mode === "time") {
    const dt = (ui.cursors.b - ui.cursors.a) * cap.window_s;
    bits.push(`<span><b>Δt</b> ${seconds(dt)}</span>`);
    bits.push(`<span><b>1/|Δt|</b> ${dt !== 0 ? hz(1 / Math.abs(dt)) : "—"}</span>`);
  } else if (mode === "voltage") {
    const lane = layout.find((l) => l.ch === ui.activeCh) ?? layout[0];
    if (lane) {
      const va = lane.vY(lane.y + 2 + ui.cursors.a * (lane.lane - 4));
      const vb = lane.vY(lane.y + 2 + ui.cursors.b * (lane.lane - 4));
      bits.push(`<span><b>A</b> ${volts(va)}</span>`);
      bits.push(`<span><b>B</b> ${volts(vb)}</span>`);
      bits.push(`<span><b>ΔV</b> ${volts(va - vb)}</span>`);
      bits.push(`<span><b>CH${lane.ch + 1}</b></span>`);
    }
  }
  bits.push(`<span><b>кадр</b> ${cap.frame_id}</span>`);
  bits.push(`<span><b>выборка</b> ${hz(cap.sample_rate)}</span>`);
  bits.push(`<span><b>отсчётов</b> ${fmt(cap.window_samples, 0)}</span>`);
  bits.push(`<span><b>окно</b> ${seconds(cap.window_s)}</span>`);
  const dropouts = cap.channel_meta.reduce((sum, m) => sum + (m.dropouts ?? 0), 0);
  if (dropouts > 0) bits.push(`<span><b>сбоев АЦП</b> ${dropouts}</span>`);
  const aliased = cap.stats.filter((s) => s.aliased);
  if (aliased.length > 0) {
    bits.push(
      `<span class="warn"><b>мало точек</b> ${aliased.map((s) => `CH${s.ch + 1}`).join(", ")}</span>`,
    );
  }
  if (cap.trigger.clipped_pre || cap.trigger.clipped_post) {
    bits.push(`<span><b>T₀</b> ${fmt(cap.trigger.pct, 1)}% (обрезано)</span>`);
  } else if (!cap.trigger.free) {
    bits.push(`<span><b>T₀</b> ${fmt(cap.trigger.pct, 1)}%</span>`);
  }
  host.innerHTML = bits.join("");
}

// --------------------------------------------------------------- interaction

function canvasPoint(event: PointerEvent): { x: number; y: number } {
  const rect = canvas.getBoundingClientRect();
  return { x: event.clientX - rect.left, y: event.clientY - rect.top };
}

function onPointerDown(event: PointerEvent): void {
  const cap = ui.cap;
  if (!cap) return;
  const { x, y } = canvasPoint(event);
  const { left, right, pw } = geom;

  const sourceLane = layout.find((l) => l.ch === cap.trigger.source);
  if (sourceLane && cap.trigger.mode !== "free" && x >= right - 12 && x <= right + 34) {
    ui.drag = "level";
    canvas.setPointerCapture(event.pointerId);
    onPointerMove(event);
    return;
  }

  if (x >= left && x <= right) {
    if (ui.cursors.mode === "time") {
      const f = (x - left) / pw;
      ui.drag = Math.abs(f - ui.cursors.a) <= Math.abs(f - ui.cursors.b) ? "a" : "b";
      ui.dragTarget = ui.drag;
      canvas.setPointerCapture(event.pointerId);
      onPointerMove(event);
      return;
    }
    if (ui.cursors.mode === "voltage") {
      const lane = layout.find((l) => l.ch === ui.activeCh) ?? layout[0];
      if (lane && y >= lane.y && y <= lane.y + lane.lane) {
        const f = Math.min(1, Math.max(0, (y - lane.y - 2) / (lane.lane - 4)));
        ui.drag = Math.abs(f - ui.cursors.a) <= Math.abs(f - ui.cursors.b) ? "a" : "b";
        ui.dragTarget = ui.drag;
        canvas.setPointerCapture(event.pointerId);
        onPointerMove(event);
        return;
      }
    }
  }

  // Clicking a lane makes it the one the voltage cursors measure.
  const lane = layout.find((l) => y >= l.y && y <= l.y + l.lane);
  if (lane && lane.ch !== ui.activeCh) {
    ui.activeCh = lane.ch;
    syncActive();
    draw();
  }
  canvas.focus();
}

function onPointerMove(event: PointerEvent): void {
  const cap = ui.cap;
  if (!cap || !ui.drag) return;
  const { x, y } = canvasPoint(event);
  const { left, pw } = geom;

  if (ui.drag === "level") {
    const lane = layout.find((l) => l.ch === cap.trigger.source);
    if (!lane) return;
    const clamped = Math.min(lane.y + lane.lane, Math.max(lane.y, y));
    // Local preview only: the server confirms the value on release, so a drag
    // never spams the command endpoint with half-finished numbers.
    ui.levelPreview = lane.vY(clamped);
    draw();
    return;
  }
  const f = Math.min(1, Math.max(0, (x - left) / pw));
  if (ui.cursors.mode === "time") {
    if (ui.drag === "a") ui.cursors.a = f;
    else ui.cursors.b = f;
    draw();
    return;
  }
  if (ui.cursors.mode === "voltage") {
    const lane = layout.find((l) => l.ch === ui.activeCh) ?? layout[0];
    if (!lane) return;
    const local = Math.min(1, Math.max(0, (y - lane.y - 2) / (lane.lane - 4)));
    if (ui.drag === "a") ui.cursors.a = local;
    else ui.cursors.b = local;
    draw();
  }
}

function finishDrag(): void {
  if (ui.drag === "level") {
    const level = ui.levelPreview;
    ui.levelPreview = null;
    ui.drag = null;
    if (level !== null && Number.isFinite(level)) {
      const rounded = Number(level.toFixed(4));
      const input = $<HTMLInputElement>("trig-level");
      input.value = String(rounded);
      void applyTrigger({ level_v: rounded });
      return;
    }
  }
  ui.drag = null;
  draw();
}

let wheelAccum = 0;
let wheelStepAt = 0;

function onWheel(event: WheelEvent): void {
  event.preventDefault();
  const now = performance.now();
  const pixels = event.deltaMode === 1 ? event.deltaY * 16 : event.deltaY;
  // A trackpad flick arrives as dozens of events, momentum included, and this
  // handler used to step once per event: one gesture raced the base from
  // 10 ms/div to 1 ns/div and left the screen looking broken. A step now needs a
  // notch's worth of scrolling, a pause since the last one, and a delta big
  // enough not to be the tail of a gesture.
  if (Math.abs(pixels) < 8) return;
  if (now - wheelStepAt < 250) return;
  wheelAccum += pixels;
  if (Math.abs(wheelAccum) < 80) return;
  const dir = Math.sign(wheelAccum);
  wheelAccum = 0;
  wheelStepAt = now;
  const id = Math.min(25, Math.max(0, ui.timeDivId + dir));
  if (id === ui.timeDivId) return;
  ui.timeDivId = id;
  $<HTMLSelectElement>("time-div").value = String(id);
  void applyTimeDiv(id);
}

function onKeyDown(event: KeyboardEvent): void {
  const step = event.shiftKey ? 0.05 : 0.01;
  if (event.key === "a" || event.key === "A") ui.dragTarget = "a";
  if (event.key === "b" || event.key === "B") ui.dragTarget = "b";
  if (event.key === "Escape") {
    ui.drag = null;
    ui.levelPreview = null;
    draw();
    return;
  }
  if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
  event.preventDefault();
  const active = ui.dragTarget;
  const delta = (event.key === "ArrowRight" ? 1 : -1) * step;
  ui.cursors[active] = Math.min(1, Math.max(0, ui.cursors[active] + delta));
  draw();
}

/** Which cursor the arrow keys move; A until the user picks B with a click. */

// ---------------------------------------------------------------- exports

function download(name: string, blob: Blob): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 3000);
}

function stamp(): string {
  return new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
}

function downloadFrameCsv(): void {
  const id = ui.cap?.frame_id;
  if (!id) {
    fail("кадр ещё не получен");
    return;
  }
  const link = document.createElement("a");
  link.href = `/api/frame.csv?frame_id=${id}`;
  link.download = `davinci-frame-${id}.csv`;
  document.body.append(link);
  link.click();
  link.remove();
}

function downloadHistoryCsv(): void {
  const link = document.createElement("a");
  link.href = "/api/export.csv";
  link.download = `davinci-history-${stamp()}.csv`;
  document.body.append(link);
  link.click();
  link.remove();
}

function downloadPng(): void {
  canvas.toBlob((blob) => {
    if (blob) download(`davinci-${ui.cap?.frame_id ?? 0}-${stamp()}.png`, blob);
    else fail("PNG не собрался");
  }, "image/png");
}

// --------------------------------------------------------------------- boot

async function boot(): Promise<void> {
  buildShell();
  syncActive();
  draw();
  try {
    const first = await getJSON<Info>("/api/info");
    ui.info = first;
    ui.cfg = first.trigger;
    ui.timeDivId = first.time_div_id;
    if (first.trigger.source >= 0) ui.activeCh = first.trigger.source;
  } catch (e) {
    fail(e);
  }
  await poll();

  // One request in flight at a time; a slow USB block must not queue up polls.
  const tick = async (): Promise<void> => {
    if (!document.hidden) await poll();
    setTimeout(() => void tick(), 90);
  };
  void tick();
}

void boot();
