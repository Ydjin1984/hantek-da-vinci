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
    /// Full scale the user asked to see, in volts, symmetric around zero.
    ///
    /// The instrument itself has only three ranges (±0.41, ±2.5, ±20 V), so this
    /// is what the lane is scaled to, while `ranges` holds the hardware range
    /// chosen to cover it. Below ±0.41 V the difference is digital zoom.
    pub range_v: [f32; CHANNELS],
    pub time_div: TimeDiv,
    /// Fit each channel's trace to its whole lane. A display setting: the device
    /// never sees it, but it is part of the state that must survive a restart.
    pub autoscale: [bool; CHANNELS],
}

impl Default for Params {
    fn default() -> Self {
        Params {
            enabled: (0..CHANNELS).collect(),
            ranges: [Range::V20; CHANNELS],
            range_v: [20.0; CHANNELS],
            time_div: TimeDiv::DEFAULT,
            autoscale: [false; CHANNELS],
        }
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
    /// Protocol index of the time base, so the UI can step it.
    pub time_div_id: u8,
    /// Seconds one acquisition covers. This, not the USB throughput, is the
    /// width of the screen and therefore the only honest x-axis label.
    pub window_s: f64,
    /// True when the time base asks for more than the ADC can deliver, so the
    /// real window is longer than the one on the dial.
    pub window_clamped: bool,
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
                    range_v: dev.ranges.map(|r| r.full_scale()),
                    time_div: dev.time_div,
                    autoscale: [false; CHANNELS],
                },
                calib: dev.calib.clone(),
                history: VecDeque::new(),
                peaks: [Peak { min: f32::INFINITY, max: f32::NEG_INFINITY }; CHANNELS],
                info_rate: if fixed_rate() > 0.0 {
                    fixed_rate()
                } else {
                    dev.time_div.sample_rate(dev.enabled.len())
                },
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
            range_v: dev.ranges.map(|r| r.full_scale()),
            time_div: dev.time_div,
            autoscale: [false; CHANNELS],
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

