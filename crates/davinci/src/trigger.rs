//! Software trigger.
//!
//! The instrument has no trigger hardware of its own that we can reach: it hands
//! back one screen of samples per USB read and nothing else. So the trigger is
//! computed here, on the same calibrated samples the trace is drawn from, and its
//! only job is to choose *where the window starts* inside that screen.
//!
//! Two consequences are worth stating plainly, because they shape everything
//! below:
//!
//! * A block is a continuous segment; the gap between two blocks is a USB round
//!   trip of unknown length. Blocks are never stitched, so a trigger can only be
//!   searched inside one block, and the published window can never be wider than
//!   that block. When the crossing sits near an edge, the window is *shorter*
//!   than the screen and says so.
//! * The window is positioned so the crossing lands at the requested fraction of
//!   its width (the "pretrigger" percentage). That is what makes a periodic
//!   signal stand still on screen, and it is exactly what the acceptance test
//!   measures.
//!
//! Everything in this module is pure: no clocks, no device, no locks. The engine
//! owns the state machine and the timers, and feeds these functions.

use serde::{Deserialize, Serialize};

/// Where the window starts when nobody says otherwise: a third of the screen of
/// history before the edge, the rest after it.
pub const DEFAULT_PRETRIGGER_PCT: f32 = 30.0;
/// How long Auto waits for an edge before publishing a free-running frame.
pub const DEFAULT_TIMEOUT_MS: u32 = 250;
pub const MIN_PRETRIGGER_PCT: f32 = 10.0;
pub const MAX_PRETRIGGER_PCT: f32 = 90.0;
/// Levels beyond this are a typo, not a setting: the instrument's widest range
/// is ±20 V.
pub const MAX_LEVEL_V: f32 = 25.0;
pub const MAX_TIMEOUT_MS: u32 = 10_000;

/// Which slope counts as an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Rise,
    Fall,
}

impl Edge {
    pub fn as_str(self) -> &'static str {
        match self {
            Edge::Rise => "rise",
            Edge::Fall => "fall",
        }
    }

    pub fn from_str(s: &str) -> Option<Edge> {
        match s {
            "rise" | "up" | "↑" => Some(Edge::Rise),
            "fall" | "down" | "↓" => Some(Edge::Fall),
            _ => None,
        }
    }
}

/// What the acquisition does when no edge is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Publish every block as it arrives.
    Free,
    /// Wait for an edge, but publish a free frame after a timeout.
    Auto,
    /// Wait for an edge and hold the last frame until one arrives.
    Normal,
    /// Capture exactly one frame per arm, then hold it.
    Single,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Free => "free",
            Mode::Auto => "auto",
            Mode::Normal => "normal",
            Mode::Single => "single",
        }
    }

    pub fn from_str(s: &str) -> Option<Mode> {
        match s {
            "free" => Some(Mode::Free),
            "auto" => Some(Mode::Auto),
            "normal" => Some(Mode::Normal),
            "single" => Some(Mode::Single),
            _ => None,
        }
    }
}

/// Everything the user can set about the trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub mode: Mode,
    /// Channel index 0..8 the edge is searched on.
    pub source: usize,
    pub edge: Edge,
    /// Threshold in volts, on the same calibrated scale as the trace.
    pub level_v: f32,
    /// Where in the window the edge is placed, 10..90 %.
    pub pretrigger_pct: f32,
    /// Auto's patience, in milliseconds.
    pub timeout_ms: u32,
    /// Hysteresis in volts. `None` means "derive it from the range".
    pub hysteresis_v: Option<f32>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            // Free by default: with no signal connected a waiting trigger shows a
            // frozen screen and looks broken, while a free sweep shows the noise
            // honestly and proves the link is alive.
            mode: Mode::Free,
            source: 0,
            edge: Edge::Rise,
            level_v: 0.0,
            pretrigger_pct: DEFAULT_PRETRIGGER_PCT,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            hysteresis_v: None,
        }
    }
}

