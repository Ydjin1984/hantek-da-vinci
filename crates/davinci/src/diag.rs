//! Small always-on diagnostic log for field problems.
//!
//! The window has `window.log` for start-up timing, but the acquisition had no
//! place to say "the link died at 03:12 and came back at 03:14". That is the one
//! fact needed to diagnose an overnight freeze after the fact, so it goes to a
//! single file next to the settings, rotated once at 1 MB.
//!
//! It is written only on state changes (open, lost, reopened, reload), never per
//! block and never per poll, so a healthy system writes almost nothing.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1_000_000;

pub fn log(message: &str) {
    let path = crate::app::state_dir().join("davinci.log");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{stamp} {message}");
    }
}
