//! High-level device object: init, configuration, acquisition.
//!
//! The sequence below is the one the vendor software performs and the one both
//! public reverse-engineering efforts reproduce. Two properties matter:
//!
//! * The device drops off USB if it does not receive a command for ~7 s, so a
//!   ping (`0xF3`) is part of every acquisition cycle and of the idle keepalive.
//! * Every command is echoed back with its own opcode as the first byte, which
//!   doubles as a cheap framing check.

use crate::protocol::{
    demux, Block, Range, TimeDiv, ADC_MAX, CHANNELS,
};
use crate::transport::Transport;
use crate::{Calibration, Error, Result};
use std::time::{Duration, Instant};

/// What the UI and logs report about the link.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub link: String,
    pub channels: usize,
    /// Samples per second per channel, measured rather than assumed.
    pub sample_rate: f64,
    /// Samples per channel in one block.
    pub block_samples: usize,
    /// Blocks per second the link actually sustains.
    pub blocks_per_s: f64,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        DeviceInfo {
            link: String::new(),
            channels: CHANNELS,
            sample_rate: 0.0,
            block_samples: 0,
            blocks_per_s: 0.0,
        }
    }
}

/// An open, configured oscilloscope.
pub struct Device {
    io: Box<dyn Transport>,
    pub info: DeviceInfo,
    /// Enabled channel indexes, ascending.
    pub enabled: Vec<usize>,
    /// Range per channel.
    pub ranges: [Range; CHANNELS],
    pub time_div: TimeDiv,
    pub calib: Calibration,
    seq: u64,
    t0: Instant,
}

impl Device {
    /// Open the first instrument found over WinUSB.
    #[cfg(windows)]
    pub fn open() -> Result<Device> {
        let io = crate::transport::open_default()?;
        let link = io.describe();
        let mut dev = Device {
            io,
            info: DeviceInfo { link, ..Default::default() },
            enabled: (0..CHANNELS).collect(),
            ranges: [Range::V20; CHANNELS],
            time_div: TimeDiv::DEFAULT,
            calib: Calibration::default(),
            seq: 0,
            t0: Instant::now(),
        };
        // `0xB0` is the handshake the vendor DLL sends before anything else.
        dev.ping()?;
        Ok(dev)
    }

    /// No native transport on this platform yet; use [`Device::with_transport`].
    #[cfg(not(windows))]
    pub fn open() -> Result<Device> {
        Err(Error::NoDevice)
    }

    /// Wrap an arbitrary transport (used by tests and the simulator).
    pub fn with_transport(io: Box<dyn Transport>) -> Device {
        Device {
            info: DeviceInfo { link: io.describe(), ..Default::default() },
            io,
            enabled: (0..CHANNELS).collect(),
            ranges: [Range::V20; CHANNELS],
            time_div: TimeDiv::DEFAULT,
            calib: Calibration::default(),
            seq: 0,
            t0: Instant::now(),
        }
    }

    // ----- low level -------------------------------------------------------

    /// Send a command and consume whatever the device answers.
    ///
    /// Firmware revisions disagree about the shape of the acknowledgement: some
    /// echo the opcode, some return a status byte, some a table of a different
    /// length, some nothing at all. Insisting on an echo (as the reference
    /// implementation does) desynchronises the stream on our unit, so the safe
    /// common denominator is "send, then drain".
    fn cmd(&mut self, bytes: &[u8]) -> Result<()> {
        self.io.write(bytes)?;
        let _ = self.io.drain(256, 40)?;
        Ok(())
    }

    /// Send a command and drain whatever the device answers.
    ///
    /// Firmware revisions disagree about how much they answer (some echo the
    /// opcode, some return a status byte, some return a table of a different
    /// length), so the only portable way to stay in sync is to consume
    /// everything that arrives and not insist on a shape.
    fn send_drain(&mut self, bytes: &[u8]) -> Result<()> {
        self.io.write(bytes)?;
        let _ = self.io.drain(256, 40)?;
        Ok(())
    }

    /// Send a command, check the echo, return the payload that follows it.
    fn cmd_resp(&mut self, bytes: &[u8], len: usize) -> Result<Vec<u8>> {
        self.io.write(bytes)?;
        let mut buf = self.io.read(len + 1)?;
        if buf[0] != bytes[0] {
            return Err(Error::Protocol(format!(
                "echo mismatch: sent {:02X}, got {:02X}",
                bytes[0], buf[0]
            )));
        }
        Ok(buf.split_off(1))
    }

    /// Send a command that does not echo (bulk reads).
    fn cmd_raw(&mut self, bytes: &[u8], len: usize) -> Result<Vec<u8>> {
        self.io.write(bytes)?;
        self.io.read(len)
    }

    /// Hardware ping; also the keepalive that stops the device from dropping.
    pub fn ping(&mut self) -> Result<()> {
        self.cmd(&[0xF3])
    }

    // ----- configuration ---------------------------------------------------