impl Config {
    /// Reject nonsense before it reaches the device thread.
    ///
    /// The HTTP layer turns these strings into 4xx responses; nothing here is
    /// allowed to silently clamp, because a clamped setting that the UI then
    /// shows as applied is a lie about what was measured.
    pub fn validate(&self) -> Result<(), String> {
        if self.source >= hantek1008::CHANNELS {
            return Err(format!("источник триггера {} вне диапазона 1..8", self.source + 1));
        }
        if !self.level_v.is_finite() {
            return Err("уровень триггера должен быть числом".into());
        }
        if self.level_v.abs() > MAX_LEVEL_V {
            return Err(format!(
                "уровень {:.3} В вне диапазона ±{MAX_LEVEL_V} В",
                self.level_v
            ));
        }
        if !self.pretrigger_pct.is_finite()
            || self.pretrigger_pct < MIN_PRETRIGGER_PCT
            || self.pretrigger_pct > MAX_PRETRIGGER_PCT
        {
            return Err(format!(
                "предыстория {:.1}% вне диапазона {MIN_PRETRIGGER_PCT}..{MAX_PRETRIGGER_PCT}%",
                self.pretrigger_pct
            ));
        }
        if self.timeout_ms > MAX_TIMEOUT_MS {
            return Err(format!("таймаут {ms} мс больше {MAX_TIMEOUT_MS} мс", ms = self.timeout_ms));
        }
        if let Some(h) = self.hysteresis_v {
            if !h.is_finite() || h < 0.0 {
                return Err("гистерезис должен быть неотрицательным числом".into());
            }
        }
        Ok(())
    }

    /// Hysteresis actually used for this range.
    ///
    /// Four ADC counts is above the converter's own noise on every range, and the
    /// floor of 2 mV keeps a quiet ±0.41 V input from triggering on the last bit.
    /// Without it, a signal sitting exactly on the level produced a burst of
    /// frames per block, each claiming a different "edge".
    pub fn hysteresis(&self, resolution_v: f32) -> f32 {
        self.hysteresis_v
            .unwrap_or_else(|| (4.0 * resolution_v).max(0.002))
    }
}

/// Where the published window sits inside the block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// First sample index (fractional) of the window.
    pub start: f64,
    /// Width of the window in samples (fractional, never more than the block).
    pub len: f64,
    /// Fractional sample index of the trigger crossing, if one was used.
    pub trig: Option<f64>,
    /// Where the crossing landed, as a percentage of the window width.
    pub pct: f32,
    /// The block ran out of history before the requested pretrigger.
    pub clipped_pre: bool,
    /// The block ran out of samples after the crossing.
    pub clipped_post: bool,
}

impl Window {
    /// A window covering the whole block, no trigger.
    pub fn whole(n_total: usize) -> Window {
        Window {
            start: 0.0,
            len: (n_total.saturating_sub(1)) as f64,
            trig: None,
            pct: 0.0,
            clipped_pre: false,
            clipped_post: false,
        }
    }
}

