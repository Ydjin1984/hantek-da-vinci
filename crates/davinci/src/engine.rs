//! Acquisition engine: one thread owns the device, everything else reads state.
//!
//! The engine keeps a short rolling history for the live view, per-channel
//! statistics, a peak-hold that never resets on its own, and an optional
//! continuous log on disk. The log is the only place where full-rate data lives;
//! the UI is always served a decimated envelope, which is what makes eight
//! channels at once cheap to render and impossible to "lose" a peak.

use hantek1008::protocol::Measure;
use hantek1008::{Block, Calibration, Device, Range, TimeDiv, CHANNELS};
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How much history the live view can look back at.
const HISTORY_SECONDS: f64 = 5.0;

/// Live parameters, changed from the UI while acquisition runs.
#[derive(Debug, Clone)]
pub struct Params {
    pub enabled: Vec<usize>,
    pub ranges: [Range; CHANNELS],
    pub time_div: TimeDiv,
}

impl Default for Params {
    fn default() -> Self {
        Params { enabled: (0..CHANNELS).collect(), ranges: [Range::V20; CHANNELS], time_div: TimeDiv::DEFAULT }
    }
}

/// Peak-hold values since the last reset.
#[derive(Debug, Clone, Copy, Default)]
pub struct Peak {
    pub min: f32,
    pub max: f32,
}

impl Peak {
    fn update(&mut self, v: f32) {
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
    }
}

/// Public snapshot of the engine, cheap to clone for HTTP handlers.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub running: bool,
    pub link: String,
    pub sample_rate: f64,
    pub block_samples: usize,
    pub blocks_per_s: f64,
    pub time_div: String,
    pub error: Option<String>,
    pub log_path: Option<String>,
    pub log_bytes: u64,
    pub log_seconds: f64,
    pub blocks_total: u64,
}

struct Inner {
    params: Params,
    calib: Calibration,
    history: VecDeque<Arc<Block>>,
    peaks: [Peak; CHANNELS],
    info_rate: f64,
    block_samples: usize,
    blocks_per_s: f64,
    link: String,
    error: Option<String>,
    log: Option<LogState>,
    started: Instant,
    blocks_total: u64,
    frame_counter: u64,
}

struct LogState {
    path: PathBuf,
    writer: BufWriter<std::fs::File>,
    bytes: u64,
    since_fsync: u64,
    started_at: Instant,
}

/// The acquisition engine.
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    running: Arc<AtomicBool>,
    seq: Arc<AtomicU64>,
    reset_peaks: Arc<AtomicBool>,
}

