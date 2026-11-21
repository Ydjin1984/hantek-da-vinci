//! One published frame, frozen.
//!
//! Everything a client needs to draw, measure, export and re-export a single
//! sweep lives here, and it is immutable once built. That is the point: the live
//! view, the "Удержать экран" button, CSV of the frame and PNG of the frame all
//! have to describe the *same* samples. Rebuilding a frame from "whatever the
//! engine holds now" is how a frozen screen starts exporting data the user never
//! looked at.
//!
//! A snapshot carries its own metadata — ranges, calibration-independent volts,
//! `dt`, channel layout, trigger position — because the engine's settings can
//! change while a captured frame is still on screen.

use crate::trigger::Config;
use hantek1008::protocol::Measure;
use hantek1008::CHANNELS;

/// Fewer samples than this per period of the measured signal and the trace on
/// screen is an alias of the waveform, not the waveform. Four is the usual
/// "barely enough to see the shape" line: below it a square wave turns into a
/// filled band and its frequency reading means nothing.
pub const MIN_POINTS_PER_PERIOD: f64 = 4.0;

/// Share of sample pairs that may jump by more than half the swing before the
/// trace is called undersampled.
///
/// A square wave with six real points per period jumps twice in six (0,33); an
/// aliased bus jumps on two samples out of three (0,67). A sine is smoother than
/// either. 0,5 sits in the gap, not on an edge.
pub const JUMP_FRACTION_LIMIT: f64 = 0.5;

