//! # hantek1008
//!
//! Driver-level access to the Hantek 1008C eight-channel USB oscilloscope.
//!
//! The device is an STM32F103 + 74HCT4051 front end with a 12-bit ADC shared by
//! all eight channels. On Windows it is bound to Hantek's own kernel driver,
//! which exposes the USB bulk pipe as a plain file object. That is the same path
//! the vendor tool uses (`HTNTLVDll.dll` calls `CreateFileA` + `WriteFile` +
//! `ReadFile`), so we can talk to the hardware natively from a 64-bit process:
//! no Zadig, no driver replacement, no vendor DLL.
//!
//! ```no_run
//! use hantek1008::{Device, Range};
//!
//! # fn main() -> hantek1008::Result<()> {
//! let mut dev = Device::open()?;
//! dev.init()?;
//! dev.set_ranges(&[Range::V20; 8])?;
//! let block = dev.read_block()?;
//! println!("{} samples on ch1", block.n);
//! # Ok(())
//! # }
//! ```
//!
//! The API is synchronous and allocation-light: one acquisition thread owns a
//! [`Device`], everything else consumes [`Block`]s.

pub mod calib;
pub mod device;
pub mod protocol;
pub mod sim;
pub mod transport;

pub use calib::Calibration;
pub use device::{Device, DeviceInfo};
pub use protocol::{Block, Range, TimeDiv, CHANNELS};

use std::fmt;

/// Everything that can go wrong while talking to the instrument.
#[derive(Debug)]
pub enum Error {
    /// The USB device (VID:PID 0783:5725) is not present or not bound to the
    /// Hantek driver.
    NoDevice,
    /// A Win32 call failed.
    Win32 { call: &'static str, code: u32 },
    /// The device answered something we did not expect.
    Protocol(String),
    /// The device did not answer within the timeout.
    Timeout(&'static str),
    /// Generic I/O error.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoDevice => write!(
                f,
                "Hantek 1008C not found (USB 0783:5725). Is it plugged in and bound to the Hantek driver?"
            ),
            Error::Win32 { call, code } => {
                write!(f, "{call} failed with Win32 error {code}")
            }
            Error::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Error::Timeout(what) => write!(f, "timeout while {what}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
