//! Byte transport to the instrument.
//!
//! There are two ways to reach the Hantek 1008C on Windows:
//!
//! * **WinUSB** (default) — the device is bound to the generic WinUSB driver and
//!   we drive the bulk pipe directly with `WinUsb_WritePipe`/`WinUsb_ReadPipe`.
//!   No vendor kernel code is involved, so nothing can bugcheck the machine.
//! * **Vendor driver** (`vendor-driver` feature, off by default) — Hantek's own
//!   `DSO6CDE` driver exposes the same pipe as a file. It is faster to set up
//!   (nothing to install) but it is a 2017 WDM driver that does not tolerate
//!   direct access: on this hardware it answers reads with `ERROR_GEN_FAILURE`
//!   and, on retries, bugchecks the machine with `0x3B`. Do not enable it unless
//!   you know what you are doing.
//!
//! Both paths speak the same 64-byte command/response framing.

use crate::Result;

/// A bidirectional 64-byte-packet link to the device.
pub trait Transport: Send {
    /// Send one command frame.
    fn write(&mut self, buf: &[u8]) -> Result<()>;
    /// Read exactly `len` bytes of response.
    fn read(&mut self, len: usize) -> Result<Vec<u8>>;
    /// Read whatever is available within `timeout_ms`, up to `max` bytes.
    ///
    /// Some commands (the calibration dumps) answer with a device-specific
    /// number of bytes, so the only safe way to consume them is to drain until
    /// the device goes quiet.
    fn drain(&mut self, _max: usize, _timeout_ms: u64) -> Result<Vec<u8>> {
        Ok(Vec::new())
    }
    /// Human readable identity of the link, for logs and the UI.
    fn describe(&self) -> String;
}

#[cfg(windows)]
pub mod winusb;

#[cfg(all(windows, feature = "vendor-driver"))]
pub mod vendor;

#[cfg(windows)]
pub use winusb::WinUsbTransport;

/// Open the first instrument reachable over WinUSB.
#[cfg(windows)]
pub fn open_default() -> Result<Box<dyn Transport>> {
    Ok(Box::new(winusb::open_first()?))
}

#[cfg(not(windows))]
pub fn open_default() -> Result<Box<dyn Transport>> {
    Err(crate::Error::NoDevice)
}
