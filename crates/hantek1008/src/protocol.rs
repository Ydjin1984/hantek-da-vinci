//! Wire protocol, ranges, time bases and signal measurements.
//!
//! Everything here is pure data: no I/O, no threads. It mirrors what the device
//! actually does, as recovered from the vendor binary and cross-checked against
//! the two public reverse-engineering efforts (`hantek1008py`, libsigrok).

use crate::{Error, Result};

/// Number of analog channels.
pub const CHANNELS: usize = 8;
/// Maximum size of one bulk transfer on either endpoint.
pub const MAX_PACKET: usize = 64;
/// The ADC is 12 bit; samples arrive as little-endian 16-bit words.
pub const ADC_MAX: u16 = 4095;

/// Nominal mid-scale code, used until a real zero offset has been measured.
pub const ZERO_NOMINAL: f32 = 2048.0;

/// 16-bit samples the device returns in one acquisition, across all enabled
/// channels. Measured on hardware: the buffer reports 8000 bytes (`0x1F40`), and
/// that number does not change with the time base or channel count — only the
/// per-channel split does (8 channels -> 500, 4 -> 1000, 1 -> 4000).
pub const SAMPLES_PER_SCREEN: usize = 4000;

/// Real-time sampling rate of the whole instrument, in hertz.
///
/// From the 1008C datasheet: 2.4 MSa/s, 4K memory, 100 kHz bandwidth. The
/// measured buffer of 4000 samples matches the 4K memory depth, which is what
/// makes this number trustworthy enough to compute with.
pub const ADC_TOTAL_HZ: f64 = 2_400_000.0;

/// Input range, selected per channel by command `0xA2`.
///
/// The value sent to the device is `id` (1..=3). Voltage per ADC count is
/// `0.01 * VSCALE`, so the full-scale span is `4096 * 0.01 * vscale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// ±0.41 V — the narrowest range, best resolution, slowest settling.
    V04 = 1,
    /// ±2.56 V
    V25 = 2,
    /// ±20.5 V — the only range that is linear over the whole span according to
    /// the sigrok notes; also the most robust choice for automotive signals.
    V20 = 3,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::V04, Range::V25, Range::V20];

    /// Smallest hardware range that can show ±`volts` without clipping.
    ///
    /// The instrument only has three ranges; anything finer than ±0.41 V is
    /// digital zoom inside that range, and anything wider than ±20 V does not
    /// exist at all.
    pub fn covering(volts: f32) -> Range {
        for r in Range::ALL {
            if r.full_scale() >= volts - 1e-6 {
                return r;
            }
        }
        Range::V20
    }

    /// Protocol id written to the device.
    pub fn id(self) -> u8 {
        self as u8
    }

    /// The `vscale` factor the vendor software uses.
    pub fn vscale(self) -> f32 {
        match self {
            Range::V04 => 0.02,
            Range::V25 => 0.125,
            Range::V20 => 1.0,
        }
    }

    /// Volts per ADC count.
    pub fn volt_per_count(self) -> f32 {
        0.01 * self.vscale()
    }

    /// Positive full-scale voltage (symmetric range).
    pub fn full_scale(self) -> f32 {
        ADC_MAX as f32 * self.volt_per_count() * 0.5
    }

    /// One ADC count in volts, i.e. the physical resolution floor.
    pub fn resolution(self) -> f32 {
        self.volt_per_count()
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::V04 => "±0.41 V",
            Range::V25 => "±2.5 V",
            Range::V20 => "±20 V",
        }
    }

    pub fn from_id(id: u8) -> Result<Range> {
        match id {
            1 => Ok(Range::V04),
            2 => Ok(Range::V25),
            3 => Ok(Range::V20),
            other => Err(Error::Protocol(format!("bad range id {other}"))),
        }
    }

    pub fn from_index(i: usize) -> Range {
        Range::ALL[i.min(2)]
    }
}

/// Horizontal time base, one value for all channels.
///
/// The device takes an index 0..=25; the resulting nanoseconds per division
/// follow the 1-2-5 sequence: 1, 2, 5, 10, 20, 50 ns ... 500 ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeDiv {
    /// Protocol index, 0..=25.
    pub id: u8,
    /// Nanoseconds per division (10 divisions across the screen).
    pub ns_per_div: u64,
}

impl TimeDiv {
    /// Default of the vendor software: 500 µs/div.
    pub const DEFAULT: TimeDiv = TimeDiv { id: 17, ns_per_div: 500_000 };

    pub fn from_id(id: u8) -> Result<TimeDiv> {
        if id > 25 {
            return Err(Error::Protocol(format!("bad time/div id {id}")));
        }
        let mantissa = [1u64, 2, 5][(id % 3) as usize];
        let ns_per_div = mantissa * 10u64.pow((id / 3) as u32);
        Ok(TimeDiv { id, ns_per_div })
    }