/// Place a window around `trig` so the crossing lands at `pre_pct` of its width.
///
/// The block is exactly one screen wide, so an aligned window has to be a
/// *sub-window* of it: a full-width window can only ever start at sample 0. The
/// longest window that still puts the crossing at `p` percent is limited by the
/// history before the edge (`trig / p`) and by the samples after it
/// (`(span - trig) / (1 - p)`); the code takes the smaller of those and the block.
///
/// A quarter of the block is the floor. Without it, an edge a few samples from
/// the start would leave a sliver of a window — technically aligned, practically
/// unreadable. When the floor or the block edge wins, the crossing no longer sits
/// at the requested fraction; `pct` reports where it actually is and the clip
/// flags say which side ran out.
pub fn window(trig: f64, n_total: usize, pre_pct: f32) -> Window {
    let n = n_total as f64;
    if n_total < 2 || !trig.is_finite() {
        return Window::whole(n_total);
    }
    let span = n - 1.0;
    let p = (pre_pct.clamp(MIN_PRETRIGGER_PCT, MAX_PRETRIGGER_PCT) as f64) / 100.0;
    let trig = trig.clamp(0.0, span);

    let before = if p > 0.0 { trig / p } else { span };
    let after = if p < 1.0 { (span - trig) / (1.0 - p) } else { span };
    let longest = span.min(before).min(after);
    let floor = span * 0.25;
    let len = longest.clamp(floor.min(span), span).max(1.0).min(span);

    // Slide the window so the crossing sits at p, then keep it inside the block.
    let mut start = (trig - p * len).clamp(0.0, (span - len).max(0.0));
    let mut clipped_pre = false;
    let mut clipped_post = false;
    if trig - p * len < 0.0 {
        clipped_pre = true;
        start = 0.0;
    }
    if trig + (1.0 - p) * len > span {
        clipped_post = true;
        start = (span - len).max(0.0);
    }
    let pct = (((trig - start) / len) * 100.0).clamp(0.0, 100.0) as f32;

    Window {
        start,
        len,
        trig: Some(trig),
        pct,
        clipped_pre,
        clipped_post,
    }
}

/// First crossing of `level` in `samples[first..=last]` for `edge`.
///
/// `samples` must be calibrated volts. `hysteresis` is the band the signal has to
/// leave on the opposite side before an edge counts: the detector arms after a
/// sample beyond `level ∓ hysteresis` and only then accepts the pair
/// `prev < level <= curr` (or the mirror for a falling edge). The crossing itself
/// is interpolated between the two samples, which is what lets the window land
/// within one sample of the requested position even when the edge falls between
/// two ADC readings.
pub fn find_crossing(
    samples: &[f32],
    level: f32,
    edge: Edge,
    hysteresis: f32,
    first: usize,
    last: usize,
) -> Option<f64> {
    if samples.len() < 2 || !level.is_finite() {
        return None;
    }
    let last = last.min(samples.len() - 1);
    let first = first.max(1);
    if first > last {
        return None;
    }
    let h = hysteresis.max(0.0);
    let mut armed = false;
    for i in first..=last {
        let prev = samples[i - 1];
        let cur = samples[i];
        match edge {
            Edge::Rise => {
                if cur <= level - h {
                    armed = true;
                }
                if armed && prev < level && cur >= level {
                    let d = cur - prev;
                    let f = if d.abs() > f32::EPSILON { (level - prev) / d } else { 0.0 };
                    return Some((i - 1) as f64 + f.clamp(0.0, 1.0) as f64);
                }
            }
            Edge::Fall => {
                if cur >= level + h {
                    armed = true;
                }
                if armed && prev > level && cur <= level {
                    let d = prev - cur;
                    let f = if d.abs() > f32::EPSILON { (prev - level) / d } else { 0.0 };
                    return Some((i - 1) as f64 + f.clamp(0.0, 1.0) as f64);
                }
            }
        }
    }
    None
}

/// Search a whole block for the first usable edge, skipping the first sample.
pub fn first_crossing(samples: &[f32], cfg: &Config, hysteresis: f32) -> Option<f64> {
    find_crossing(samples, cfg.level_v, cfg.edge, hysteresis, 1, samples.len().saturating_sub(1))
}