/// Share of neighbouring samples whose difference exceeds half the swing.
fn jump_fraction(samples: &[f32]) -> f64 {
    if samples.len() < 3 {
        return 0.0;
    }
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &v in samples {
        if !v.is_finite() {
            continue;
        }
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let swing = hi - lo;
    if !(swing > 0.0) {
        return 0.0;
    }
    let limit = swing * 0.5;
    let mut jumps = 0usize;
    let mut pairs = 0usize;
    for w in samples.windows(2) {
        if !(w[0].is_finite() && w[1].is_finite()) {
            continue;
        }
        pairs += 1;
        if (w[1] - w[0]).abs() > limit {
            jumps += 1;
        }
    }
    if pairs == 0 {
        0.0
    } else {
        jumps as f64 / pairs as f64
    }
}

/// One sweep, selected by the trigger and measured on its own samples.
#[derive(Debug)]
pub struct Snapshot {
    pub id: u64,
    /// Configuration generation this frame was acquired under.
    pub generation: u64,
    pub dt_s: f64,
    /// Channels present in this frame, in the order the UI draws them.
    pub channels: Vec<usize>,
    /// Calibrated volts, one vector per entry of `channels`.
    ///
    /// These are the block's own samples covering the window, not an envelope:
    /// measurements and CSV are computed from them, so a millivolt-level reading
    /// never comes from a resampled column.
    pub samples: Vec<Vec<f32>>,
    /// Window start inside the block, in samples (fractional).
    pub start: f64,
    /// Window width in samples (fractional).
    pub len: f64,
    /// Samples per channel in the whole block.
    pub n_block: usize,
    pub cfg: Config,
    /// True when no edge was used (Free, or Auto after its timeout).
    pub free: bool,
    /// Where the edge landed in the window, as a percentage.
    pub pct: f32,
    pub clipped_pre: bool,
    pub clipped_post: bool,
    /// Full scale the user asked to see, per channel, in volts.
    pub range_v: [f32; CHANNELS],
    /// Hardware range id per channel.
    pub range_id: [u8; CHANNELS],
    /// Display centre per channel, in volts.
    pub center_v: [f32; CHANNELS],
    pub autoscale: [bool; CHANNELS],
    /// Per-channel measurements of this frame only.
    pub stats: Vec<Measure>,
    /// ADC saturation seen in the block, per channel.
    pub saturation: Vec<bool>,
    /// Single-sample converter dropouts repaired in this frame, per channel.
    pub dropouts: Vec<usize>,
    /// Cumulative extremes at the moment of capture.
    pub peak_min: [f32; CHANNELS],
    pub peak_max: [f32; CHANNELS],
    pub hysteresis_v: f32,
}

impl Snapshot {
    /// Sample rate that this frame was actually captured at.
    pub fn rate(&self) -> f64 {
        if self.dt_s > 0.0 {
            1.0 / self.dt_s
        } else {
            0.0
        }
    }

    /// Width of the window in seconds.
    pub fn window_s(&self) -> f64 {
        self.len * self.dt_s
    }

    /// Seconds from the window start to the trigger point (0 when free).
    pub fn trigger_offset_s(&self) -> f64 {
        if self.free {
            0.0
        } else {
            self.pct as f64 / 100.0 * self.window_s()
        }
    }

    /// The min/max envelope the plot draws, one column per pixel.
    ///
    /// Columns are filled from the raw samples of the window, so a single-sample
    /// spike survives any amount of horizontal compression. A column with no
    /// sample keeps `lo > hi`, the "nothing here" marker — it is never turned into
    /// a fake 0 V.
    pub fn columns(&self, width: usize) -> Vec<Vec<(f32, f32)>> {
        let width = width.clamp(1, 8192);
        let mut out = Vec::with_capacity(self.channels.len());
        for (k, _ch) in self.channels.iter().enumerate() {
            let mut cols = vec![(f32::INFINITY, f32::NEG_INFINITY); width];
            let Some(samples) = self.samples.get(k) else {
                out.push(cols);
                continue;
            };
            let base = self.start.floor();
            let span = self.len.max(f64::MIN_POSITIVE);
            for (i, &v) in samples.iter().enumerate() {
                let pos = base + i as f64;
                if pos < self.start - 0.5 || pos > self.start + span + 0.5 {
                    continue;
                }
                let x = (pos - self.start) / span * width as f64;
                let col = (x.floor() as isize).clamp(0, width as isize - 1) as usize;
                let cell = &mut cols[col];
                if v < cell.0 {
                    cell.0 = v;
                }
                if v > cell.1 {
                    cell.1 = v;
                }
            }
            out.push(cols);
        }
        out
    }

    /// Interleaved little-endian `i16` millivolts, `(lo, hi)` per column.
    pub fn columns_i16(&self, width: usize) -> Vec<u8> {
        let cols = self.columns(width);
        let mut out = Vec::with_capacity(cols.len() * width * 4);
        for ch in cols {
            for (lo, hi) in ch {
                out.extend_from_slice(&to_mv(lo).to_le_bytes());
                out.extend_from_slice(&to_mv(hi).to_le_bytes());
            }
        }
        out
    }

    /// Metadata for `/api/capture/v2`, without the samples.
    pub fn meta_json(&self, width: usize, held: bool, state: &str, running: bool) -> serde_json::Value {
        let stats: Vec<serde_json::Value> = self
            .channels
            .iter()
            .enumerate()
            .map(|(k, &ch)| {
                let m = self.stats.get(k).copied().unwrap_or_default();
                let points_per_period = m
                    .freq
                    .filter(|f| f.is_finite() && *f > 0.0)
                    .map(|f| 1.0 / (f as f64 * self.dt_s.max(f64::MIN_POSITIVE)));
                // Points per period is measured against the frequency the frame
                // *reports*, and an alias reports a low one: a 15,8 kHz bus seen
                // at 20 kHz "measures" 3,3 kHz and looks like six points per
                // period. A signal that cannot be sampled properly also jumps
                // between the rails on most sample pairs, which a waveform with
                // six real points per period does not do (a square wave has two
                // jumps in six, a sine none).
                let jump_fraction = self.samples.get(k).map(|s| jump_fraction(s)).unwrap_or(0.0);
                let too_few = points_per_period.is_some_and(|p| p < MIN_POINTS_PER_PERIOD);
                let jumping = m.freq.is_some() && jump_fraction > JUMP_FRACTION_LIMIT;
                serde_json::json!({
                    "ch": ch,
                    "vmin": finite(m.vmin),
                    "vmax": finite(m.vmax),
                    "vpp": finite(m.vpp),
                    "mean": finite(m.mean),
                    "rms": finite(m.rms),
                    "rms_ac": finite(m.rms_ac),
                    "freq": m.freq.filter(|f| f.is_finite()),
                    "duty": m.duty.filter(|d| d.is_finite()),
                    "period_s": m.period_s.filter(|p| p.is_finite()),
                    "points_per_period": points_per_period,
                    "jump_fraction": jump_fraction,
                    "aliased": too_few || jumping,
                    "alias_reason": if too_few {
                        Some("points")
                    } else if jumping {
                        Some("jumps")
                    } else {
                        None
                    },
                    "n": self.samples.get(k).map(|s| s.len()).unwrap_or(0),
                    "window_s": self.window_s(),
                })
            })
            .collect();

        let channels: Vec<serde_json::Value> = self
            .channels
            .iter()
            .enumerate()
            .map(|(k, &ch)| {
                serde_json::json!({
                    "ch": ch,
                    "range_id": self.range_id[ch],
                    "full_scale_v": self.range_v[ch],
                    "center_v": self.center_v[ch],
                    "autoscale": self.autoscale[ch],
                    "saturated": self.saturation.get(k).copied().unwrap_or(false),
                    "dropouts": self.dropouts.get(k).copied().unwrap_or(0),
                    "peak_min": finite(self.peak_min[ch]),
                    "peak_max": finite(self.peak_max[ch]),
                })
            })
            .collect();

        serde_json::json!({
            "format": "dav2",
            "frame_id": self.id,
            "generation": self.generation,
            "held": held,
            "running": running,
            "state": state,
            "dt_s": self.dt_s,
            "sample_rate": self.rate(),
            "window_s": self.window_s(),
            "window_samples": self.len,
            "start_sample": self.start,
            "n_block": self.n_block,
            "channels": self.channels,
            "trigger": {
                "mode": self.cfg.mode.as_str(),
                "edge": self.cfg.edge.as_str(),
                "source": self.cfg.source,
                "level_v": self.cfg.level_v,
                "pretrigger_pct": self.cfg.pretrigger_pct,
                "timeout_ms": self.cfg.timeout_ms,
                "pct": self.pct,
                "free": self.free,
                "clipped_pre": self.clipped_pre,
                "clipped_post": self.clipped_post,
                "t0_s": self.trigger_offset_s(),
                "hysteresis_v": self.hysteresis_v,
            },
            "channel_meta": channels,
            "stats": stats,
            "width": width,
        })
    }

    /// CSV of exactly this frame: raw volts, one row per sample.
    ///
    /// Time is relative to the trigger point when there is one, otherwise to the
    /// start of the window; the header says which. Long format would be friendlier
    /// to a database, but every scope exports wide, and this is what people paste
    /// into a spreadsheet.
    pub fn csv(&self) -> String {
        let mut out = String::with_capacity(1 << 16);
        out.push_str("# Осциллограф Хелтег да Винчи · кадр\n");
        out.push_str(&format!("# кадр: {}\n", self.id));
        out.push_str(&format!(
            "# режим: {}, фронт: {}, источник: CH{}, уровень: {:.6} В\n",
            self.cfg.mode.as_str(),
            self.cfg.edge.as_str(),
            self.cfg.source + 1,
            self.cfg.level_v
        ));
        out.push_str(&format!(
            "# частота дискретизации: {:.1} Гц на канал, dt: {:.9} с\n",
            self.rate(),
            self.dt_s
        ));
        out.push_str(&format!(
            "# окно: {:.9} с, {} отсчётов на канал, начало: {:.3} отсчёта\n",
            self.window_s(),
            self.samples.first().map(|s| s.len()).unwrap_or(0),
            self.start
        ));
        out.push_str(&format!(
            "# время отсчитывается от {}\n",
            if self.free { "начала окна" } else { "фронта T₀" }
        ));
        out.push_str("# формат: секунды, вольты\n");
        out.push_str("frame,t_s");
        for &ch in &self.channels {
            out.push_str(&format!(",CH{}_V", ch + 1));
        }
        out.push('\n');

        let t0 = if self.free { 0.0 } else { self.trigger_offset_s() };
        let n = self.samples.iter().map(|s| s.len()).min().unwrap_or(0);
        for i in 0..n {
            out.push_str(&format!("{},{:.9}", self.id, i as f64 * self.dt_s - t0));
            for s in &self.samples {
                out.push(',');
                out.push_str(&format!("{:.6}", s.get(i).copied().unwrap_or(f32::NAN)));
            }
            out.push('\n');
        }
        out
    }

    /// Short human description of the frame for logs and the status panel.
    #[allow(dead_code)]
    pub fn describe(&self) -> String {
        format!(
            "кадр {} · {} · {:.0} Гц · {:.3} мс · {} отсч/кан",
            self.id,
            self.cfg.mode.as_str(),
            self.rate(),
            self.window_s() * 1000.0,
            self.samples.first().map(|s| s.len()).unwrap_or(0)
        )
    }

}

fn finite(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}

/// Repair single-sample converter dropouts in a block.
///
/// The instrument occasionally returns one sample at the rail (count 0, which the
/// decimation then draws as −20 V) while both neighbours sit at mid-scale. That is
/// not a signal: no front end moves 20 V in 2.5 µs, and the raw log keeps the
/// original counts for anyone who wants to see it. One isolated rail sample
/// between two ordinary ones is replaced by the mean of its neighbours; real
/// saturation comes in runs and is left alone (and flagged separately).
///
/// Returns the corrected counts and how many samples were repaired.
pub fn repair_dropouts(raw: &[u16]) -> (Vec<f32>, usize) {
    let mut counts: Vec<f32> = raw.iter().map(|&v| v as f32).collect();
    let mut fixed = 0usize;
    if counts.len() < 3 {
        return (counts, 0);
    }
    let rail_high = hantek1008::protocol::ADC_MAX as f32;
    for i in 1..counts.len() - 1 {
        let c = counts[i];
        let at_rail = c <= 0.0 || c >= rail_high;
        if !at_rail {
            continue;
        }
        let (a, b) = (counts[i - 1], counts[i + 1]);
        if a != c && b != c && (a - b).abs() < rail_high * 0.1 {
            counts[i] = 0.5 * (a + b);
            fixed += 1;
        }
    }
    (counts, fixed)
}

fn to_mv(v: f32) -> i16 {
    (v * 1000.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Base64, because the frame travels as JSON.
///
/// The envelope is ~38 KB for eight channels at 1200 columns; as a JSON array of
/// numbers that would be five times larger and parse five times slower, so the
/// bytes go through this instead of pulling in a dependency.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger;

    fn snapshot(channels: Vec<usize>, samples: Vec<Vec<f32>>, len: f64) -> Snapshot {
        Snapshot {
            id: 42,
            generation: 3,
            dt_s: 1.0 / 50_000.0,
            channels,
            samples,
            start: 0.0,
            len,
            n_block: 4000,
            cfg: Config::default(),
            free: false,
            pct: 30.0,
            clipped_pre: false,
            clipped_post: false,
            range_v: [20.0; CHANNELS],
            range_id: [3; CHANNELS],
            center_v: [0.0; CHANNELS],
            autoscale: [false; CHANNELS],
            stats: vec![Measure::default()],
            saturation: vec![false],
            dropouts: vec![0],
            peak_min: [f32::NAN; CHANNELS],
            peak_max: [f32::NAN; CHANNELS],
            hysteresis_v: 0.04,
        }
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn envelope_keeps_a_single_sample_spike() {
        let mut s = vec![0.0f32; 4000];
        s[1234] = 3.0;
        let snap = snapshot(vec![0], vec![s], 3999.0);
        let cols = snap.columns(400);
        // Ten samples share a column here, so the spike shows as the column's
        // maximum while its minimum stays at the baseline.
        let hit = cols[0].iter().any(|(_lo, hi)| *hi >= 3.0);
        assert!(hit, "the spike must survive the compression");
        let peak_columns = cols[0].iter().filter(|(_lo, hi)| *hi >= 3.0).count();
        assert_eq!(peak_columns, 1);
    }

    #[test]
    fn envelope_never_invents_zero_volts() {
        // Ten samples across 1000 columns: most columns are empty and stay so.
        let snap = snapshot(vec![0], vec![vec![1.0; 10]], 999.0);
        let cols = snap.columns(1000);
        let empty = cols[0].iter().filter(|(lo, hi)| lo > hi).count();
        assert!(empty > 900, "empty columns {empty}");
        assert!(cols[0].iter().all(|(lo, hi)| lo > hi || (*lo - 1.0).abs() < 1e-6));
    }

    #[test]
    fn csv_is_relative_to_the_trigger_and_in_volts() {
        let snap = snapshot(vec![0, 1], vec![vec![0.0, 0.5, 1.0], vec![1.0, 1.5, 2.0]], 2.0);
        let csv = snap.csv();
        let rows: Vec<&str> = csv.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(rows[0], "frame,t_s,CH1_V,CH2_V");
        // len 2 samples at 20 µs, trigger at 30 % of 40 µs = 12 µs.
        assert_eq!(rows[1], "42,-0.000012000,0.000000,1.000000");
        assert!(rows[3].contains(",2.000000"), "row: {}", rows[3]);
    }

    #[test]
    fn a_reading_with_too_few_points_per_period_is_flagged_as_an_alias() {
        // 20 µs per sample: a 3.3 kHz reading has three points per period, which
        // is how a 15 kHz bus looks at 10 kHz sampling — a filled band, not a
        // waveform, and a frequency that means nothing.
        let mut snap = snapshot(vec![0], vec![vec![0.0; 4000]], 3999.0);
        snap.dt_s = 1.0 / 10_000.0;
        snap.stats = vec![Measure { freq: Some(3333.0), ..Measure::default() }];
        let meta = snap.meta_json(800, false, "captured", true);
        let ppp = meta["stats"][0]["points_per_period"].as_f64().expect("points");
        assert!((ppp - 3.0).abs() < 0.01, "points per period {ppp}");
        assert_eq!(meta["stats"][0]["aliased"], true);

        // 125 Hz is 400 points per period: the trace and the reading agree.
        snap.stats = vec![Measure { freq: Some(125.0), ..Measure::default() }];
        assert_eq!(snap.meta_json(800, false, "captured", true)["stats"][0]["aliased"], false);

        // No reading at all: nothing is claimed either way.
        snap.stats = vec![Measure::default()];
        let meta = snap.meta_json(800, false, "captured", true);
        assert_eq!(meta["stats"][0]["aliased"], false);
        assert!(meta["stats"][0]["points_per_period"].is_null());
    }

    #[test]
    fn an_alias_that_measures_a_plausible_frequency_is_still_an_alias() {
        // A 15,8 kHz bus at 20 kHz sampling: the frame "measures" 3,3 kHz, i.e.
        // six points per period, so the points test alone would pass it. The
        // samples, however, leap between the rails on most pairs.
        let mut snap = snapshot(vec![0], vec![vec![0.0; 4000]], 3999.0);
        snap.dt_s = 1.0 / 20_000.0;
        let rails: Vec<f32> = (0..1000).map(|i| if i % 3 == 0 { 0.2 } else { 2.5 }).collect();
        snap.samples = vec![rails];
        snap.stats = vec![Measure { freq: Some(3333.0), ..Measure::default() }];
        let meta = snap.meta_json(800, false, "captured", true);
        assert_eq!(meta["stats"][0]["aliased"], true);
        assert_eq!(meta["stats"][0]["alias_reason"], "jumps");
        let jumps = meta["stats"][0]["jump_fraction"].as_f64().expect("jumps");
        assert!(jumps > 0.5, "jump fraction {jumps}");

        // A square wave with six real points per period jumps twice in six.
        let square: Vec<f32> = (0..1000).map(|i| if (i % 6) < 3 { 0.2 } else { 2.5 }).collect();
        snap.samples = vec![square];
        let meta = snap.meta_json(800, false, "captured", true);
        assert_eq!(meta["stats"][0]["aliased"], false, "a real square must not be flagged");

        // A sine at six points per period is smoother still.
        let sine: Vec<f32> = (0..1000)
            .map(|i| 1.35 + 1.15 * ((i as f32) * 6.2832 / 6.0).sin())
            .collect();
        snap.samples = vec![sine];
        let meta = snap.meta_json(800, false, "captured", true);
        assert_eq!(meta["stats"][0]["aliased"], false);
    }

    #[test]
    fn meta_carries_the_numbers_the_ui_labels() {
        let snap = snapshot(vec![0], vec![vec![0.0; 4000]], 3999.0);
        let meta = snap.meta_json(800, true, "captured", true);
        assert_eq!(meta["format"], "dav2");
        assert_eq!(meta["frame_id"], 42);
        assert_eq!(meta["held"], true);
        assert_eq!(meta["trigger"]["source"], 0);
        assert!((meta["sample_rate"].as_f64().unwrap() - 50_000.0).abs() < 1e-6);
        assert!(meta["trigger"]["t0_s"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn window_and_trigger_helpers_agree_with_the_trigger_module() {
        let w = trigger::window(2000.0, 4000, 30.0);
        let mut snap = snapshot(vec![0], vec![vec![0.0; 4000]], w.len);
        snap.start = w.start;
        snap.pct = w.pct;
        snap.dt_s = 20e-6;
        assert!((snap.window_s() - w.len * 20e-6).abs() < 1e-12);
        assert!((snap.trigger_offset_s() - 0.3 * snap.window_s()).abs() < 1e-9);
    }
}
    #[test]
    fn a_single_rail_sample_is_repaired() {
        let mut raw = vec![2048u16; 100];
        raw[40] = 0;
        let (fixed, count) = repair_dropouts(&raw);
        assert_eq!(count, 1);
        assert!((fixed[40] - 2048.0).abs() < 1e-6);
        assert_eq!(fixed[39], 2048.0);
    }

    #[test]
    fn a_run_of_rail_samples_is_left_alone() {
        // Two neighbours both at the rail: this is clipping, not a dropout.
        let mut raw = vec![2048u16; 100];
        raw[40] = 0;
        raw[41] = 0;
        let (_, count) = repair_dropouts(&raw);
        assert_eq!(count, 0);
    }

    #[test]
    fn a_real_step_is_not_touched() {
        let mut raw = vec![2048u16; 100];
        for v in raw.iter_mut().skip(50) {
            *v = 4095;
        }
        let (fixed, count) = repair_dropouts(&raw);
        assert_eq!(count, 0);
        assert_eq!(fixed[50], 4095.0);
    }

    #[test]
    fn short_blocks_are_returned_unchanged() {
        let raw = vec![0u16, 5];
        let (fixed, count) = repair_dropouts(&raw);
        assert_eq!(count, 0);
        assert_eq!(fixed.len(), 2);
    }