    pub fn all() -> Vec<TimeDiv> {
        (0..=25).filter_map(|id| TimeDiv::from_id(id).ok()).collect()
    }

    /// Human readable, e.g. `500 µs/div`.
    pub fn label(&self) -> String {
        let ns = self.ns_per_div;
        if ns >= 1_000_000 {
            format!("{} ms/div", ns as f64 / 1e6)
        } else if ns >= 1_000 {
            format!("{} µs/div", ns as f64 / 1e3)
        } else {
            format!("{ns} ns/div")
        }
    }

    pub fn window_s(&self) -> f64 {
        self.ns_per_div as f64 * 10.0 / 1e9
    }

    /// Sample rate in hertz that this time base implies for `channels` channels.
    ///
    /// The device returns a fixed *byte* count per acquisition — 8000 bytes, i.e.
    /// [`SAMPLES_PER_SCREEN`] 16-bit samples shared between the enabled channels.
    /// The time span is fixed by the time base, so the per-channel rate depends on
    /// how many channels are on: 8 channels at 500 µs/div is 500 samples over
    /// 5 ms, one channel is 4000 samples over the same 5 ms.
    ///
    /// Above the ADC's own ceiling this saturates. Asking for 1 µs/div would need
    /// 200 MSa/s per channel with two channels on; the instrument tops out at
    /// 2.4 MSa/s shared, so it fills the same 4000 samples much more slowly and
    /// the sweep covers [`real_window_s`](Self::real_window_s).
    pub fn sample_rate(&self, channels: usize) -> f64 {
        if channels == 0 {
            return 0.0;
        }
        let per_channel = (SAMPLES_PER_SCREEN / channels).max(1) as f64;
        let demanded = per_channel / self.window_s();
        let ceiling = ADC_TOTAL_HZ / channels as f64;
        demanded.min(ceiling)
    }

    /// Seconds one acquisition really covers.
    ///
    /// Equal to the time base whenever the ADC keeps up, longer when it cannot.
    /// This, not the time base, is what the screen is showing.
    pub fn real_window_s(&self, channels: usize) -> f64 {
        let rate = self.sample_rate(channels);
        if rate <= 0.0 {
            return self.window_s();
        }
        (SAMPLES_PER_SCREEN / channels.max(1)) as f64 / rate
    }
}

/// One acquisition result: `n` raw ADC codes for every enabled channel.
#[derive(Debug, Clone, Default)]
pub struct Block {
    /// Monotonic sequence number, starting at 0.
    pub seq: u64,
    /// Monotonic timestamp in nanoseconds at the moment the block was read.
    pub t_ns: u64,
    /// Samples per channel in this block.
    pub n: usize,
    /// Raw codes 0..=4095. `None` for disabled channels.
    pub ch: [Option<Vec<u16>>; CHANNELS],
}

impl Block {
    pub fn enabled(&self) -> impl Iterator<Item = usize> + '_ {
        (0..CHANNELS).filter(|&c| self.ch[c].is_some())
    }

    /// Convert one channel to volts using a measured zero offset.
    pub fn volts(&self, ch: usize, range: Range, zero: f32) -> Vec<f32> {
        let vpc = range.volt_per_count();
        match &self.ch[ch] {
            Some(raw) => raw.iter().map(|&v| (v as f32 - zero) * vpc).collect(),
            None => Vec::new(),
        }
    }

    /// Min/max envelope, `buckets` columns wide, for pixel-perfect rendering.
    ///
    /// Peaks are never dropped: each column reports the true extremes of the
    /// samples that fall into it, which is what makes a slow sweep look like the
    /// signal instead of a staircase.
    pub fn envelope(&self, ch: usize, buckets: usize) -> Vec<(f32, f32)> {
        let mut out = vec![(f32::INFINITY, f32::NEG_INFINITY); buckets];
        let Some(raw) = &self.ch[ch] else { return out };
        if raw.is_empty() || buckets == 0 {
            return out;
        }
        for (i, &v) in raw.iter().enumerate() {
            let b = i * buckets / raw.len();
            let v = v as f32;
            if v < out[b].0 {
                out[b].0 = v;
            }
            if v > out[b].1 {
                out[b].1 = v;
            }
        }
        for cell in &mut out {
            if !cell.0.is_finite() {
                *cell = (0.0, 0.0);
            }
        }
        out
    }
}