    /// Full initialisation: handshake, calibration dumps, defaults.
    pub fn init(&mut self) -> Result<()> {
        // Handshake and capability dump, byte for byte what Scope.exe sends.
        self.cmd(&[0xB0])?;
        self.cmd(&[0xF3])?;
        self.cmd(&[0xB9, 0x01, 0xBF, 0x04, 0x00, 0x00])?;
        self.cmd(&[0xB7, 0x00])?;
        self.cmd(&[0xBB, 0x08, 0x00])?;
        self.cmd(&[0xB5])?;
        self.cmd(&[0xB6])?;

        // Calibration tables. Contents differ between firmware revisions, so we
        // drain them and keep going rather than asserting on the bytes.
        let _ = self.cmd_raw(&[0xE5], 2)?;
        let _ = self.cmd_raw(&[0xF7], 64)?;
        let _ = self.cmd_raw(&[0xF8], 64)?;
        let _ = self.cmd_raw(&[0xFA], 56)?;
        std::thread::sleep(Duration::from_millis(213));
        self.cmd(&[0xF5])?;

        self.set_channels(&self.enabled.clone())?;
        self.set_time_div(self.time_div)?;
        self.set_trigger(0, false)?;
        self.cmd(&[0xA7, 0x00])?;
        self.cmd(&[0xAC, 0x01, 0xF4, 0x00, 0x09, 0xC5, 0x00, 0x09, 0xC5])?;

        // Second stage: latch the state we just configured.
        self.cmd(&[0xF6])?;
        std::thread::sleep(Duration::from_millis(213));
        let _ = self.cmd_raw(&[0xE5], 2)?;
        let _ = self.cmd_raw(&[0xF7], 64)?;
        let _ = self.cmd_raw(&[0xF8], 64)?;
        let _ = self.cmd_raw(&[0xFA], 56)?;

        self.set_time_div(self.time_div)?;
        self.cmd(&[0xAC, 0x00, 0xC8, 0x00, 0x02, 0xBD, 0x00, 0x02, 0xBD])?;
        self.cmd(&[0xE4, 0x01])?;
        let _ = self.cmd_raw(&[0xE6, 0x01], 10)?;
        self.ping()?;

        let ranges = self.ranges;
        let channels = self.enabled.clone();
        self.set_channels(&channels)?;
        self.set_ranges(&ranges)?;
        self.set_time_div(self.time_div)?;
        self.set_trigger(0, false)?;
        self.cmd(&[0xA7, 0x00])?;
        self.cmd(&[0xAC, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x05, 0x79])?;
        self.set_trigger_level(2048)?;
        let e9 = self.cmd_raw(&[0xE9], 2)?;
        let _ = e9;
        Ok(())
    }

    /// Enable exactly these channels.
    pub fn set_channels(&mut self, channels: &[usize]) -> Result<()> {
        if channels.is_empty() {
            return Err(Error::Protocol("at least one channel must be enabled".into()));
        }
        let mut chans: Vec<usize> = channels.to_vec();
        chans.sort_unstable();
        chans.dedup();
        if chans.iter().any(|&c| c >= CHANNELS) {
            return Err(Error::Protocol("channel index out of range".into()));
        }
        self.cmd(&[0xA0, chans.len() as u8])?;
        let mut mask = [0u8; CHANNELS];
        for &c in &chans {
            mask[c] = 1;
        }
        let mut aa = vec![0xAAu8];
        aa.extend_from_slice(&mask);
        self.cmd(&aa)?;
        self.enabled = chans;
        Ok(())
    }

    /// Set the input range of every channel.
    pub fn set_ranges(&mut self, ranges: &[Range; CHANNELS]) -> Result<()> {
        let mut a2 = vec![0xA2u8];
        for r in ranges {
            a2.push(r.id());
        }
        self.cmd(&a2)?;
        // The front end needs time to settle after a range change.
        std::thread::sleep(Duration::from_millis(213));
        self.ranges = *ranges;
        Ok(())
    }

    /// Set the time base.
    pub fn set_time_div(&mut self, td: TimeDiv) -> Result<()> {
        self.cmd(&[0xA3, td.id])?;
        self.time_div = td;
        Ok(())
    }

    /// Trigger source and slope.
    pub fn set_trigger(&mut self, channel: usize, falling: bool) -> Result<()> {
        self.cmd(&[0xC1, channel as u8, falling as u8])
    }

    /// Trigger level as a raw ADC code.
    pub fn set_trigger_level(&mut self, level: u16) -> Result<()> {
        let level = level.min(ADC_MAX);
        let hi = (level >> 8) as u8;
        let lo = level as u8;
        self.cmd(&[0xAB, hi, lo])
    }

    // ----- acquisition -----------------------------------------------------