            // The time axis is defined by the time base and the number of
            // channels, not by how fast USB happens to deliver blocks: the device
            // always returns one screen of 4000 samples, split between the
            // enabled channels. Deriving the rate from throughput instead made
            // every frequency measurement drift with USB load.
            self.inner.lock().unwrap().info_rate = implied_rate(&want);

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
                        }
                        // Throughput only; the sample rate comes from implied_rate.
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
        g.persist();
    }

    /// Change the hardware time base, the way a scope's time/div knob does.
    pub fn set_time_div(&self, td: TimeDiv) {
        let mut g = self.inner.lock().unwrap();
        g.params.time_div = td;
        g.persist();
    }

    pub fn set_range(&self, ch: usize, range: Range) {
        let mut g = self.inner.lock().unwrap();
        if ch < CHANNELS {
            g.params.ranges[ch] = range;
            g.params.range_v[ch] = range.full_scale();
        }
        g.persist();
    }

    pub fn set_range_all(&self, range: Range) {
        let mut g = self.inner.lock().unwrap();
        g.params.ranges = [range; CHANNELS];
        g.params.range_v = [range.full_scale(); CHANNELS];
        g.persist();
    }

    /// Choose the full scale a channel is drawn at, in volts.
    ///
    /// The hardware range is picked to cover it; anything finer than the ±0.41 V
    /// range is digital zoom, which costs resolution but nothing else.
    pub fn set_range_v(&self, ch: usize, volts: f32) {
        let mut g = self.inner.lock().unwrap();
        if ch >= CHANNELS {
            return;
        }
        let v = volts.clamp(0.05, 20.0);
        g.params.range_v[ch] = v;
        g.params.ranges[ch] = Range::covering(v);
        g.persist();
    }

    /// Fit one channel, or all of them, to the full height of their lane.
    pub fn set_autoscale(&self, ch: Option<usize>, on: bool) {
        let mut g = self.inner.lock().unwrap();
        match ch {
            Some(c) if c < CHANNELS => g.params.autoscale[c] = on,
            Some(_) => {}
            None => g.params.autoscale = [on; CHANNELS],
        }
        g.persist();
    }

    /// Everything currently held in memory as CSV, for the "Экспорт" button.
    ///
    /// Wide format, one row per sample: the first columns are the acquisition
    /// number and the time inside it, the rest are one column per enabled
    /// channel. Time is relative to the start of the sweep, because consecutive
    /// acquisitions are not contiguous and absolute time would imply they are.
    pub fn export_csv(&self) -> String {
        let g = self.inner.lock().unwrap();
        let enabled = g.params.enabled.clone();
        let rate = if g.info_rate > 0.0 { g.info_rate } else { 1.0 };
        let dt_ms = 1000.0 / rate;

        let mut out = String::with_capacity(1 << 16);
        out.push_str("# Осциллограф Хелтег да Винчи · экспорт данных\n");
        out.push_str(&format!("# развёртка: {}\n", g.params.time_div.label()));
        out.push_str(&format!("# частота: {rate:.0} Гц на канал\n"));
        for &c in &enabled {
            let r = g.params.ranges[c];
            out.push_str(&format!("# CH{}: {} ({})\n", c + 1, r.label(), r.id()));
        }
        out.push_str("seq,t_ms");
        for &c in &enabled {
            out.push_str(&format!(",CH{}", c + 1));
        }
        out.push('\n');

        for b in &g.history {
            // Only blocks acquired with exactly this channel set: a block taken
            // while a channel was off has no data for it, and mixing the two
            // produces columns with holes that look like dropouts.
            if !enabled.iter().all(|&c| b.ch[c].is_some()) {
                continue;
            }
            for i in 0..b.n {
                out.push_str(&format!("{},{:.6}", b.seq, i as f64 * dt_ms));
                for &c in &enabled {
                    out.push(',');
                    if let Some(raw) = b.ch[c].as_ref().and_then(|r| r.get(i)) {
                        let range = g.params.ranges[c];
                        let zero = g.calib.zero(range, c);
                        let v = (*raw as f32 - zero) * range.volt_per_count();
                        out.push_str(&format!("{v:.4}"));
                    }
                }
                out.push('\n');
            }
        }
        out
    }

    /// Put back the settings read from disk at startup, without writing them
    /// straight back out again.
    pub fn restore(&self, s: &crate::settings::Settings) {
        let mut g = self.inner.lock().unwrap();
        g.params.autoscale = [false; CHANNELS];
        for (i, slot) in g.params.autoscale.iter_mut().enumerate() {
            *slot = s.autoscale.get(i).copied().unwrap_or(false);
        }
        for i in 0..CHANNELS {
            if let Some(v) = s.range_v.get(i) {
                let v = v.clamp(0.05, 20.0);
                g.params.range_v[i] = v;
                g.params.ranges[i] = Range::covering(v);
            }
        }
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
            time_div_id: g.params.time_div.id,
            window_s: g.params.time_div.real_window_s(g.params.enabled.len().max(1)),
            window_clamped: {
                let n = g.params.enabled.len().max(1);
                g.params.time_div.real_window_s(n) > g.params.time_div.window_s() * 1.02
            },
            error: g.error.clone(),
            log_path: g.log.as_ref().map(|l| l.path.display().to_string()),
            log_bytes: g.log.as_ref().map(|l| l.bytes).unwrap_or(0),
            log_seconds: g.log.as_ref().map(|l| l.started_at.elapsed().as_secs_f64()).unwrap_or(0.0),
            blocks_total: g.blocks_total,
        }
    }

    /// Per-channel statistics as JSON-ready values.
    ///
    /// Covers about the last second of *wall clock*, not a single block and not
    /// the last second of captured signal. One block of 5 ms arrives every ~60 ms,
    /// so a second of signal is twelve seconds of wall time: the cards then keep
    /// reporting a peak from a signal that left the screen long ago, while the
    /// trace shows nothing. Several blocks are still needed, because a 50 Hz
    /// waveform has one period in 20 ms and a single block would report a random
    /// slice of it as "Vmax".
    pub fn channels(&self) -> Vec<serde_json::Value> {
        let g = self.inner.lock().unwrap();
        // Roughly one second at the observed block rate, bounded on both sides.
        let want = (g.blocks_per_s.max(1.0) as usize).clamp(1, 200);
        let mut blocks: Vec<Arc<Block>> = Vec::new();
        let mut total = 0usize;
        for b in g.history.iter().rev() {
            blocks.push(Arc::clone(b));
            total += b.n;
            if blocks.len() >= want {
                break;
            }
        }
        blocks.reverse();
        let rate = if g.info_rate > 0.0 { g.info_rate } else { 50_000.0 };
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
                    "autoscale": g.params.autoscale[ch],
                    "range_id": range.id(),
                    "range": range.label(),
                    "range_v": g.params.range_v[ch],
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
    /// One column per pixel, true extremes per column: a 3 ns glitch survives,
    /// which is exactly what the vendor renderer throws away.
    ///
    /// Only the newest acquisition is drawn, never a run of them. Consecutive
    /// blocks are separated by however long the USB round trip takes (tens of
    /// milliseconds), so stitching them into one axis would invent a continuous
    /// window that was never captured. One block *is* one screen.
    pub fn frame(&self, width: usize, _span_s: f64) -> Frame {
        let g = self.inner.lock().unwrap();
        let width = width.clamp(16, 8192);
        let enabled: Vec<usize> = g.params.enabled.clone();
        let rate = if g.info_rate > 0.0 { g.info_rate } else { 1.0 };
        let picked: Vec<Arc<Block>> = match g.history.back() {
            Some(b) => vec![Arc::clone(b)],
            None => Vec::new(),
        };

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
            n: picked.first().map(|b| b.n).unwrap_or(0),
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

impl Inner {
    /// Write the current state to disk so the next start comes up the same way.    ///
    /// Called on every user change rather than on exit: the program is closed by
    /// killing the browser tab or the machine, and neither runs a shutdown path.
    fn persist(&self) {
        crate::settings::Settings {
            enabled: self.params.enabled.clone(),
            ranges: self.params.ranges.iter().map(|r| r.id()).collect(),
            range_v: self.params.range_v.to_vec(),
            time_div_id: self.params.time_div.id,
            autoscale: self.params.autoscale.to_vec(),
        }
        .save();
    }
}

/// One decimated view of one acquisition.
pub struct Frame {
    pub width: usize,
    pub channels: Vec<usize>,
    pub dt_s: f32,
    pub t0_ns: u64,
    /// Samples per channel behind this frame, so the UI can label the real
    /// width of the window instead of guessing.
    pub n: usize,
    pub columns: Vec<Vec<(f32, f32)>>,
}

impl Frame {
    /// Binary wire format for the browser: volts in millivolts as i16.
    pub fn to_bytes(&self, zeroed: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.channels.len() * self.width * 4);
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
        out.extend_from_slice(&(self.n as u32).to_le_bytes());
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

/// Sample rate implied by the time base and the number of enabled channels.
///
/// `DAVINCI_FIXED_RATE` still wins, so replay and simulation numbers stay
/// comparable across machines.
fn implied_rate(params: &Params) -> f64 {
    let fixed = fixed_rate();
    if fixed > 0.0 {
        return fixed;
    }
    params.time_div.sample_rate(params.enabled.len())
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