impl Engine {
    pub fn new(dev: &Device) -> Engine {
        Engine {
            inner: Arc::new(Mutex::new(Inner {
                params: Params {
                    enabled: dev.enabled.clone(),
                    ranges: dev.ranges,
                    time_div: dev.time_div,
                },
                calib: dev.calib.clone(),
                history: VecDeque::new(),
                peaks: [Peak { min: f32::INFINITY, max: f32::NEG_INFINITY }; CHANNELS],
                info_rate: fixed_rate(),
                block_samples: 0,
                blocks_per_s: 0.0,
                link: dev.info.link.clone(),
                error: None,
                log: None,
                started: Instant::now(),
                blocks_total: 0,
                frame_counter: 0,
            })),
            running: Arc::new(AtomicBool::new(true)),
            seq: Arc::new(AtomicU64::new(0)),
            reset_peaks: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An engine for a program that has no instrument (yet).
    ///
    /// It never produces samples: the status carries the reason and `run` is
    /// simply not called. This exists so the window opens and explains itself
    /// instead of the process exiting on a missing cable.
    pub fn offline(link: String, error: String) -> Engine {
        Engine {
            inner: Arc::new(Mutex::new(Inner {
                params: Params::default(),
                calib: Calibration::default(),
                history: VecDeque::new(),
                peaks: [Peak { min: f32::INFINITY, max: f32::NEG_INFINITY }; CHANNELS],
                info_rate: 0.0,
                block_samples: 0,
                blocks_per_s: 0.0,
                link,
                error: Some(error),
                log: None,
                started: Instant::now(),
                blocks_total: 0,
                frame_counter: 0,
            })),
            running: Arc::new(AtomicBool::new(false)),
            seq: Arc::new(AtomicU64::new(0)),
            reset_peaks: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn handle(&self) -> EngineHandle {
        EngineHandle {
            inner: Arc::clone(&self.inner),
            running: Arc::clone(&self.running),
            reset_peaks: Arc::clone(&self.reset_peaks),
        }
    }

    /// Run the acquisition loop forever on the current thread.
    pub fn run(self, mut dev: Device) -> ! {
        let mut applied = Params {
            enabled: dev.enabled.clone(),
            ranges: dev.ranges,
            time_div: dev.time_div,
        };
        let mut last_rate_check = Instant::now();
        let mut blocks_at_check = 0u64;
        loop {
            // Push user changes down to the hardware before acquiring again.
            let want = { self.inner.lock().unwrap().params.clone() };
            if want.enabled != applied.enabled {
                let _ = dev.set_channels(&want.enabled);
                applied.enabled = want.enabled.clone();
            }
            if want.ranges != applied.ranges {
                let _ = dev.set_ranges(&want.ranges);
                applied.ranges = want.ranges;
            }
            if want.time_div.id != applied.time_div.id {
                let _ = dev.set_time_div(want.time_div);
                applied.time_div = want.time_div;
            }

            if !self.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                // Idle: keep the device alive without acquiring.
                let _ = dev.ping();
                continue;
            }
            match dev.read_block() {
                Ok(block) => {
                    let n = block.n;
                    let elapsed = {
                        let mut g = self.inner.lock().unwrap();
                        ingest(&mut g, block, &self.reset_peaks);
                        g.blocks_total += 1;
                        g.blocks_total
                    };
                    if last_rate_check.elapsed() >= Duration::from_secs(1) {
                        let now = Instant::now();
                        let dt = now.duration_since(last_rate_check).as_secs_f64();
                        let db = elapsed - blocks_at_check;
                        blocks_at_check = elapsed;
                        last_rate_check = now;
                        let mut g = self.inner.lock().unwrap();
                        if dt > 0.0 {
                            g.blocks_per_s = db as f64 / dt;
                            g.block_samples = n;
                            if fixed_rate() == 0.0 {
                                g.info_rate = g.blocks_per_s * n as f64;
                            }
                        }
                    }
                }
                Err(e) => {
                    let mut g = self.inner.lock().unwrap();
                    g.error = Some(e.to_string());
                    drop(g);
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

/// Cheap clonable control surface used by the HTTP layer.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Mutex<Inner>>,
    running: Arc<AtomicBool>,
    reset_peaks: Arc<AtomicBool>,
}

impl EngineHandle {
    pub fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::Relaxed);
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn reset_peaks(&self) {
        self.reset_peaks.store(true, Ordering::Relaxed);
    }

    pub fn set_channels(&self, chans: Vec<usize>) {
        let mut g = self.inner.lock().unwrap();
        if !chans.is_empty() {
            g.params.enabled = chans;
        }
    }

    pub fn set_range(&self, ch: usize, range: Range) {
        let mut g = self.inner.lock().unwrap();
        if ch < CHANNELS {
            g.params.ranges[ch] = range;
        }
    }

    pub fn set_range_all(&self, range: Range) {
        let mut g = self.inner.lock().unwrap();
        g.params.ranges = [range; CHANNELS];
    }

    pub fn status(&self) -> Status {
        let g = self.inner.lock().unwrap();
        Status {
            running: self.running.load(Ordering::Relaxed),
            link: g.link.clone(),
            sample_rate: g.info_rate,
            block_samples: g.block_samples,
            blocks_per_s: g.blocks_per_s,
            time_div: g.params.time_div.label(),
            error: g.error.clone(),
            log_path: g.log.as_ref().map(|l| l.path.display().to_string()),
            log_bytes: g.log.as_ref().map(|l| l.bytes).unwrap_or(0),
            log_seconds: g.log.as_ref().map(|l| l.started_at.elapsed().as_secs_f64()).unwrap_or(0.0),
            blocks_total: g.blocks_total,
        }
    }

    /// Per-channel statistics as JSON-ready values.
    ///
    /// Statistics cover the last second, not a single 10 ms block: a 50 Hz
    /// waveform has one period in 20 ms, so a per-block window would report a
    /// random slice of it as "Vmax" and make the numbers look unreliable.
    pub fn channels(&self) -> Vec<serde_json::Value> {
        let g = self.inner.lock().unwrap();
        let rate = if g.info_rate > 0.0 { g.info_rate } else { 50_000.0 };
        let need = rate as usize;
        let mut blocks: Vec<Arc<Block>> = Vec::new();
        let mut total = 0usize;
        for b in g.history.iter().rev() {
            blocks.push(Arc::clone(b));
            total += b.n;
            if total >= need {
                break;
            }
        }
        blocks.reverse();
        let dt = (1.0 / rate) as f32;

        (0..CHANNELS)
            .map(|ch| {
                let range = g.params.ranges[ch];
                let zero = g.calib.zero(range, ch);
                let vpc = range.volt_per_count();
                let mut volts: Vec<f32> = Vec::with_capacity(total);
                for b in &blocks {
                    if let Some(raw) = &b.ch[ch] {
                        volts.extend(raw.iter().map(|&v| (v as f32 - zero) * vpc));
                    }
                }
                let m = if volts.is_empty() { None } else { Some(Measure::of(&volts, dt)) };
                let p = g.peaks[ch];
                serde_json::json!({
                    "ch": ch + 1,
                    "enabled": g.params.enabled.contains(&ch),
                    "range_id": range.id(),
                    "range": range.label(),
                    "resolution": range.resolution(),
                    "min": m.map(|m| m.vmin),
                    "max": m.map(|m| m.vmax),
                    "pp": m.map(|m| m.vpp),
                    "mean": m.map(|m| m.mean),
                    "rms": m.map(|m| m.rms),
                    "rms_ac": m.map(|m| m.rms_ac),
                    "freq": m.and_then(|m| m.freq),
                    "duty": m.and_then(|m| m.duty),
                    "peak_min": if p.min.is_finite() { Some(p.min) } else { None },
                    "peak_max": if p.max.is_finite() { Some(p.max) } else { None },
                })
            })
            .collect()
    }

    pub fn params(&self) -> (Params, Calibration) {
        let g = self.inner.lock().unwrap();
        (g.params.clone(), g.calib.clone())
    }

    /// Build the min/max envelope the UI draws.
    ///
    /// One column per pixel, true extremes per column: a 3 ns glitch in a 200 ms
    /// window survives, which is exactly what the vendor renderer throws away.
    pub fn frame(&self, width: usize, span_s: f64) -> Frame {
        let g = self.inner.lock().unwrap();
        let width = width.clamp(16, 8192);
        let enabled: Vec<usize> = g.params.enabled.clone();
        let rate = if g.info_rate > 0.0 { g.info_rate } else { 1.0 };
        let want = (span_s * rate) as usize;
        let mut picked: Vec<Arc<Block>> = Vec::new();
        let mut total = 0usize;
        for b in g.history.iter().rev() {
            picked.push(Arc::clone(b));
            total += b.n;
            if total >= want || picked.len() > 4096 {
                break;
            }
        }
        picked.reverse();

        let mut out: Vec<Vec<(f32, f32)>> = Vec::with_capacity(enabled.len());
        for &ch in &enabled {
            let mut cols = vec![(f32::INFINITY, f32::NEG_INFINITY); width];
            let range = g.params.ranges[ch];
            let zero = g.calib.zero(range, ch);
            let vpc = range.volt_per_count();
            let mut seen = 0usize;
            let total_n: usize = picked.iter().map(|b| b.n).sum();
            for b in &picked {
                if let Some(raw) = &b.ch[ch] {
                    for &v in raw {
                        let idx = (seen * width / total_n.max(1)).min(width - 1);
                        let volts = (v as f32 - zero) * vpc;
                        let cell = &mut cols[idx];
                        if volts < cell.0 {
                            cell.0 = volts;
                        }
                        if volts > cell.1 {
                            cell.1 = volts;
                        }
                        seen += 1;
                    }
                }
            }
            for c in &mut cols {
                if !c.0.is_finite() {
                    *c = (0.0, 0.0);
                }
            }
            out.push(cols);
        }

        Frame {
            width,
            channels: enabled,
            dt_s: (1.0 / rate) as f32,
            t0_ns: picked.first().map(|b| b.t_ns).unwrap_or(0),
            columns: out,
        }
    }

    pub fn start_log(&self, path: PathBuf) -> std::io::Result<()> {
        let mut g = self.inner.lock().unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let meta = serde_json::json!({
            "channels": CHANNELS,
            "enabled": g.params.enabled,
            "ranges": (0..CHANNELS).map(|c| g.params.ranges[c].id()).collect::<Vec<_>>(),
            "time_div": g.params.time_div.id,
            "sample_rate": g.info_rate,
            "zero": (0..CHANNELS).map(|c| g.calib.zero(g.params.ranges[c], c)).collect::<Vec<_>>(),
        });
        let head = meta.to_string();
        w.write_all(b"DAV1")?;
        w.write_all(&1u16.to_le_bytes())?;
        w.write_all(&(CHANNELS as u16).to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(&(head.len() as u32).to_le_bytes())?;
        w.write_all(head.as_bytes())?;
        g.log = Some(LogState {
            path,
            writer: w,
            bytes: 0,
            since_fsync: 0,
            started_at: Instant::now(),
        });
        Ok(())
    }

    pub fn stop_log(&self) {
        let mut g = self.inner.lock().unwrap();
        if let Some(mut l) = g.log.take() {
            let _ = l.writer.flush();
            let _ = l.writer.get_ref().sync_all();
        }
    }
}

/// One decimated view of recent data.
pub struct Frame {
    pub width: usize,
    pub channels: Vec<usize>,
    pub dt_s: f32,
    pub t0_ns: u64,
    pub columns: Vec<Vec<(f32, f32)>>,
}

impl Frame {
    /// Binary wire format for the browser: volts in millivolts as i16.
    pub fn to_bytes(&self, zeroed: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.channels.len() * self.width * 4);
        out.extend_from_slice(b"DVF1");
        out.extend_from_slice(&(self.width as u32).to_le_bytes());
        let mut mask = 0u32;
        for &c in &self.channels {
            mask |= 1 << c;
        }
        out.extend_from_slice(&mask.to_le_bytes());
        out.extend_from_slice(&(self.dt_s * 1e9).to_le_bytes());
        out.extend_from_slice(&self.t0_ns.to_le_bytes());
        out.extend_from_slice(&(self.channels.len() as u32).to_le_bytes());
        let _ = zeroed;
        for cols in &self.columns {
            for (lo, hi) in cols {
                out.extend_from_slice(&to_mv(*lo).to_le_bytes());
                out.extend_from_slice(&to_mv(*hi).to_le_bytes());
            }
        }
        out
    }
}

fn to_mv(v: f32) -> i16 {
    (v * 1000.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

fn ingest(g: &mut Inner, block: Block, reset: &AtomicBool) {
    let rate = if g.info_rate > 0.0 {
        g.info_rate
    } else if let Ok(v) = std::env::var("DAVINCI_FIXED_RATE") {
        v.parse::<f64>().unwrap_or(50_000.0)
    } else {
        50_000.0
    };
    let do_reset = reset.swap(false, Ordering::Relaxed);

    for ch in 0..CHANNELS {
        let Some(raw) = &block.ch[ch] else { continue };
        let range = g.params.ranges[ch];
        let zero = g.calib.zero(range, ch);
        let vpc = range.volt_per_count();
        if do_reset {
            g.peaks[ch] = Peak { min: f32::INFINITY, max: f32::NEG_INFINITY };
        }
        let mut peak = g.peaks[ch];
        for &v in raw {
            peak.update((v as f32 - zero) * vpc);
        }
        g.peaks[ch] = peak;
    }

    // Log before trimming history.
    if let Some(log) = g.log.as_mut() {
        let mut payload = Vec::with_capacity(64 + block.n * 16);
        payload.extend_from_slice(&block.seq.to_le_bytes());
        payload.extend_from_slice(&block.t_ns.to_le_bytes());
        payload.extend_from_slice(&(block.n as u16).to_le_bytes());
        let mut mask = 0u8;
        for ch in 0..CHANNELS {
            if block.ch[ch].is_some() {
                mask |= 1 << ch;
            }
        }
        payload.push(mask);
        for ch in 0..CHANNELS {
            if let Some(raw) = &block.ch[ch] {
                for &v in raw {
                    payload.extend_from_slice(&(v as i16).to_le_bytes());
                }
            }
        }
        let len = payload.len() as u32;
        if log.writer.write_all(&len.to_le_bytes()).is_ok()
            && log.writer.write_all(&payload).is_ok()
        {
            log.bytes += 4 + len as u64;
            log.since_fsync += 1;
            if log.since_fsync >= 200 {
                log.since_fsync = 0;
                let _ = log.writer.flush();
                let _ = log.writer.get_ref().sync_data();
            }
        }
    }

    g.frame_counter += 1;
    g.history.push_back(Arc::new(block));
    let cap = (HISTORY_SECONDS * rate) as usize;
    let mut have = 0usize;
    for b in g.history.iter().rev() {
        have += b.n;
        if have > cap {
            break;
        }
    }
    while g.history.len() > 8 {
        let total: usize = g.history.iter().map(|b| b.n).sum();
        if total <= cap {
            break;
        }
        g.history.pop_front();
    }
}

/// Time base override for simulation and replay: when set, the engine trusts it
/// instead of measuring the link, so numbers stay comparable across machines.
pub fn fixed_rate() -> f64 {
    std::env::var("DAVINCI_FIXED_RATE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Unix seconds, for the log header and file names.
pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