/// Decode interleaved little-endian 16-bit samples into per-channel blocks.
///
/// The device sends samples multiplexed across the enabled channels: sample `i`
/// belongs to channel `enabled[i % n]`.
pub fn demux(bytes: &[u8], enabled: &[usize], seq: u64, t_ns: u64) -> Result<Block> {
    if enabled.is_empty() {
        return Err(Error::Protocol("no enabled channels".into()));
    }
    if bytes.len() % 2 != 0 {
        return Err(Error::Protocol(format!("odd payload length {}", bytes.len())));
    }
    let shorts: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    let n = shorts.len() / enabled.len();
    let mut ch: [Option<Vec<u16>>; CHANNELS] = Default::default();
    for (slot, &c) in enabled.iter().enumerate() {
        if c >= CHANNELS {
            return Err(Error::Protocol(format!("channel {c} out of range")));
        }
        let v: Vec<u16> = shorts.iter().skip(slot).step_by(enabled.len()).copied().collect();
        ch[c] = Some(v);
    }
    Ok(Block { seq, t_ns, n, ch })
}

/// Measurements over one channel of one block.
#[derive(Debug, Clone, Copy, Default)]
pub struct Measure {
    pub vmin: f32,
    pub vmax: f32,
    pub vpp: f32,
    pub mean: f32,
    pub rms: f32,
    pub rms_ac: f32,
    pub freq: Option<f32>,
    pub duty: Option<f32>,
    pub period_s: Option<f32>,
}

impl Measure {
    /// Peak-to-peak and RMS of a window of volts.
    pub fn of(volts: &[f32], dt_s: f32) -> Measure {
        if volts.is_empty() {
            return Measure::default();
        }
        let mut vmin = f32::INFINITY;
        let mut vmax = f32::NEG_INFINITY;
        let mut sum = 0.0f64;
        let mut sq = 0.0f64;
        for &v in volts {
            if v < vmin {
                vmin = v;
            }
            if v > vmax {
                vmax = v;
            }
            sum += v as f64;
            sq += (v as f64) * (v as f64);
        }
        let n = volts.len() as f64;
        let mean = (sum / n) as f32;
        let rms = (sq / n).sqrt() as f32;
        let mut m = Measure {
            vmin,
            vmax,
            vpp: vmax - vmin,
            mean,
            rms,
            rms_ac: (rms * rms - mean * mean).max(0.0).sqrt(),
            ..Default::default()
        };
        if dt_s > 0.0 {
            if let Some(period) = estimate_period(volts, dt_s) {
                m.period_s = Some(period);
                m.freq = Some(1.0 / period);
                m.duty = duty_cycle(volts, dt_s, period, (vmin + vmax) * 0.5);
            }
        }
        m
    }
}

/// Rising-edge period estimate with hysteresis, median filtered.
fn estimate_period(v: &[f32], dt_s: f32) -> Option<f32> {
    if v.len() < 16 {
        return None;
    }
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &x in v {
        lo = lo.min(x);
        hi = hi.max(x);
    }
    let amp = hi - lo;
    if amp < 1e-4 {
        return None;
    }
    let mid = (hi + lo) * 0.5;
    let hyst = amp * 0.10;
    let mut edges = Vec::new();
    let mut armed = v[0] < mid;
    let mut last = 0usize;
    for (i, &x) in v.iter().enumerate() {
        if armed && x > mid + hyst {
            if last > 0 || !edges.is_empty() {
                edges.push(i);
            }
            armed = false;
            last = i;
        } else if !armed && x < mid - hyst {
            armed = true;
            last = i;
        }
    }
    if edges.len() < 2 {
        return None;
    }
    let mut periods: Vec<f32> = edges.windows(2).map(|w| (w[1] - w[0]) as f32 * dt_s).collect();
    periods.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = periods[periods.len() / 2];
    if median <= 0.0 {
        return None;
    }
    // Reject the estimate when the edges are not regularly spaced.
    let ok = periods
        .iter()
        .filter(|&&p| (p - median).abs() < median * 0.25)
        .count();
    if ok * 3 < periods.len() * 2 {
        return None;
    }
    Some(median)
}

