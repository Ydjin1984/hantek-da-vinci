// Mini scope: three live traces in a window small enough to keep beside a
// diagnostic program.
//
// This page is a *reader*, not a second controller. It only asks the same HTTP
// server the main window uses for `/api/info`, `/api/channels` and
// `/api/capture/v2`; it never sends a command, so opening or closing it cannot
// reconnect the instrument, reset a range or interrupt a recording. The engine
// keeps running in the process behind both windows.
//
// Drawing is decoupled from acquisition: the poll may answer at any rate, while
// at most one repaint per display frame is queued. A flat signal still gets a
// line (the min/max column is widened to a visible pixel) and its real Vpp.

import "./mini.css";

interface TriggerCfg {
  mode: string;
  source: number;
  edge: string;
  level_v: number;
  pretrigger_pct: number;
}

interface Info {
  running: boolean;
  link_state: "live" | "reconnecting" | "offline";
  state: string;
  time_div: string;
  time_div_id: number;
  window_s: number;
  last_block_age_ms: number | null;
  frame_age_ms: number | null;
  error: string | null;
  trigger: TriggerCfg;
}

interface ChannelMeta {
  ch: number;
  full_scale_v: number;
  center_v: number;
  autoscale: boolean;
  saturated: boolean;
  dropouts: number;
}

interface Stat {
  ch: number;
  vmin: number | null;
  vmax: number | null;
  vpp: number | null;
  mean: number | null;
  freq: number | null;
  /** Share of sample pairs leaping more than half the swing. */
  jump_fraction: number;
  aliased: boolean;
}

interface Capture {
  frame_id: number;
  channels: number[];
  channel_meta: ChannelMeta[];
  stats: Stat[];
  width: number;
  columns_b64: string;
  window_s: number;
}