/// The edge that leaves the widest window, or `None` when none is suitable.
///
/// A crossing a few samples into the block cannot be placed at 30 % of a window:
/// there is no history before it. The spec is explicit about that case — "если
/// фронт у края и буфера не хватает, искать следующий подходящий фронт" — so the
/// detector walks the block and keeps the crossing that allows the longest
/// aligned window. For a periodic signal that is also the one nearest the middle,
/// which is what makes the trace stand still *and* fill the screen.
pub fn best_crossing(samples: &[f32], cfg: &Config, hysteresis: f32) -> Option<f64> {
    let n = samples.len();
    if n < 2 {
        return None;
    }
    let span = n as f64 - 1.0;
    let p = (cfg.pretrigger_pct.clamp(MIN_PRETRIGGER_PCT, MAX_PRETRIGGER_PCT) as f64) / 100.0;
    let floor = span * 0.25;

    let mut best: Option<(f64, f64)> = None;
    let mut from = 1usize;
    while from < n {
        let Some(trig) = find_crossing(samples, cfg.level_v, cfg.edge, hysteresis, from, n - 1)
        else {
            break;
        };
        let before = if p > 0.0 { trig / p } else { span };
        let after = if p < 1.0 { (span - trig) / (1.0 - p) } else { span };
        let len = span.min(before).min(after);
        if len >= floor && best.map(|(_, l)| len > l + 1e-9).unwrap_or(true) {
            best = Some((trig, len));
        }
        let next = (trig.floor() as usize + 2).min(n - 1);
        if next <= from {
            break;
        }
        from = next;
    }
    best.map(|(trig, _)| trig)
}

/// What one acquisition should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Publish a frame aligned to the edge.
    Publish,
    /// Publish a free-running frame because Auto timed out.
    PublishFree,
    /// Publish nothing; keep showing the previous frame.
    Wait,
}