    /// Start one acquisition and wait until the device says data is ready.
    pub fn arm(&mut self) -> Result<()> {
        self.ping()?;
        self.cmd(&[0xE4, 0x01])?;
        let _ = self.cmd_raw(&[0xE6, 0x01], 10)?;
        self.cmd(&[0xA4, 0x01])?;
        std::thread::sleep(Duration::from_millis(15));
        self.cmd(&[0xC0])?;
        self.cmd(&[0xC2])?;
        self.wait_ready()
    }

    /// Poll `0xA5 0x5A` until the device reports a filled buffer.
    ///
    /// Some firmware answers with echo plus a status byte (2 = buffer 2 is
    /// ready, 3 = both), others echo only. Treat a bare echo as "not ready yet"
    /// and keep asking; after the last attempt let the caller try reading.
    fn wait_ready(&mut self) -> Result<()> {
        for attempt in 0..20 {
            self.io.write(&[0xA5, 0x5A])?;
            let r = self.io.drain(64, 60)?;
            if r.len() >= 2 && (r[1] == 2 || r[1] == 3) {
                return Ok(());
            }
            if r.len() == 1 && r[0] == 0xA5 && attempt < 19 {
                std::thread::sleep(Duration::from_millis(20));
                self.ping()?;
                continue;
            }
            if attempt == 19 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            self.ping()?;
        }
        Ok(())
    }

    /// Read one of the two acquisition buffers.
    fn read_buffer(&mut self, which: u8) -> Result<Vec<u8>> {
        let hdr = self.cmd_raw(&[0xC6, which], 2)?;
        let len = u16::from_be_bytes([hdr[0], hdr[1]]) as usize;
        if len == 0 || len > 64 * 1024 {
            return Err(Error::Protocol(format!("implausible buffer length {len}")));
        }
        let packets = len.div_ceil(64);
        let mut out = Vec::with_capacity(len);
        for _ in 0..packets {
            let p = self.cmd_raw(&[0xA6, which], 64)?;
            out.extend_from_slice(&p);
        }
        out.truncate(len);
        Ok(out)
    }

    /// One acquisition: arm, then drain both buffers.
    pub fn read_block(&mut self) -> Result<Block> {
        self.arm()?;
        let mut bytes = self.read_buffer(0x02)?;
        bytes.extend_from_slice(&self.read_buffer(0x03)?);
        let t_ns = self.t0.elapsed().as_nanos() as u64;
        let block = demux(&bytes, &self.enabled, self.seq, t_ns)?;
        self.seq += 1;
        Ok(block)
    }

    /// Measure the real sample rate and block cadence of the link.
    ///
    /// The device does not report its clock, and the effective rate depends on
    /// how many channels are enabled, so the only honest number is the measured
    /// one. This is what the UI shows and the log records.
    pub fn measure_rate(&mut self, samples: usize) -> Result<DeviceInfo> {
        let mut total = 0usize;
        let mut blocks = 0usize;
        let mut block_n = 0usize;
        let start = Instant::now();
        for _ in 0..samples {
            let b = self.read_block()?;
            total += b.n;
            block_n = b.n;
            blocks += 1;
        }
        let secs = start.elapsed().as_secs_f64().max(1e-9);
        let rate = if blocks > 1 {
            // First block pays for arming; measure the steady state.
            (total.saturating_sub(block_n)) as f64 / (secs * (blocks - 1) as f64 / blocks as f64)
        } else {
            total as f64 / secs
        };
        self.info.sample_rate = rate;
        self.info.block_samples = block_n;
        self.info.blocks_per_s = blocks as f64 / secs;
        Ok(self.info.clone())
    }

    /// Measure the zero offset of every channel on every range.
    ///
    /// Ground the inputs (or leave them unconnected) before calling this; the
    /// result is only as good as the silence of the front end.
    pub fn measure_zeros(&mut self, blocks: usize) -> Result<Calibration> {
        let saved_ranges = self.ranges;
        let saved_channels = self.enabled.clone();
        self.set_channels(&(0..CHANNELS).collect::<Vec<_>>())?;

        let mut calib = self.calib.clone();
        for range in Range::ALL {
            self.set_ranges(&[range; CHANNELS])?;
            let mut sums = [0f64; CHANNELS];
            let mut counts = [0usize; CHANNELS];
            for _ in 0..blocks.max(1) {
                let b = self.read_block()?;
                for ch in 0..CHANNELS {
                    if let Some(raw) = &b.ch[ch] {
                        for &v in raw {
                            sums[ch] += v as f64;
                            counts[ch] += 1;
                        }
                    }
                }
            }
            for ch in 0..CHANNELS {
                if counts[ch] > 0 {
                    calib.set_zero(range, ch, (sums[ch] / counts[ch] as f64) as f32);
                }
            }
        }
        calib.measured_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        calib.device = self.info.link.clone();

        self.set_channels(&saved_channels)?;
        self.set_ranges(&saved_ranges)?;
        self.calib = calib.clone();
        Ok(calib)
    }

    /// Describe the configured state for logging and the UI.
    pub fn state(&self) -> DeviceInfo {
        self.info.clone()
    }
}