interface ChannelInfo {
  ch: number;
  enabled: boolean;
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

const SLOTS = 3;
/** Older than this, the last block is not "live" any more. Matches the main UI. */
const STALE_MS = 3000;
const POS_KEY = "davinci.mini.position";

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

/** 100 % minus the share of sample pairs that leap more than half the swing. */
function purity(stat: Stat | undefined): string {
  if (!stat || !Number.isFinite(stat.jump_fraction)) return "—";
  const p = Math.round((1 - Math.min(1, Math.max(0, stat.jump_fraction))) * 100);
  return `${p} %`;
}

class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function getJSON<T>(url: string, timeoutMs = 4000): Promise<T> {
  const ctrl = new AbortController();
  const timer = window.setTimeout(() => ctrl.abort(), timeoutMs);
  try {
    const r = await fetch(url, { cache: "no-store", signal: ctrl.signal });
    if (!r.ok) {
      let reason = `${r.status}`;
      try {
        const body = (await r.json()) as { error?: string };
        if (body.error) reason = body.error;
      } catch {
        /* not JSON */
      }
      throw new ApiError(r.status, reason);
    }
    return (await r.json()) as T;
  } catch (e) {
    if ((e as Error).name === "AbortError") throw new ApiError(0, "нет ответа от программы");
    throw e;
  } finally {
    window.clearTimeout(timer);
  }
}

interface SlotEls {
  root: HTMLElement;
  name: HTMLElement;
  dot: HTMLElement;
  level: HTMLElement;
  vpp: HTMLElement;
  freq: HTMLElement;
  clean: HTMLElement;
  canvas: HTMLCanvasElement;
}

const slots: SlotEls[] = [];

/** Current truth from the server, replaced only by a complete answer. */
let info: Info | null = null;
let cap: Capture | null = null;
let columns: Int16Array | null = null;
/** Enabled channels from the last frame; the order the main window uses. */
let enabled: number[] = [];
/** Fallback before the first frame exists (or while the instrument is absent). */
let channelList: number[] = [];

let polling = false;
let pollDelay = 120;
let lastFrameKey = "none";
let lastOkAt = performance.now();
let lastError = "";
let drawQueued = false;

function buildShell(): void {
  const app = $("mini-app");
  app.innerHTML = `
  <header class="mini-head">
    <div class="mini-brand"><span class="logo" aria-hidden="true">∿</span><span>DaVinci Scope</span></div>
    <button id="back" type="button" title="Вернуться в основное окно">Вернуться в основное окно</button>
  </header>
  <main id="graphs"></main>`;

  const host = $("graphs");
  for (let i = 0; i < SLOTS; i++) {
    const root = document.createElement("section");
    root.className = "mini";
    root.innerHTML = `
      <div class="mini-top">
        <span class="mini-ch"><span class="dot"></span><b data-role="name">—</b></span>
        <span class="mini-read"><span data-role="level">U —</span><span class="sep">·</span><span data-role="vpp">Vpp —</span></span>
      </div>
      <div class="mini-sub">
        <span data-role="freq">f —</span>
        <span data-role="clean">чистота —</span>
      </div>
      <canvas></canvas>`;
    const name = root.querySelector<HTMLElement>('[data-role="name"]')!;
    const dot = root.querySelector<HTMLElement>(".dot")!;
    const level = root.querySelector<HTMLElement>('[data-role="level"]')!;
    const vpp = root.querySelector<HTMLElement>('[data-role="vpp"]')!;
    const freq = root.querySelector<HTMLElement>('[data-role="freq"]')!;
    const clean = root.querySelector<HTMLElement>('[data-role="clean"]')!;
    const canvas = root.querySelector<HTMLCanvasElement>("canvas")!;
    host.append(root);
    slots.push({ root, name, dot, level, vpp, freq, clean, canvas });
    new ResizeObserver(() => scheduleDraw()).observe(canvas);
  }

  $("back").addEventListener("click", goBack);
}

type IpcWindow = Window & { ipc?: { postMessage(message: string): void } };

/**
 * Back means "the small window goes away". In the native program the Rust side
 * restores the main window; a browser fallback only has its own window to close.
 */
function goBack(): void {
  const ipc = (window as IpcWindow).ipc;
  if (ipc) {
    ipc.postMessage("mini-close");
    return;
  }
  // Document Picture-in-Picture hosts this page in an iframe; the opener owns
  // the PiP window and listens for this message.
  if (window.parent !== window) {
    window.parent.postMessage("davinci-mini-close", location.origin);
    return;
  }
  window.close();
}

/** Browser fallback position memory; the native window has its own file. */
function savePosition(): void {
  try {
    localStorage.setItem(
      POS_KEY,
      JSON.stringify({ x: Math.round(window.screenX), y: Math.round(window.screenY) }),
    );
  } catch {
    /* storage can be disabled; the position is a convenience, not a feature */
  }
}

function plotWidth(): number {
  const css = slots[0]?.canvas.clientWidth ?? 300;
  return Math.min(600, Math.max(48, Math.round(css)));
}

function scheduleDraw(): void {
  if (drawQueued) return;
  drawQueued = true;
  requestAnimationFrame(() => {
    drawQueued = false;
    drawAll();
  });
}

function decodeColumns(c: Capture): Int16Array | null {
  if (!c.columns_b64) return null;
  const raw = atob(c.columns_b64);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  const view = new DataView(bytes.buffer);
  const out = new Int16Array(bytes.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = view.getInt16(i * 2, true);
  return out;
}

async function refreshChannels(): Promise<void> {
  try {
    const list = await getJSON<ChannelInfo[]>("/api/channels");
    channelList = list.filter((c) => c.enabled).map((c) => c.ch - 1);
    // No frame yet (the instrument is absent): the channel order still exists
    // in the engine's settings, and the empty slots must say "Отключено", not
    // "Канал недоступен". Once a frame arrives it replaces this list.
    if (!cap) enabled = [...channelList];
  } catch {
    // The first frame will carry the same list; an absent device is not an
    // error here, it is the state the labels are about to show.
  }
}

function channelsNow(): number[] {
  return cap?.channels ?? (enabled.length > 0 ? enabled : channelList);
}

function slotChannel(slot: number): number | null {
  return channelsNow()[slot] ?? null;
}

type SlotState = "ok" | "unavailable" | "offline" | "nodata" | "stopped" | "stale";

function slotState(slot: number): { kind: SlotState; text: string } {
  if (slotChannel(slot) === null) {
    // The frame may still be from the old channel set while the main window
    // applies a change; say what is true for this slot instead of drawing a
    // trace that does not belong there.
    return { kind: "unavailable", text: "Канал недоступен" };
  }
  if (info?.link_state === "offline") return { kind: "offline", text: "Отключено" };
  if (info?.link_state === "reconnecting") return { kind: "nodata", text: "Нет данных" };
  // A failed poll is also "data stopped arriving", even if the last successful
  // /api/info looked healthy a second ago.
  if (lastError) return { kind: "nodata", text: "Нет данных" };
  if (info && !info.running) return { kind: "stopped", text: "Захват остановлен" };
  if (!cap || !columns) return { kind: "nodata", text: "Нет данных" };
  const age = info?.last_block_age_ms ?? null;
  if (age !== null && age > STALE_MS) return { kind: "stale", text: "Нет данных" };
  return { kind: "ok", text: "" };
}

function updateLabels(): void {
  for (let i = 0; i < slots.length; i++) {
    const slot = slots[i];
    const ch = slotChannel(i);
    const state = slotState(i);
    slot.root.dataset["state"] = state.kind;

    if (ch === null) {
      slot.name.textContent = "—";
      slot.dot.style.background = "#3a414f";
      slot.level.textContent = "U —";
      slot.vpp.textContent = "Vpp —";
      slot.freq.textContent = "f —";
      slot.clean.textContent = "Канал недоступен";
      slot.level.className = "";
      slot.vpp.className = "";
      slot.freq.className = "";
      slot.clean.className = "";
      continue;
    }

    const stat = cap?.stats.find((s) => s.ch === ch);
    const meta = cap?.channel_meta.find((m) => m.ch === ch);
    slot.name.textContent = `CH${ch + 1}`;
    slot.name.style.color = COLORS[ch] ?? "#fff";
    slot.dot.style.background = COLORS[ch] ?? "#fff";

    if (state.kind === "ok") {
      // Vpp answers "how big is the swing"; U answers "at what voltage the
      // signal actually sits". A 14 V rail with 150 mV of ripple is both, and
      // showing only Vpp made it look like the input was at 150 mV.
      slot.level.textContent = `U ${volts(stat?.mean)}`;
      slot.vpp.textContent = `Vpp ${volts(stat?.vpp)}`;
      const f = stat?.freq ?? null;
      slot.freq.textContent = `f ${stat?.aliased && f !== null ? "≈ " : ""}${hz(f)}`;

      const marks: string[] = [];
      if (stat?.aliased) marks.push("алиас");
      else if (meta?.saturated) marks.push("предел");
      const p = purity(stat);
      slot.clean.textContent =
        marks.length > 0 ? `чистота ${p} · ${marks.join(" ")}` : `чистота ${p}`;
      slot.level.className = "";
      slot.vpp.className = "";
      slot.freq.className = stat?.aliased ? "warn" : "";
      slot.clean.className = marks.length > 0 ? "warn" : "";
      slot.level.title = "Среднее напряжение кадра (U)";
      slot.vpp.title = "Размах кадра: Vmax − Vmin";
      slot.freq.title = "Частота, измеренная по фронтам этого кадра";
      slot.clean.title =
        "Чистота: доля отсчётов без скачков больше половины размаха. " +
        "Алиас и предел АЦП помечаются отдельно.";
      if (meta?.dropouts) {
        slot.clean.title += ` Сбоев АЦП в кадре: ${meta.dropouts}.`;
      }
    } else {
      slot.level.textContent = "U —";
      slot.vpp.textContent = "Vpp —";
      slot.freq.textContent = "f —";
      slot.clean.textContent = state.text;
      slot.level.className = "";
      slot.vpp.className = "";
      slot.freq.className = "";
      slot.clean.className = state.kind === "offline" ? "bad" : "warn";
      slot.level.title = "";
      slot.vpp.title = "";
      slot.freq.title = "";
      slot.clean.title = "";
    }
  }
}

async function poll(): Promise<void> {
  if (polling) return;
  polling = true;
  try {
    const ix = await getJSON<Info>("/api/info");
    info = ix;

    let next: Capture | null = null;
    try {
      // One answer carries the frame and its metadata, so Vpp and the trace
      // always describe the same samples.
      next = await getJSON<Capture>(`/api/capture/v2?width=${plotWidth()}`);
    } catch (e) {
      // No frame yet is a normal state, not a failure.
      if (!(e instanceof ApiError && e.status === 404)) throw e;
    }

    cap = next;
    columns = next ? decodeColumns(next) : null;
    if (next) {
      enabled = [...next.channels];
    } else if (channelList.length === 0) {
      await refreshChannels();
    }

    lastError = "";
    lastOkAt = performance.now();
    updateLabels();
    scheduleDraw();

    const bucket =
      info.last_block_age_ms === null
        ? "none"
        : info.last_block_age_ms > STALE_MS
          ? "stale"
          : "live";
    const key = next
      ? `${next.frame_id}|${next.channels.join(",")}|${next.width}|${info.link_state}|${info.running}|${bucket}`
      : `none|${channelList.join(",")}|${info.link_state}|${info.running}`;
    pollDelay = key !== lastFrameKey ? 100 : 300;
    lastFrameKey = key;
  } catch (e) {
    lastError = e instanceof Error ? e.message : String(e);
    // The last trace stays, the labels say why it is not moving.
    updateLabels();
    scheduleDraw();
    pollDelay = 500;
  } finally {
    polling = false;
  }
}

// ------------------------------------------------------------------- drawing

interface Prepared {
  ctx: CanvasRenderingContext2D;
  w: number;
  h: number;
}

function prepare(canvas: HTMLCanvasElement): Prepared | null {
  const rect = canvas.getBoundingClientRect();
  const w = Math.round(rect.width);
  const h = Math.round(rect.height);
  if (w <= 0 || h <= 0) return null;
  const dpr = window.devicePixelRatio || 1;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  return { ctx, w, h };
}

function drawGrid(ctx: CanvasRenderingContext2D, w: number, h: number): void {
  const left = 4;
  const top = 3;
  const pw = w - 8;
  const ph = h - 6;
  ctx.strokeStyle = "#1b2230";
  ctx.lineWidth = 1;
  for (let d = 0; d <= 4; d++) {
    const x = Math.round(left + (d * pw) / 4) + 0.5;
    ctx.beginPath();
    ctx.moveTo(x, top);
    ctx.lineTo(x, top + ph);
    ctx.stroke();
  }
  for (let d = 0; d <= 4; d++) {
    const y = Math.round(top + (d * ph) / 4) + 0.5;
    ctx.beginPath();
    ctx.moveTo(left, y);
    ctx.lineTo(left + pw, y);
    ctx.stroke();
  }
}

/**
 * The same scaling rule as the main window: the frame's own channel metadata,
 * or the measured span when autoscale is on. A constant signal therefore sits
 * on its real voltage instead of being blown up by a guessed range.
 */
function drawTrace(
  ctx: CanvasRenderingContext2D,
  w: number,
  h: number,
  ch: number,
  alpha: number,
): void {
  if (!cap || !columns) return;
  const ki = cap.channels.indexOf(ch);
  if (ki < 0) return;

  const meta = cap.channel_meta.find((m) => m.ch === ch);
  const stat = cap.stats.find((s) => s.ch === ch);
  const span =
    stat && stat.vmin !== null && stat.vmax !== null ? stat.vmax - stat.vmin : 0;
  const auto = meta?.autoscale ?? false;
  const vpd = auto && span > 0
    ? Math.max(0.0125, span / 6)
    : Math.max(0.0125, (meta?.full_scale_v ?? 20) / 4);
  const center =
    auto && stat && stat.vmin !== null && stat.vmax !== null
      ? (stat.vmin + stat.vmax) / 2
      : (meta?.center_v ?? 0);

  const left = 4;
  const top = 3;
  const pw = w - 8;
  const ph = h - 6;
  const mid = top + ph / 2;
  const yV = (v: number) => mid - ((v - center) * ph) / (8 * vpd);
  const color = COLORS[ch] ?? "#fff";

  // Zero volts, where it actually is.
  const zeroY = yV(0);
  if (zeroY >= top && zeroY <= top + ph) {
    ctx.strokeStyle = `${color}55`;
    ctx.setLineDash([3, 4]);
    ctx.beginPath();
    ctx.moveTo(left, Math.round(zeroY) + 0.5);
    ctx.lineTo(left + pw, Math.round(zeroY) + 0.5);
    ctx.stroke();
    ctx.setLineDash([]);
  }

  ctx.save();
  ctx.beginPath();
  ctx.rect(left, top, pw, ph);
  ctx.clip();
  ctx.globalAlpha = alpha;
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.4;
  ctx.lineJoin = "round";

  const base = ki * cap.width * 2;
  let previous: { x: number; y: number } | null = null;
  ctx.beginPath();
  for (let col = 0; col < cap.width; col++) {
    const lo = columns[base + col * 2] / 1000;
    const hi = columns[base + col * 2 + 1] / 1000;
    if (!(lo <= hi)) continue;
    const x = left + ((col + 0.5) * pw) / cap.width;
    const yLo = yV(lo);
    const yHi = yV(hi);
    // A constant signal has lo == hi; the 1.2 px minimum is what keeps that
    // line visible instead of letting antialiasing eat it.
    ctx.moveTo(x, yHi);
    ctx.lineTo(x, Math.max(yLo, yHi + 1.2));
    if (previous) {
      ctx.moveTo(previous.x, previous.y);
      ctx.lineTo(x, (yLo + yHi) / 2);
    }
    previous = { x, y: (yLo + yHi) / 2 };
  }
  ctx.stroke();
  ctx.restore();
}

function drawStateText(
  ctx: CanvasRenderingContext2D,
  w: number,
  h: number,
  text: string,
  color: string,
): void {
  ctx.save();
  ctx.font = "600 10px system-ui, 'Segoe UI', sans-serif";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  const tw = ctx.measureText(text).width;
  const bw = Math.min(w - 8, tw + 12);
  const bh = 16;
  ctx.fillStyle = "rgba(8, 11, 16, 0.84)";
  ctx.fillRect((w - bw) / 2, (h - bh) / 2, bw, bh);
  ctx.fillStyle = color;
  ctx.fillText(text, w / 2, h / 2 + 0.5);
  ctx.restore();
}

function drawAll(): void {
  for (let i = 0; i < slots.length; i++) {
    const slot = slots[i];
    const prepared = prepare(slot.canvas);
    if (!prepared) continue;
    const { ctx, w, h } = prepared;

    ctx.fillStyle = "#0d1016";
    ctx.fillRect(0, 0, w, h);
    drawGrid(ctx, w, h);

    const ch = slotChannel(i);
    const state = slotState(i);
    if (ch === null) {
      drawStateText(ctx, w, h, "Канал недоступен", "#707a8b");
      continue;
    }

    if (cap && columns) {
      drawTrace(ctx, w, h, ch, state.kind === "ok" ? 1 : 0.28);
    }
    if (state.kind !== "ok") {
      drawStateText(
        ctx,
        w,
        h,
        state.text,
        state.kind === "offline" ? "#ff8181" : state.kind === "unavailable" ? "#707a8b" : "#ffd16c",
      );
    }
  }
}

// ---------------------------------------------------------------------- boot

async function boot(): Promise<void> {
  buildShell();
  await refreshChannels();
  await poll();
  updateLabels();

  // One request in flight at a time; the delay comes from the poll above, so a
  // static held frame costs a third of the requests a live one does.
  const tick = async (): Promise<void> => {
    if (!document.hidden) await poll();
    setTimeout(() => void tick(), pollDelay);
  };
  void tick();

  document.addEventListener("visibilitychange", () => {
    lastOkAt = performance.now();
    if (!document.hidden) {
      void poll();
      scheduleDraw();
    }
  });

  // A wedged page must not show a frozen trace for ever. The main window has a
  // native watchdog on top of this; the mini window is small enough not to need
  // a second one in Rust.
  window.setInterval(() => {
    if (!document.hidden && performance.now() - lastOkAt > 15_000) {
      location.reload();
    }
  }, 5_000);

  window.setInterval(savePosition, 1000);
  window.addEventListener("pagehide", savePosition);
  window.addEventListener("beforeunload", savePosition);

  if (lastError) updateLabels();
}

void boot();