/// The mode logic, with no state and no clock.
///
/// `found` says whether an edge was seen in this block; `awaiting_ms` is how long
/// the caller has been waiting for one.
pub fn decide(mode: Mode, found: bool, awaiting_ms: u64, timeout_ms: u32) -> Outcome {
    match mode {
        Mode::Free => Outcome::PublishFree,
        Mode::Auto => {
            if found {
                Outcome::Publish
            } else if awaiting_ms >= timeout_ms as u64 {
                Outcome::PublishFree
            } else {
                Outcome::Wait
            }
        }
        Mode::Normal | Mode::Single => {
            if found {
                Outcome::Publish
            } else {
                Outcome::Wait
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 50_000.0;

    fn sine(freq: f64, phase: f64, amp: f32, offset: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 * DT;
                offset + amp * ((2.0 * std::f64::consts::PI * freq * t + phase) as f32).sin()
            })
            .collect()
    }

    /// Sample index where a rising crossing of `level` must fall, analytically.
    fn expected_rise(freq: f64, phase: f64, amp: f32, offset: f32, level: f32) -> f64 {
        let arg = ((level - offset) / amp) as f64;
        let s = arg.asin();
        // First non-negative solution of 2πft + φ = asin(...) + 2πk
        let mut t = (s - phase) / (2.0 * std::f64::consts::PI * freq);
        while t < 0.0 {
            t += 1.0 / freq;
        }
        t / DT
    }

    #[test]
    fn crossing_matches_the_waveform_within_a_sample() {
        let s = sine(125.0, 0.0, 1.0, 0.0, 4000);
        let got = find_crossing(&s, 0.5, Edge::Rise, 0.01, 1, s.len() - 1).expect("edge");
        let want = expected_rise(125.0, 0.0, 1.0, 0.0, 0.5);
        assert!((got - want).abs() < 1.0, "found {got}, expected {want}");
    }

    #[test]
    fn arbitrary_phase_and_offset_still_land_on_the_edge() {
        let s = sine(125.0, 1.234, 1.26, 0.17, 4000);
        let got = find_crossing(&s, 0.8, Edge::Rise, 0.01, 1, s.len() - 1).expect("edge");
        let prev = got.floor() as usize;
        let next = prev + 1;
        assert!(s[prev] < 0.8 && s[next] >= 0.8, "crossing {got} not bracketed by {} and {}", s[prev], s[next]);
        assert!((s[prev] - 0.8).abs() < 0.2 && (s[next] - 0.8).abs() < 0.2);
    }

    #[test]
    fn falling_edge_is_the_mirror_image() {
        let s = sine(125.0, 0.4, 1.0, 0.0, 4000);
        let rise = find_crossing(&s, 0.0, Edge::Rise, 0.01, 1, s.len() - 1).unwrap();
        let fall = find_crossing(&s, 0.0, Edge::Fall, 0.01, (((rise as usize) + 2).min(s.len() - 1)), s.len() - 1).unwrap();
        // Half a period later, i.e. 200 samples at 50 kHz / 125 Hz.
        assert!(((fall - rise) - 200.0).abs() < 2.0, "rise {rise}, fall {fall}");
    }

    #[test]
    fn level_outside_the_signal_finds_nothing() {
        let s = sine(125.0, 0.0, 0.5, 0.0, 4000);
        assert!(find_crossing(&s, 2.0, Edge::Rise, 0.01, 1, s.len() - 1).is_none());
        assert!(find_crossing(&s, -2.0, Edge::Fall, 0.01, 1, s.len() - 1).is_none());
    }

    #[test]
    fn constant_signal_on_the_level_finds_nothing() {
        let s = vec![0.5f32; 4000];
        assert!(find_crossing(&s, 0.5, Edge::Rise, 0.01, 1, s.len() - 1).is_none());
        assert!(find_crossing(&s, 0.5, Edge::Fall, 0.01, 1, s.len() - 1).is_none());
    }

    #[test]
    fn hysteresis_ignores_noise_around_the_level() {
        // ±1 mV of noise on a level of 0 V, hysteresis 8 mV.
        let s: Vec<f32> = (0..4000)
            .map(|i| if i % 2 == 0 { 0.001 } else { -0.001 })
            .collect();
        assert!(find_crossing(&s, 0.0, Edge::Rise, 0.008, 1, s.len() - 1).is_none());
        // The same samples without hysteresis do cross, which proves the test.
        assert!(find_crossing(&s, 0.0, Edge::Rise, 0.0, 1, s.len() - 1).is_some());
    }

    #[test]
    fn crossing_is_interpolated_between_samples() {
        // A ramp that crosses 0.5 exactly a quarter of the way into sample 123.
        let s: Vec<f32> = (0..200).map(|i| (i as f32) * 0.01).collect();
        let got = find_crossing(&s, 1.2325, Edge::Rise, 0.0, 1, s.len() - 1).unwrap();
        assert!((got - 123.25).abs() < 1e-5, "got {got}");
    }

    #[test]
    fn window_puts_the_edge_at_the_requested_fraction() {
        let w = window(2000.0, 4000, 30.0);
        assert!((w.pct - 30.0).abs() < 1e-3, "pct {}", w.pct);
        assert_eq!(w.trig, Some(2000.0));
        assert!(!w.clipped_pre && !w.clipped_post);
        // Aligning the edge costs width: the window is the longest one that can
        // still hold the crossing at 30 % of its own width.
        let want_len = (3999.0 - 2000.0) / 0.7;
        assert!((w.len - want_len).abs() < 1e-6, "len {} want {want_len}", w.len);
        assert!((w.start - (2000.0 - 0.3 * w.len)).abs() < 1e-6);
        assert!(w.start + w.len <= 3999.0 + 1e-9);
    }

    #[test]
    fn window_flags_a_missing_pretrigger() {
        let w = window(100.0, 4000, 30.0);
        assert!(w.clipped_pre);
        assert_eq!(w.start, 0.0);
        assert!(w.pct < 30.0, "pct {}", w.pct);
        // The floor keeps a readable window instead of a sliver.
        assert!(w.len >= 3999.0 * 0.25 - 1e-9);
    }

    #[test]
    fn window_flags_a_missing_posttrigger() {
        let w = window(3900.0, 4000, 30.0);
        assert!(w.clipped_post, "expected post clip, got {w:?}");
        assert!(w.len < 3999.0);
        assert!(w.pct > 30.0, "pct {}", w.pct);
        assert!((w.start + w.len) - 3999.0 < 1e-6);
    }

    #[test]
    fn window_of_a_tiny_block_is_still_sane() {
        let w = window(1.0, 2, 50.0);
        assert!(w.len >= 1.0);
        assert!(w.start >= 0.0);
    }

    #[test]
    fn decisions_follow_the_mode_table() {
        assert_eq!(decide(Mode::Free, false, 0, 250), Outcome::PublishFree);
        assert_eq!(decide(Mode::Auto, true, 0, 250), Outcome::Publish);
        assert_eq!(decide(Mode::Auto, false, 10, 250), Outcome::Wait);
        assert_eq!(decide(Mode::Auto, false, 250, 250), Outcome::PublishFree);
        assert_eq!(decide(Mode::Normal, false, 9_000, 250), Outcome::Wait);
        assert_eq!(decide(Mode::Normal, true, 0, 250), Outcome::Publish);
        assert_eq!(decide(Mode::Single, false, 9_000, 250), Outcome::Wait);
        assert_eq!(decide(Mode::Single, true, 0, 250), Outcome::Publish);
    }

    #[test]
    fn validation_rejects_what_the_ui_must_not_send() {
        let mut c = Config::default();
        assert!(c.validate().is_ok());

        c.level_v = f32::NAN;
        assert!(c.validate().is_err());

        c = Config::default();
        c.level_v = 30.0;
        assert!(c.validate().is_err());

        c = Config::default();
        c.source = 8;
        assert!(c.validate().is_err());

        c = Config::default();
        c.pretrigger_pct = 95.0;
        assert!(c.validate().is_err());

        c = Config::default();
        c.timeout_ms = 60_000;
        assert!(c.validate().is_err());

        c = Config::default();
        c.hysteresis_v = Some(-1.0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn hysteresis_default_follows_the_range() {
        let c = Config::default();
        // ±20 V range: 10 mV per count, so four counts is 40 mV.
        assert!((c.hysteresis(0.01) - 0.04).abs() < 1e-6);
        // ±0.41 V range: 1 mV per count, but the 2 mV floor wins.
        assert!((c.hysteresis(0.001) - 0.004).abs() < 1e-6);
    }

    #[test]
    fn names_round_trip() {
        for e in [Edge::Rise, Edge::Fall] {
            assert_eq!(Edge::from_str(e.as_str()), Some(e));
        }
        for m in [Mode::Free, Mode::Auto, Mode::Normal, Mode::Single] {
            assert_eq!(Mode::from_str(m.as_str()), Some(m));
        }
    }
}

#[cfg(test)]
mod best_tests {
    use super::*;

    #[test]
    fn best_crossing_skips_an_edge_without_history() {
        // 125 Hz sine starting at phase 0: the first rising crossing of 0.5 V is
        // ~33 samples in, which cannot hold a 30 % pretrigger. A later one can.
        let rate = 50_000.0;
        let s: Vec<f32> = (0..4000)
            .map(|i| ((2.0 * std::f64::consts::PI * 125.0 * i as f64 / rate) as f32).sin())
            .collect();
        let cfg = Config { level_v: 0.5, ..Config::default() };
        let first = first_crossing(&s, &cfg, 0.01).unwrap();
        assert!(first < 100.0, "first crossing {first}");
        let best = best_crossing(&s, &cfg, 0.01).expect("a suitable edge");
        assert!(best > first, "best {best} must be later than {first}");
        let w = window(best, s.len(), cfg.pretrigger_pct);
        assert!((w.pct - 30.0).abs() < 0.5, "pct {}", w.pct);
        assert!(w.len > 3000.0, "widest window was not chosen: {}", w.len);
    }

    #[test]
    fn best_crossing_returns_none_when_no_edge_has_history() {
        let mut s = vec![0.0f32; 4000];
        // One edge, right at the start.
        for (i, v) in s.iter_mut().enumerate() {
            *v = if i > 10 { 1.0 } else { -1.0 };
        }
        let cfg = Config { level_v: 0.0, ..Config::default() };
        assert!(best_crossing(&s, &cfg, 0.01).is_none());
        assert!(first_crossing(&s, &cfg, 0.01).is_some());
    }
}
