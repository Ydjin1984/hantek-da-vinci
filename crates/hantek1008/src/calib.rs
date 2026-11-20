//! Per-channel zero offsets.
//!
//! The ADC zero drifts with temperature — the sigrok notes measured tens of
//! millivolts of movement on the ±20 V range during warm-up. A scope that does
//! not compensate for that silently lies, which is exactly the "shows squares,
//! not volts" complaint. We measure the zero per channel *per range* and store
//! it, so every reading is relative to a real zero instead of a nominal 2048.

use crate::protocol::{Range, CHANNELS, ZERO_NOMINAL};
use serde::{Deserialize, Serialize};

/// Nominal + measured zeros for all channels and ranges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calibration {
    /// `zeros[range_index][channel]`, range index 0..=2 as in [`Range::ALL`].
    pub zeros: [[f32; CHANNELS]; 3],
    /// Optional gain correction per channel and range, applied on top.
    #[serde(default)]
    pub gain: [[f32; CHANNELS]; 3],
    /// Unix seconds when the zero pass was taken, `None` if never.
    #[serde(default)]
    pub measured_at: Option<u64>,
    /// Device serial or path the calibration belongs to.
    #[serde(default)]
    pub device: String,
}

impl Default for Calibration {
    fn default() -> Self {
        Calibration {
            zeros: [[ZERO_NOMINAL; CHANNELS]; 3],
            gain: [[1.0; CHANNELS]; 3],
            measured_at: None,
            device: String::new(),
        }
    }
}

impl Calibration {
    /// Zero offset for one channel on one range.
    pub fn zero(&self, range: Range, ch: usize) -> f32 {
        self.zeros[(range as usize) - 1][ch.min(CHANNELS - 1)]
    }

    pub fn set_zero(&mut self, range: Range, ch: usize, value: f32) {
        self.zeros[(range as usize) - 1][ch.min(CHANNELS - 1)] = value;
    }

    pub fn gain(&self, range: Range, ch: usize) -> f32 {
        let g = self.gain[(range as usize) - 1][ch.min(CHANNELS - 1)];
        if g == 0.0 { 1.0 } else { g }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn from_json(s: &str) -> Option<Calibration> {
        serde_json::from_str(s).ok()
    }

    /// Load from `path`, or defaults when the file is missing/corrupt.
    pub fn load(path: &std::path::Path) -> Calibration {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| Calibration::from_json(&s))
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_json())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_neutral() {
        let c = Calibration::default();
        assert_eq!(c.zero(Range::V20, 3), ZERO_NOMINAL);
        assert_eq!(c.gain(Range::V20, 3), 1.0);
    }

    #[test]
    fn roundtrips_through_json() {
        let mut c = Calibration::default();
        c.set_zero(Range::V25, 2, 2010.5);
        let back = Calibration::from_json(&c.to_json()).unwrap();
        assert!((back.zero(Range::V25, 2) - 2010.5).abs() < 1e-6);
    }
}