/// Fraction of the period spent above the mid level.
fn duty_cycle(v: &[f32], dt_s: f32, period: f32, mid: f32) -> Option<f32> {
    let high = v.iter().filter(|&&x| x > mid).count();
    let frac = high as f32 / v.len() as f32;
    let _ = (dt_s, period);
    if frac.is_finite() {
        Some(frac)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_match_the_adc() {
        assert!((Range::V20.full_scale() - 20.48).abs() < 0.01);
        assert!((Range::V25.full_scale() - 2.56).abs() < 0.01);
        assert!((Range::V04.full_scale() - 0.41).abs() < 0.01);
        assert_eq!(Range::V20.id(), 3);
        assert_eq!(Range::from_id(1).unwrap(), Range::V04);
    }

    #[test]
    fn the_adc_ceiling_stretches_the_window() {
        // 1 µs/div with two channels would need 200 MSa/s per channel. The
        // instrument tops out at 2.4 MSa/s shared, so 2000 samples take
        // 2000 / 1.2e6 ≈ 1.67 ms however the dial is set.
        let td = TimeDiv::from_id(9).unwrap();
        assert_eq!(td.ns_per_div, 1_000);
        assert!((td.window_s() - 1e-5).abs() < 1e-15);
        assert!((td.sample_rate(2) - 1_200_000.0).abs() < 1.0);
        assert!((td.real_window_s(2) - 1.666_666_7e-3).abs() < 1e-9);

        // Slow enough for the ADC, the dial is honoured exactly.
        let slow = TimeDiv::from_id(17).unwrap();
        assert!((slow.sample_rate(2) - 400_000.0).abs() < 1.0);
        assert!((slow.real_window_s(2) - slow.window_s()).abs() < 1e-12);
    }

    #[test]
    fn the_chosen_scale_picks_the_smallest_range_that_covers_it() {
        // Finer than the instrument's narrowest range is digital zoom, so the
        // hardware stays on ±0.41 V; wider steps up through ±2.5 V to ±20 V.
        assert_eq!(Range::covering(0.1), Range::V04);
        assert_eq!(Range::covering(0.2), Range::V04);
        assert_eq!(Range::covering(0.4), Range::V04);
        assert_eq!(Range::covering(0.5), Range::V25);
        assert_eq!(Range::covering(2.5), Range::V25);
        assert_eq!(Range::covering(3.0), Range::V20);
        assert_eq!(Range::covering(20.0), Range::V20);
    }

    #[test]
    fn sample_rate_follows_the_time_base_not_the_link() {
        // 500 µs/div is a 5 ms screen; 4000 samples are shared between channels,
        // so eight channels give 500 each and one channel gives all 4000.
        let td = TimeDiv::from_id(17).unwrap();
        assert!((td.window_s() - 0.005).abs() < 1e-12);
        assert!((td.sample_rate(8) - 100_000.0).abs() < 1e-6);
        assert!((td.sample_rate(4) - 200_000.0).abs() < 1e-6);
        assert!((td.sample_rate(1) - 800_000.0).abs() < 1e-6);
        assert_eq!(td.sample_rate(0), 0.0);
    }

    #[test]
    fn time_div_follows_1_2_5() {        assert_eq!(TimeDiv::from_id(17).unwrap().ns_per_div, 500_000);
        assert_eq!(TimeDiv::from_id(0).unwrap().ns_per_div, 1);
        assert_eq!(TimeDiv::from_id(3).unwrap().ns_per_div, 10);
        assert_eq!(TimeDiv::from_id(25).unwrap().ns_per_div, 200_000_000);
    }

    #[test]
    fn demux_splits_interleaved_samples() {
        // 2 channels, 4 samples each, ch0 first.
        let mut bytes = Vec::new();
        for i in 0..4u16 {
            bytes.extend_from_slice(&(100 + i).to_le_bytes()); // ch0
            bytes.extend_from_slice(&(900 + i).to_le_bytes()); // ch1
        }
        let b = demux(&bytes, &[0, 1], 7, 42).unwrap();
        assert_eq!(b.seq, 7);
        assert_eq!(b.n, 4);
        assert_eq!(b.ch[0].as_ref().unwrap(), &[100, 101, 102, 103]);
        assert_eq!(b.ch[1].as_ref().unwrap(), &[900, 901, 902, 903]);
        assert!(b.ch[2].is_none());
    }

    #[test]
    fn envelope_keeps_extremes() {
        let mut b = Block::default();
        b.n = 100;
        let mut v: Vec<u16> = (0..100).map(|i| 2000 + (i % 5) as u16).collect();
        v[37] = 4000; // a single fast peak
        b.ch[0] = Some(v);
        let env = b.envelope(0, 10);
        assert_eq!(env.len(), 10);
        assert_eq!(env[3].1, 4000.0);
    }

    #[test]
    fn measures_a_square_wave() {
        // 1 kHz, 25% duty, 0..1 V, 100 kS/s
        let n = 1000;
        let dt = 1.0 / 100_000.0;
        let v: Vec<f32> = (0..n)
            .map(|i| if (i as f32 * dt * 1000.0).fract() < 0.25 { 1.0 } else { 0.0 })
            .collect();
        let m = Measure::of(&v, dt);
        let f = m.freq.unwrap();
        assert!((f - 1000.0).abs() < 30.0, "freq {f}");
        let d = m.duty.unwrap();
        assert!((d - 0.25).abs() < 0.05, "duty {d}");
        assert!((m.vpp - 1.0).abs() < 0.01);
    }
}
