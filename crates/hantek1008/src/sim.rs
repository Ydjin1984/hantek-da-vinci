//! Deterministic simulator.
//!
//! Speaks just enough of the wire protocol for [`crate::Device`] to run with no
//! hardware attached. The UI is developed and the tests run against this, so a
//! missing scope never blocks work on the interesting parts.

use crate::protocol::{mux_slots, ADC_MAX, CHANNELS};
use crate::transport::Transport;
use crate::Result;

/// A fake instrument that answers every command and synthesises waveforms.
pub struct SimTransport {
    enabled: Vec<usize>,
    last: u8,
    phase: u64,
    bytes_since_pause: usize,
    /// Sample rate the simulator pretends to run at.
    pub rate: f64,
}

impl Default for SimTransport {
    fn default() -> Self {
        SimTransport {
            enabled: (0..CHANNELS).collect(),
            last: 0,
            phase: 0,
            bytes_since_pause: 0,
            rate: 50_000.0,
        }
    }
}

impl SimTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// One sample instant: all channels are sampled simultaneously.
    fn sample_at(&self, ch: usize, t: f64) -> u16 {
        let v = match ch {
            0 => (2.0 * std::f64::consts::PI * 1_000.0 * t).sin() * 5.0,
            1 => {
                if (t * 500.0).fract() < 0.3 { 3.2 } else { -0.4 }
            }
            2 => ((t * 37.0).sin() + (t * 311.0).sin() * 0.4) * 2.5,
            3 => (2.0 * std::f64::consts::PI * 50.0 * t).sin() * 12.0,
            4 => (t * 2_000.0).sin() * 0.8 + 0.5,
            5 => 0.0,
            // A rare narrow spike: exactly what a min/max renderer must keep.
            6 => if (t * 3.0).fract() < 0.002 { 8.0 } else { 0.2 },
            _ => (2.0 * std::f64::consts::PI * 120.0 * t).sin() * 1.5,
        };
        let code = 2048.0 + v / 0.01; // ±20 V range
        code.clamp(0.0, ADC_MAX as f64) as u16
    }

    fn payload(&mut self, len: usize) -> Vec<u8> {
        // Mirror the hardware's frame layout exactly: sample groups are the
        // channel count rounded up to even, and the spare slots repeat the last
        // channel. A simulator that packs them tightly cannot catch a demux
        // that assumes it does.
        let slots = mux_slots(self.enabled.len());
        let mut out = Vec::with_capacity(len + 2);
        while out.len() < len {
            let t = self.phase as f64 / self.rate;
            for slot in 0..slots {
                let ch = self.enabled[slot.min(self.enabled.len() - 1)];
                let v = self.sample_at(ch, t);
                out.extend_from_slice(&v.to_le_bytes());
                if out.len() >= len {
                    break;
                }
            }
            self.phase += 1;
        }
        out.truncate(len);
        out
    }
}

impl Transport for SimTransport {
    fn write(&mut self, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.last = buf[0];
        match buf[0] {
            0xA0 => {
                let n = buf[1] as usize;
                if n >= 1 && n <= CHANNELS {
                    self.enabled = (0..n).collect();
                }
            }
            0xAA if buf.len() >= 9 => {
                let mask: Vec<usize> =
                    (0..CHANNELS).filter(|&i| buf[1 + i] != 0).collect();
                self.enabled = if mask.is_empty() { vec![0] } else { mask };
            }
            _ => {}
        }
        Ok(())
    }

    fn read(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut out = match self.last {
            // Bulk sample reads carry no echo. Pace them so the fake link runs
            // at roughly real speed: 2000 samples per channel per buffer.
            0xA6 => {
                let payload = self.payload(len);
                if len >= 64 {
                    self.bytes_since_pause += len;
                    if self.bytes_since_pause >= 4000 {
                        self.bytes_since_pause = 0;
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                return Ok(payload);
            }
            // Buffer length, big endian: 4000 bytes = 2000 samples shared by
            // all enabled channels, as the real device reports it.
            0xC6 => vec![(4000u16 >> 8) as u8, (4000u16 & 0xff) as u8],
            0xC7 => vec![0, 0],
            // Acquisition ready: 2 means "buffer 2 filled", 3 means both.
            0xA5 => vec![0xA5, 0x03],
            0xE5 => vec![0xD6, 0x06],
            0xE6 => vec![0xE9, 0x06, 0xE6, 0x06, 0xE6, 0x06, 0xE7, 0x06, 0xE7, 0x06],
            0xE9 => vec![0x01, 0x09],
            0xF7 | 0xF8 => self.payload(len.min(64)),
            0xFA => self.payload(len.min(56)),
            // Everything else echoes its opcode, then zero padding.
            op => {
                let mut v = vec![op];
                v.resize(len.max(1), 0);
                v
            }
        };
        out.resize(len, 0);
        Ok(out)
    }

    fn describe(&self) -> String {
        "simulator".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_drives_a_device() {
        use crate::Device;
        let mut dev = Device::with_transport(Box::new(SimTransport::new()));
        dev.set_channels(&[0, 1, 2]).unwrap();
        let block = dev.read_block().unwrap();
        assert!(block.n > 0);
        assert_eq!(block.ch[3], None);
        assert!(block.ch[0].is_some());
    }
}
