//! Acquisition engine: one thread owns the device, everything else reads state.
//!
//! The engine keeps a short rolling history for the live view, per-channel
//! statistics, a peak-hold that never resets on its own, and an optional
//! continuous log on disk. The log is the only place where full-rate data lives;
//! the UI is always served a decimated envelope, which is what makes eight
//! channels at once cheap to render and impossible to "lose" a peak.

use crate::capture::Snapshot;
use crate::trigger::{self, Config as TriggerConfig};
use hantek1008::protocol::Measure;
use hantek1008::{Block, Calibration, Device, Range, TimeDiv, CHANNELS};
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How much history the live view can look back at.
const HISTORY_SECONDS: f64 = 5.0;

/// How long a level may sit outside the signal before the engine moves it.
///
/// One block is 1–60 ms, so this is several blocks of evidence: long enough that
/// noise or a single off-screen excursion does not move the level, short enough
/// that the picture is never visibly stuck.
const LEVEL_RESCUE_MS: u64 = 400;

/// After the user sets the level, the rescue keeps its hands off for this long.
const LEVEL_USER_GRACE_MS: u64 = 2500;

/// Consecutive read/apply failures before the USB handle is thrown away.
///
/// A handle that has seen the device re-enumerate (sleep, hub power management,
/// cable) never works again: every read fails with the same error for ever.
/// Reopening is the only cure, and the program has to do it itself — the user
/// should not have to restart the window.
const FAILURES_BEFORE_REOPEN: u32 = 3;

/// First and longest pause between attempts to reopen a missing instrument.
const RECONNECT_MIN_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 15_000;

/// How long one reopen attempt waits for the device to enumerate.
const RECONNECT_OPEN_MS: u64 = 1_200;

/// No block for this long means the acquisition is not delivering data, even if
/// the last block rate is still in the structure. The UI must show zero, not a
/// frozen lie.
const DATA_STALE_MS: u64 = 1_500;

/// One DAV file stops growing here. A night at three channels is ~70 KB/s, so
/// this is many hours; the point is that a forgotten recording cannot fill the
/// system disk. Stopping is reported in the status instead of happening silently.
const LOG_MAX_BYTES: u64 = 1 << 30;

/// Whether the USB link is delivering, being reopened, or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Live,
    Reconnecting,
    Offline,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkState::Live => "live",
            LinkState::Reconnecting => "reconnecting",
            LinkState::Offline => "offline",
        }
    }
}

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
    /// Vertical centre of each lane, in volts: the view is shifted by this much.
    /// Purely a display offset — the instrument always measures from 0 V.
    pub center_v: [f32; CHANNELS],
    /// Trigger settings. The engine applies them; the device knows nothing of
    /// them, because the edge is found on the calibrated samples.
    pub trigger: TriggerConfig,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            enabled: (0..CHANNELS).collect(),
            ranges: [Range::V20; CHANNELS],
            range_v: [20.0; CHANNELS],
            time_div: TimeDiv::DEFAULT,
            autoscale: [false; CHANNELS],
            center_v: [0.0; CHANNELS],
            trigger: TriggerConfig::default(),
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
    /// Capture state: `free`, `waiting`, `captured`, `auto_free`, `stopped`.
    pub state: String,
    /// Identifier of the frame the capture endpoint would return right now.
    pub frame_id: u64,
    /// True while the screen is frozen.
    pub held: bool,
    /// True while the DAV log is recording.
    pub rec: bool,
    /// Trigger settings in force.
    pub trigger: TriggerConfig,
    /// Smallest and largest value of the trigger source in the most recent block,
    /// in volts. The UI prints them next to the level so a level that the signal
    /// cannot reach is visible before it costs a minute of a frozen screen.
    pub source_span: Option<(f32, f32)>,
    /// Set for a while after the engine moved a level no edge could ever reach.
    pub level_rescue: Option<LevelRescueInfo>,
    /// `live`, `reconnecting` or `offline`: whether the USB link is delivering,
    /// being reopened, or absent.
    pub link_state: &'static str,
    /// Age of the frame the screen is showing, held frame included. This is what
    /// tells "Single holds a capture" apart from "the picture died".
    pub frame_age_ms: Option<u64>,
    /// Age of the newest block. Grows while the device is silent; the UI turns
    /// this into "нет данных N с".
    pub last_block_age_ms: Option<u64>,
    /// Failures since the last good block.
    pub consecutive_errors: u32,
    /// Reopen attempts in the current outage.
    pub reopen_attempts: u32,
    /// Why recording stopped, when it stopped by itself (size limit or a write
    /// error). `None` while recording is healthy or was never started.
    pub log_error: Option<String>,
}

/// What the engine did, and what it saw, when it had to move an unreachable level.
#[derive(Debug, Clone, Copy)]
pub struct LevelRescueInfo {
    pub min_v: f32,
    pub max_v: f32,
    pub level_v: f32,
    /// Milliseconds since the move, so an old note fades out of the panel.
    pub age_ms: u64,
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
    blocks_total: u64,
    frame_counter: u64,
    /// Bumped whenever something the acquisition depends on changes. A frame
    /// carries the generation it was captured under, so a result computed with
    /// old settings can never be presented as if it were the new ones.
    generation: u64,
    /// Last published frame, and the recent few kept by id for re-export.
    last: Option<Arc<Snapshot>>,
    recent: VecDeque<Arc<Snapshot>>,
    /// Frame pinned by "Удержать экран".
    held: Option<Arc<Snapshot>>,
    publish_seq: u64,
    state: CaptureState,
    /// When the current wait for an edge began (Auto's timeout runs from here).
    waiting_since: Option<Instant>,
    /// Single-shot: armed by the user, cleared by the capture it produces.
    armed: bool,
    single_done: bool,
    /// Bumped on every arm, so a block read started before the arm cannot
    /// satisfy it.
    arm_seq: u64,
    /// When the level was first seen outside the source's own range.
    level_outside_since: Option<Instant>,
    /// When the user last typed or dragged the level, which grants it a grace
    /// period the rescue must not shorten.
    level_set_at: Option<Instant>,
    /// The last time the engine had to move an unreachable level.
    level_rescue: Option<LevelRescue>,
    /// Extremes of the trigger source in the latest block, in volts.
    source_span: Option<(f32, f32)>,
    /// Link health, reported to the UI so "no data" is never mistaken for
    /// "waiting for a trigger".
    link_state: LinkState,
    /// When the last block was ingested. `None` until the first one.
    last_block_at: Option<Instant>,
    /// Consecutive failures since the last good block.
    consecutive_errors: u32,
    /// Reopen attempts in the current outage.
    reopen_attempts: u32,
    /// Why recording stopped by itself, if it did.
    log_error: Option<String>,
}

/// A trace the engine wrote when it moved a level the signal could not reach.
#[derive(Debug, Clone, Copy)]
struct LevelRescue {
    min_v: f32,
    max_v: f32,
    level_v: f32,
    at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureState {
    Free,
    Waiting,
    Captured,
    AutoFree,
    Stopped,
}

impl CaptureState {
    fn as_str(self) -> &'static str {
        match self {
            CaptureState::Free => "free",
            CaptureState::Waiting => "waiting",
            CaptureState::Captured => "captured",
            CaptureState::AutoFree => "auto_free",
            CaptureState::Stopped => "stopped",
        }
    }
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
    reset_peaks: Arc<AtomicBool>,
    /// Set by the UI's "Переподключить" button; the supervisor drops the handle
    /// and opens the instrument again.
    reconnect: Arc<AtomicBool>,
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
                    center_v: [0.0; CHANNELS],
                    trigger: TriggerConfig::default(),
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
                blocks_total: 0,
                frame_counter: 0,
                generation: 0,
                last: None,
                recent: VecDeque::new(),
                held: None,
                publish_seq: 0,
                state: CaptureState::Free,
                waiting_since: None,
                armed: false,
                single_done: false,
                arm_seq: 0,
                level_outside_since: None,
                level_set_at: None,
                level_rescue: None,
                source_span: None,
                link_state: LinkState::Live,
                last_block_at: None,
                consecutive_errors: 0,
                reopen_attempts: 0,
                log_error: None,
            })),
            running: Arc::new(AtomicBool::new(true)),
            reset_peaks: Arc::new(AtomicBool::new(false)),
            reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An engine for a program that has no instrument (yet).
    ///
    /// It never produces samples until the supervisor manages to open the
    /// instrument, but it *does* keep trying: plugging the cable back in or
    /// waking the machine must not require restarting the window. `run` is
    /// spawned with `None` for exactly this case.
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
                blocks_total: 0,
                frame_counter: 0,
                generation: 0,
                last: None,
                recent: VecDeque::new(),
                held: None,
                publish_seq: 0,
                state: CaptureState::Stopped,
                waiting_since: None,
                armed: false,
                single_done: false,
                arm_seq: 0,
                level_outside_since: None,
                level_set_at: None,
                level_rescue: None,
                source_span: None,
                link_state: LinkState::Offline,
                last_block_at: None,
                consecutive_errors: 0,
                reopen_attempts: 0,
                log_error: None,
            })),
            running: Arc::new(AtomicBool::new(true)),
            reset_peaks: Arc::new(AtomicBool::new(false)),
            reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn handle(&self) -> EngineHandle {
        EngineHandle {
            inner: Arc::clone(&self.inner),
            running: Arc::clone(&self.running),
            reset_peaks: Arc::clone(&self.reset_peaks),
            reconnect: Arc::clone(&self.reconnect),
        }
    }

    /// Run the acquisition loop forever on the current thread.
    ///
    /// `dev` is `None` when the program started without an instrument: the
    /// supervisor then keeps trying to open one instead of staying dead until a
    /// restart. The same loop owns recovery from a USB handle that stopped
    /// answering (sleep, re-enumeration, hub power management).
    pub fn run(self, mut dev: Option<Device>) -> ! {
        let mut applied: Option<Applied> = None;
        let mut last_rate_check = Instant::now();
        let mut blocks_at_check = 0u64;
        let mut failures = 0u32;
        let mut backoff_ms = RECONNECT_MIN_MS;

        loop {
            // An explicit request from the UI: drop the handle and open again.
            if self.reconnect.swap(false, Ordering::Relaxed) {
                dev = None;
                applied = None;
                failures = 0;
                let mut g = self.inner.lock().unwrap();
                g.link_state = LinkState::Reconnecting;
                g.error = Some("переподключение по запросу".into());
                g.waiting_since = None;
                crate::diag::log("link: переподключение по запросу");
            }

            // No instrument: a state, not the end of the program. Try again with
            // a growing pause, so a missing cable or a re-enumerating device is
            // picked up without a restart.
            if dev.is_none() {
                {
                    let mut g = self.inner.lock().unwrap();
                    g.link_state = LinkState::Reconnecting;
                    if g.error.is_none() {
                        g.error = Some("прибор не найден — ищу…".into());
                    }
                }
                match open_device(RECONNECT_OPEN_MS) {
                    Ok(mut d) => {
                        applied = None;
                        failures = 0;
                        backoff_ms = RECONNECT_MIN_MS;
                        let calib = self.inner.lock().unwrap().calib.clone();
                        d.calib = calib;
                        let link = d.info.link.clone();
                        dev = Some(d);
                        let mut g = self.inner.lock().unwrap();
                        g.link = link;
                        g.link_state = LinkState::Live;
                        g.error = None;
                        g.consecutive_errors = 0;
                        g.reopen_attempts = 0;
                        g.generation += 1;
                        g.waiting_since = None;
                        last_rate_check = Instant::now();
                        blocks_at_check = g.blocks_total;
                        drop(g);
                        crate::diag::log("link: прибор открыт");
                    }
                    Err(e) => {
                        let mut g = self.inner.lock().unwrap();
                        g.link_state = LinkState::Offline;
                        g.error = Some(format!("прибор не открывается: {e}"));
                        g.consecutive_errors = g.consecutive_errors.saturating_add(1);
                        g.reopen_attempts = g.reopen_attempts.saturating_add(1);
                        drop(g);
                        crate::diag::log(&format!("link: открыть не удалось: {e}"));
                        std::thread::sleep(Duration::from_millis(backoff_ms));
                        backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
                        continue;
                    }
                }
            }

            if !self.running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
                // Idle: keep the device alive without acquiring. A dead handle is
                // still a dead handle, so the same failure counter decides when
                // to reopen it.
                let ping = dev.as_mut().expect("device checked above").ping();
                match ping {
                    Ok(()) => self.link_ok(),
                    Err(e) => {
                        self.link_error(&e.to_string(), &mut failures);
                        if failures >= FAILURES_BEFORE_REOPEN {
                            dev = None;
                            applied = None;
                            failures = 0;
                        }
                    }
                }
                continue;
            }

            // Push user changes down to the hardware before acquiring again.
            let want = { self.inner.lock().unwrap().params.clone() };
            let touched = match apply_params(dev.as_mut().expect("device checked above"), &want, &mut applied) {
                Ok(changed) => changed,
                Err(e) => {
                    self.link_error(&e, &mut failures);
                    if failures >= FAILURES_BEFORE_REOPEN {
                        crate::diag::log(&format!("link: настройка не прошла ({e}) — переоткрываю"));
                        dev = None;
                        applied = None;
                        failures = 0;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };

            // The time axis is defined by the time base and the number of
            // channels, not by how fast USB happens to deliver blocks: the device
            // always returns one screen of 4000 samples, split between the
            // enabled channels. Deriving the rate from throughput instead made
            // every frequency measurement drift with USB load.
            let rate = implied_rate(&want);
            {
                let mut g = self.inner.lock().unwrap();
                g.info_rate = rate;
                // A block acquired from here on belongs to this configuration.
                // Bumping the generation only when the device accepted the change
                // keeps a failed `set_ranges` from labelling old data as new.
                if touched {
                    g.generation += 1;
                    g.waiting_since = None;
                }
            }

            // Pin the settings this read will be interpreted with, before the
            // read: the user may change the range while USB is in flight, and the
            // block that comes back belongs to the settings it was asked for.
            let (pinned, generation, arm_seq) = {
                let g = self.inner.lock().unwrap();
                (g.params.clone(), g.generation, g.arm_seq)
            };

            let read = dev.as_mut().expect("device checked above").read_block();
            match read {
                Ok(block) => {
                    let n = block.n;
                    failures = 0;
                    backoff_ms = RECONNECT_MIN_MS;
                    let elapsed = {
                        let mut g = self.inner.lock().unwrap();
                        ingest(&mut g, block, &self.reset_peaks, &pinned, generation, rate, arm_seq);
                        g.blocks_total += 1;
                        g.last_block_at = Some(Instant::now());
                        // A successful block is the only proof the link is alive.
                        // Clearing this here is what stops one transient timeout
                        // from keeping "нет связи" on screen for the session.
                        g.error = None;
                        g.consecutive_errors = 0;
                        g.link_state = LinkState::Live;
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
                    self.link_error(&e.to_string(), &mut failures);
                    if failures >= FAILURES_BEFORE_REOPEN {
                        crate::diag::log(&format!(
                            "link: {FAILURES_BEFORE_REOPEN} ошибки подряд ({e}) — переоткрываю прибор"
                        ));
                        dev = None;
                        applied = None;
                        failures = 0;
                        let mut g = self.inner.lock().unwrap();
                        g.link_state = LinkState::Reconnecting;
                        g.waiting_since = None;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    /// Record one failed read/apply without holding the lock across sleeps.
    fn link_error(&self, message: &str, failures: &mut u32) {
        *failures = failures.saturating_add(1);
        let mut g = self.inner.lock().unwrap();
        let first = g.consecutive_errors == 0;
        g.error = Some(message.to_string());
        g.consecutive_errors = g.consecutive_errors.saturating_add(1);
        if *failures >= FAILURES_BEFORE_REOPEN {
            g.link_state = LinkState::Reconnecting;
        }
        drop(g);
        if first {
            crate::diag::log(&format!("link: ошибка чтения: {message}"));
        }
    }

    /// A successful read or ping: the link is healthy, forget the failure.
    fn link_ok(&self) {
        let mut g = self.inner.lock().unwrap();
        if g.error.is_some() || g.link_state != LinkState::Live {
            g.error = None;
            g.consecutive_errors = 0;
            g.link_state = LinkState::Live;
            drop(g);
            crate::diag::log("link: связь восстановлена");
        }
    }
}

/// Parameters actually accepted by the device.
///
/// Kept separate from [`Params`] so a failed command is never recorded as
/// applied: the next loop retries it instead of believing a range change that
/// the instrument rejected.
struct Applied {
    enabled: Vec<usize>,
    ranges: [Range; CHANNELS],
    time_div: TimeDiv,
}

/// Send the differences to the device; return whether anything changed.
fn apply_params(
    dev: &mut Device,
    want: &Params,
    applied: &mut Option<Applied>,
) -> Result<bool, String> {
    let fresh = applied.is_none();
    let mut touched = false;
    if fresh || applied.as_ref().map(|a| a.enabled != want.enabled).unwrap_or(true) {
        dev.set_channels(&want.enabled).map_err(|e| e.to_string())?;
        touched = true;
    }
    if fresh || applied.as_ref().map(|a| a.ranges != want.ranges).unwrap_or(true) {
        dev.set_ranges(&want.ranges).map_err(|e| e.to_string())?;
        touched = true;
    }
    if fresh || applied.as_ref().map(|a| a.time_div.id != want.time_div.id).unwrap_or(true) {
        dev.set_time_div(want.time_div).map_err(|e| e.to_string())?;
        touched = true;
    }
    *applied = Some(Applied {
        enabled: want.enabled.clone(),
        ranges: want.ranges,
        time_div: want.time_div,
    });
    Ok(touched)
}

/// Open and initialise an instrument for the supervisor.
fn open_device(timeout_ms: u64) -> Result<Device, String> {
    let mut dev = Device::open_within(Duration::from_millis(timeout_ms))
        .map_err(|e| e.to_string())?;
    dev.init().map_err(|e| e.to_string())?;
    Ok(dev)
}

/// Cheap clonable control surface used by the HTTP layer.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Mutex<Inner>>,
    running: Arc<AtomicBool>,
    reset_peaks: Arc<AtomicBool>,
    reconnect: Arc<AtomicBool>,
}

impl EngineHandle {
    pub fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        if on {
            // Resuming starts a fresh wait; the previous frame stays until one
            // arrives, but it is no longer "the captured one".
            g.waiting_since = None;
            g.state = match g.params.trigger.mode {
                trigger::Mode::Single => CaptureState::Waiting,
                _ => CaptureState::Free,
            };
        } else {
            g.state = CaptureState::Stopped;
            // Stop cancels a pending arm: the request was for the run we just
            // ended, and silently capturing after it would be a surprise.
            g.armed = false;
            g.arm_seq += 1;
            g.waiting_since = None;
        }
    }

    pub fn reset_peaks(&self) {
        self.reset_peaks.store(true, Ordering::Relaxed);
    }

    /// Ask the supervisor to drop the USB handle and open the instrument again.
    ///
    /// For a handle that stopped answering, and for a user who suspects the
    /// link: the program no longer needs to be restarted to get the device back.
    pub fn reconnect(&self) {
        self.reconnect.store(true, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap();
        g.link_state = LinkState::Reconnecting;
        g.error = Some("переподключение по запросу".into());
        g.waiting_since = None;
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
        // Peak-hold from a wider range would claim volts this scale can never
        // show, so switching scale starts the peaks over.
        self.reset_peaks.store(true, Ordering::Relaxed);
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

    /// Replace the trigger settings, rejecting nonsense without touching state.
    ///
    /// The caller gets the reason, not a silent clamp: a UI that shows "30 %"
    /// while the engine runs 10 % is worse than an error message.
    pub fn set_trigger(&self, cfg: TriggerConfig) -> Result<(), String> {
        cfg.validate()?;
        let mut g = self.inner.lock().unwrap();
        if !g.params.enabled.contains(&cfg.source) {
            return Err(format!(
                "источник триггера CH{} выключен",
                cfg.source + 1
            ));
        }
        let mode_changed = g.params.trigger.mode != cfg.mode;
        // A level the user typed is theirs for a moment: the rescue below must not
        // move it out from under the keyboard before they finish.
        if (g.params.trigger.level_v - cfg.level_v).abs() > f32::EPSILON
            || (g.params.trigger.source != cfg.source)
            || (g.params.trigger.edge != cfg.edge)
        {
            g.level_set_at = Some(Instant::now());
            g.level_outside_since = None;
            g.level_rescue = None;
        }
        g.params.trigger = cfg;
        if mode_changed {
            g.armed = false;
            g.single_done = false;
            g.arm_seq += 1;
            g.waiting_since = None;
        }
        g.persist();
        Ok(())
    }

    /// Arm the single-shot trigger.
    pub fn arm(&self) {
        let mut g = self.inner.lock().unwrap();
        g.params.trigger.mode = trigger::Mode::Single;
        g.armed = true;
        g.single_done = false;
        g.arm_seq += 1;
        g.waiting_since = None;
        g.state = CaptureState::Waiting;
        self.running.store(true, Ordering::Relaxed);
        g.persist();
    }

    /// Freeze or release the displayed frame.
    ///
    /// Acquisition and the DAV log keep running either way: holding is about what
    /// the *screen* shows, not about stopping the instrument.
    pub fn set_hold(&self, on: bool) {
        let mut g = self.inner.lock().unwrap();
        if on {
            g.held = g.last.clone();
        } else {
            g.held = None;
        }
    }

    /// Shift a lane's vertical centre. Display only: the samples are untouched.
    pub fn set_center(&self, ch: usize, volts: f32) -> Result<(), String> {
        if ch >= CHANNELS {
            return Err(format!("канал {} вне диапазона 1..8", ch + 1));
        }
        if !volts.is_finite() || volts.abs() > 100.0 {
            return Err("центр должен быть числом в пределах ±100 В".into());
        }
        let mut g = self.inner.lock().unwrap();
        g.params.center_v[ch] = volts;
        g.persist();
        Ok(())
    }

    /// The frame a client should draw, and whether it is frozen.
    ///
    /// With `frame_id` the request is for one specific capture: that is how CSV
    /// and PNG stay pinned to the frame on screen even after the next sweep has
    /// arrived. Without it, the held frame wins, then the newest one.
    pub fn capture(&self, frame_id: Option<u64>) -> Option<Capture> {
        let g = self.inner.lock().unwrap();
        let held = g.held.is_some();
        let snap = match frame_id {
            Some(id) => g
                .held
                .as_ref()
                .filter(|s| s.id == id)
                .or_else(|| g.recent.iter().rev().find(|s| s.id == id))
                .cloned(),
            None => g.held.clone().or_else(|| g.last.clone()),
        }?;
        Some(Capture {
            snapshot: snap,
            held: held && frame_id.is_none(),
            state: g.state.as_str(),
            running: self.running.load(Ordering::Relaxed),
        })
    }

    /// CSV of one frame: the pinned one, or whatever is on screen.
    pub fn frame_csv(&self, frame_id: Option<u64>) -> Option<String> {
        self.capture(frame_id).map(|c| c.snapshot.csv())
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
            if let Some(c) = s.center_v.get(i) {
                if c.is_finite() && c.abs() <= 100.0 {
                    g.params.center_v[i] = *c;
                }
            }
        }
        // A restored trigger must never come back armed: a program that starts
        // already waiting for a single edge looks frozen and takes one shot
        // nobody asked for.
        g.params.trigger = s.trigger.clone();
        g.params.trigger.mode = match s.trigger.mode {
            trigger::Mode::Single => trigger::Mode::Normal,
            m => m,
        };
        if !g.params.enabled.contains(&g.params.trigger.source) {
            g.params.trigger.source = g.params.enabled.first().copied().unwrap_or(0);
        }
        g.armed = false;
        g.single_done = false;
    }

    pub fn status(&self) -> Status {
        let g = self.inner.lock().unwrap();
        status_from(&g, self.running.load(Ordering::Relaxed))
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
                // Amplitude wants every block; the period must not. Blocks are
                // separated by a USB round trip of unknown length, so a stitched
                // array has no single time base — the edges land on the joins and
                // the reading collapses (a 500 Hz square came out as 6 Hz, and its
                // duty as a meaningless 51%). One acquisition is one continuous
                // window, so measure each block alone and keep the middle value.
                let mut freqs: Vec<f32> = Vec::new();
                let mut dutys: Vec<f32> = Vec::new();
                for b in &blocks {
                    if let Some(raw) = &b.ch[ch] {
                        let block: Vec<f32> =
                            raw.iter().map(|&v| (v as f32 - zero) * vpc).collect();
                        let bm = Measure::of(&block, dt);
                        if let Some(f) = bm.freq {
                            freqs.push(f);
                        }
                        if let Some(d) = bm.duty {
                            dutys.push(d);
                        }
                        volts.extend_from_slice(&block);
                    }
                }
                let mut m = if volts.is_empty() {
                    None
                } else {
                    Some(Measure::of(&volts, dt))
                };
                if let Some(meas) = m.as_mut() {
                    if let Some(f) = median(&mut freqs) {
                        meas.freq = Some(f);
                    }
                    if let Some(d) = median(&mut dutys) {
                        meas.duty = Some(d);
                    }
                }
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
            // Columns with no sample keep `lo > hi`, the "nothing here" marker.
            // They used to be filled with 0 V, which paints a reading the
            // instrument never took: a sweep with fewer samples than pixels then
            // turns every gap into a spike towards zero.
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
            bytes: (8 + 4 + head.len()) as u64,
            since_fsync: 0,
            started_at: Instant::now(),
        });
        g.log_error = None;
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

/// Public status derived from the engine's state.
///
/// Kept out of [`EngineHandle`] so the staleness rules are testable without a
/// device thread: a rate that is no longer being fed must read as zero, and the
/// age of the shown frame must always be available.
fn status_from(g: &Inner, running: bool) -> Status {
    let stale = g
        .last_block_at
        .map_or(true, |t| t.elapsed() >= Duration::from_millis(DATA_STALE_MS));
    let shown = g.held.as_ref().or(g.last.as_ref());
    let age_ms = |d: Duration| d.as_millis().min(u64::MAX as u128) as u64;
    Status {
        running,
        link: g.link.clone(),
        sample_rate: g.info_rate,
        block_samples: if stale { 0 } else { g.block_samples },
        blocks_per_s: if stale { 0.0 } else { g.blocks_per_s },
        time_div: g.params.time_div.label(),
        time_div_id: g.params.time_div.id,
        window_s: g.params.time_div.real_window_s(g.params.enabled.len().max(1)),
        window_clamped: {
            let n = g.params.enabled.len().max(1);
            g.params.time_div.real_window_s(n) > g.params.time_div.window_s() * 1.02
        },
        error: g.error.clone(),
        state: g.state.as_str().to_string(),
        frame_id: shown.map(|s| s.id).unwrap_or(0),
        held: g.held.is_some(),
        rec: g.log.is_some(),
        trigger: g.params.trigger.clone(),
        source_span: g.source_span,
        level_rescue: g
            .level_rescue
            .map(|r| LevelRescueInfo {
                min_v: r.min_v,
                max_v: r.max_v,
                level_v: r.level_v,
                age_ms: r.at.elapsed().as_millis().min(u64::MAX as u128) as u64,
            })
            // An old move is history, not a message: the panel must not keep
            // explaining something that happened a minute ago.
            .filter(|r| r.age_ms < 15_000),
        log_path: g.log.as_ref().map(|l| l.path.display().to_string()),
        log_bytes: g.log.as_ref().map(|l| l.bytes).unwrap_or(0),
        log_seconds: g.log.as_ref().map(|l| l.started_at.elapsed().as_secs_f64()).unwrap_or(0.0),
        blocks_total: g.blocks_total,
        link_state: g.link_state.as_str(),
        frame_age_ms: shown.map(|s| age_ms(s.created_at.elapsed())),
        last_block_age_ms: g.last_block_at.map(|t| age_ms(t.elapsed())),
        consecutive_errors: g.consecutive_errors,
        reopen_attempts: g.reopen_attempts,
        log_error: g.log_error.clone(),
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
            center_v: self.params.center_v.to_vec(),
            trigger: self.params.trigger.clone(),
        }
        .save();
    }
}

/// Answer to "what should be on screen right now".
pub struct Capture {
    pub snapshot: Arc<Snapshot>,
    pub held: bool,
    pub state: &'static str,
    pub running: bool,
}

/// One decimated view of one acquisition.
pub struct Frame {    pub width: usize,
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
    ///
    /// A column with no samples is written as `(32767, -32768)`, i.e. `lo > hi`;
    /// the renderer skips it instead of drawing a fabricated 0 V.
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

/// Smallest and largest finite value of a block, ignoring NaN.
///
/// Both ends at 0 when nothing finite was seen, which makes "the input is not
/// moving" the same answer for an empty block and for a flat one.
fn extremes(v: &[f32]) -> (f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &x in v {
        if !x.is_finite() {
            continue;
        }
        if x < lo {
            lo = x;
        }
        if x > hi {
            hi = x;
        }
    }
    if lo > hi {
        (0.0, 0.0)
    } else {
        (lo, hi)
    }
}

/// Middle value of a set, or `None` when it is empty.
///
/// Used to combine a measurement taken from each acquisition: one bad block
/// (a truncated read, a phase that lands on an edge) must not move the reading.
fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

/// Absorb one block: peaks, log, history, and the trigger decision.
///
/// `pinned` is the settings the block was *requested* with, `generation` the
/// configuration they belong to and `rate` the sample rate they imply. Nothing
/// here reads `g.params`: a range changed while USB was in flight must not be
/// applied to data taken before it, which is exactly the bug the spec calls
/// "публикация запоздавшего кадра с новыми метаданными".
fn ingest(
    g: &mut Inner,
    block: Block,
    reset: &AtomicBool,
    pinned: &Params,
    generation: u64,
    rate: f64,
    arm_seq: u64,
) {
    let do_reset = reset.swap(false, Ordering::Relaxed);
    let dt = (1.0 / rate) as f32;

    for ch in 0..CHANNELS {
        let Some(raw) = &block.ch[ch] else { continue };
        let range = pinned.ranges[ch];
        let zero = g.calib.zero(range, ch);
        let vpc = range.volt_per_count();
        if do_reset {
            g.peaks[ch] = Peak { min: f32::INFINITY, max: f32::NEG_INFINITY };
        }
        let mut peak = g.peaks[ch];
        // Peaks go through the same dropout repair as the trace. Otherwise one
        // railed converter sample — the kind that is thrown out of every frame —
        // stays in "пики за сеанс" until the user presses the reset button, and
        // the panel claims a −20 V event the trace never showed.
        let (counts, _) = crate::capture::repair_dropouts(raw);
        for v in counts {
            peak.update((v - zero) * vpc);
        }
        g.peaks[ch] = peak;
    }

    // Log before trimming history.
    if g.log.is_some() {
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
        let mut stop: Option<String> = None;
        if let Some(log) = g.log.as_mut() {
            if log.bytes + 4 + len as u64 > LOG_MAX_BYTES {
                stop = Some(format!(
                    "запись остановлена: файл достиг {} МБ",
                    LOG_MAX_BYTES / (1024 * 1024)
                ));
            } else {
                let written = log
                    .writer
                    .write_all(&len.to_le_bytes())
                    .and_then(|()| log.writer.write_all(&payload));
                match written {
                    Ok(()) => {
                        log.bytes += 4 + len as u64;
                        log.since_fsync += 1;
                        if log.since_fsync >= 200 {
                            log.since_fsync = 0;
                            let _ = log.writer.flush();
                            let _ = log.writer.get_ref().sync_data();
                        }
                    }
                    Err(e) => stop = Some(format!("ошибка записи журнала: {e}")),
                }
            }
        }
        if let Some(reason) = stop {
            if let Some(mut l) = g.log.take() {
                let _ = l.writer.flush();
            }
            crate::diag::log(&reason);
            g.log_error = Some(reason);
        }
    }

    g.frame_counter += 1;
    g.history.push_back(Arc::new(block));
    let cap = (HISTORY_SECONDS * rate) as usize;
    while g.history.len() > 8 {
        let total: usize = g.history.iter().map(|b| b.n).sum();
        if total <= cap {
            break;
        }
        g.history.pop_front();
    }

    let block = g.history.back().expect("just pushed").clone();
    let mut cfg = pinned.trigger.clone();
    let mode = cfg.mode;

    // Single-shot only captures for the arm that is current *now*: a block whose
    // read started before the button was pressed was not asked for and must not
    // satisfy the request.
    if mode == trigger::Mode::Single {
        if g.armed && arm_seq == g.arm_seq {
            // Fall through: this block is the one the arm is waiting for.
        } else {
            g.state = if g.single_done {
                CaptureState::Captured
            } else {
                CaptureState::Waiting
            };
            return;
        }
    }

    let source_volts = block.ch[cfg.source].as_ref().map(|raw| {
        let range = pinned.ranges[cfg.source];
        let zero = g.calib.zero(range, cfg.source);
        let vpc = range.volt_per_count();
        raw.iter().map(|&v| (v as f32 - zero) * vpc).collect::<Vec<f32>>()
    });

    let hysteresis = cfg.hysteresis(pinned.ranges[cfg.source].resolution());

    // A level the signal cannot reach produces no edge, and Normal would then sit
    // on a frozen picture for ever — which reads as a hung program, not as an
    // instrument that is waiting. After a moment of watching, move the level to the
    // middle of what the input actually shows, and say so. A level inside the
    // signal is never touched: only a level that no edge could ever cross is moved,
    // and a level the user has just typed is left alone for a couple of seconds so
    // the rescue cannot fight the keyboard.
    if let Some(s) = source_volts.as_deref() {
        let (lo, hi) = extremes(s);
        g.source_span = Some((lo, hi));
        let moving = hi - lo > hysteresis;
        if mode != trigger::Mode::Free && moving {
            // A rising edge needs a sample below `low` before one above `high`;
            // when the whole block sits on one side of that band, no edge can
            // ever appear. Comparing against the band, not the bare level,
            // matters: a level 10 mV under a bus that never reaches 0 V is
            // "outside the signal" yet well inside one hysteresis.
            let low = cfg.level_v - hysteresis * 0.5;
            let high = cfg.level_v + hysteresis * 0.5;
            let unreachable = hi < high || lo > low;
            if unreachable {
                let since = *g.level_outside_since.get_or_insert_with(Instant::now);
                let user_recent = g
                    .level_set_at
                    .is_some_and(|t| t.elapsed().as_millis() < LEVEL_USER_GRACE_MS as u128);
                if since.elapsed().as_millis() >= LEVEL_RESCUE_MS as u128 && !user_recent {
                    let limit = pinned.ranges[cfg.source].full_scale();
                    let level = ((lo + hi) / 2.0).clamp(-limit, limit);
                    cfg.level_v = level;
                    g.params.trigger.level_v = level;
                    g.level_rescue = Some(LevelRescue {
                        min_v: lo,
                        max_v: hi,
                        level_v: level,
                        at: Instant::now(),
                    });
                    g.level_outside_since = None;
                }
            } else {
                g.level_outside_since = None;
            }
        } else {
            g.level_outside_since = None;
        }
    }
    let found = if mode == trigger::Mode::Free {
        None
    } else {
        source_volts.as_deref().and_then(|s| {
            // Prefer an edge with enough history before it; fall back to the first
            // one so a signal that crosses only once is still captured, with the
            // window flags telling the truth about the missing side.
            trigger::best_crossing(s, &cfg, hysteresis)
                .or_else(|| trigger::first_crossing(s, &cfg, hysteresis))
        })
    };

    let awaiting = g
        .waiting_since
        .map(|t| t.elapsed().as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0);

    match trigger::decide(mode, found.is_some(), awaiting, cfg.timeout_ms) {
        trigger::Outcome::Publish => {
            let win = found
                .map(|t| trigger::window(t, block.n, cfg.pretrigger_pct))
                .unwrap_or_else(|| trigger::Window::whole(block.n));
            publish(g, &block, pinned, generation, dt, &cfg, win, false);
            g.state = CaptureState::Captured;
            g.waiting_since = None;
            if mode == trigger::Mode::Single {
                g.armed = false;
                g.single_done = true;
            }
        }
        trigger::Outcome::PublishFree => {
            let win = trigger::Window::whole(block.n);
            publish(g, &block, pinned, generation, dt, &cfg, win, true);
            g.state = if mode == trigger::Mode::Free {
                CaptureState::Free
            } else {
                CaptureState::AutoFree
            };
            // Auto starts a fresh timeout after every free frame, so a quiet
            // signal gets roughly one screen per timeout rather than a freeze.
            g.waiting_since = None;
        }
        trigger::Outcome::Wait => {
            if g.waiting_since.is_none() {
                g.waiting_since = Some(Instant::now());
            }
            g.state = CaptureState::Waiting;
        }
    }
}

/// Freeze one window of one block into a [`Snapshot`].
#[allow(clippy::too_many_arguments)]
fn publish(
    g: &mut Inner,
    block: &Block,
    pinned: &Params,
    generation: u64,
    dt: f32,
    cfg: &TriggerConfig,
    win: trigger::Window,
    free: bool,
) {
    let n = block.n;
    if n < 2 {
        return;
    }
    let start_i = win.start.floor().max(0.0) as usize;
    let end_i = ((win.start + win.len).ceil() as usize).min(n - 1);
    let start_i = start_i.min(end_i);

    let mut channels = Vec::with_capacity(pinned.enabled.len());
    let mut samples = Vec::with_capacity(pinned.enabled.len());
    let mut stats = Vec::with_capacity(pinned.enabled.len());
    let mut saturation = Vec::with_capacity(pinned.enabled.len());
    let mut dropouts = Vec::with_capacity(pinned.enabled.len());

    for &ch in &pinned.enabled {
        let Some(raw) = &block.ch[ch] else { continue };
        let end = end_i.min(raw.len().saturating_sub(1));
        let start = start_i.min(end);
        let range = pinned.ranges[ch];
        let zero = g.calib.zero(range, ch);
        let vpc = range.volt_per_count();
        // One rail sample between two ordinary ones is a converter dropout, not a
        // 20 V event; repairing it keeps a single glitch from painting a full-scale
        // spike across the trace.
        let (counts, fixed) = crate::capture::repair_dropouts(&raw[start..=end]);
        let volts: Vec<f32> = counts.iter().map(|&c| (c - zero) * vpc).collect();
        if volts.len() < 2 {
            continue;
        }
        stats.push(Measure::of(&volts, dt));
        // Saturation is about the converter, not the drawn scale: a rail-to-rail
        // input on a wide range is not "saturated", it is simply 20 V. Runs of rail
        // samples survive the repair above, so this still reports real clipping.
        saturation.push(volts.iter().any(|v| v.abs() >= range.full_scale() * 0.99));
        dropouts.push(fixed);
        channels.push(ch);
        samples.push(volts);
    }
    if channels.is_empty() {
        return;
    }

    g.publish_seq += 1;
    let snap = Snapshot {
        id: g.publish_seq,
        created_at: Instant::now(),
        generation,
        dt_s: dt as f64,
        channels,
        samples,
        start: win.start,
        len: win.len,
        n_block: n,
        cfg: cfg.clone(),
        free,
        pct: win.pct,
        clipped_pre: win.clipped_pre,
        clipped_post: win.clipped_post,
        range_v: pinned.range_v,
        range_id: pinned.ranges.map(|r| r.id()),
        center_v: pinned.center_v,
        autoscale: pinned.autoscale,
        stats,
        saturation,
        dropouts,
        peak_min: g.peaks.map(|p| p.min),
        peak_max: g.peaks.map(|p| p.max),
        hysteresis_v: cfg.hysteresis(pinned.ranges[cfg.source].resolution()),
    };
    let snap = Arc::new(snap);
    g.recent.push_back(Arc::clone(&snap));
    while g.recent.len() > 48 {
        g.recent.pop_front();
    }
    g.last = Some(snap);
}

/// Sample rate implied by the time base and the number of enabled channels.
///
/// `DAVINCI_FIXED_RATE` still wins, so replay numbers stay comparable across
/// machines.
fn implied_rate(params: &Params) -> f64 {
    let fixed = fixed_rate();
    if fixed > 0.0 {
        return fixed;
    }
    params.time_div.sample_rate(params.enabled.len())
}

/// Time base override for replay: when set, the engine trusts it instead of
/// measuring the link, so numbers stay comparable across machines.
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

#[cfg(test)]
mod tests {
    use super::*;
    use hantek1008::protocol::{Block, ADC_MAX};

    fn inner() -> Inner {
        Inner {
            params: Params::default(),
            calib: Calibration::default(),
            history: VecDeque::new(),
            peaks: [Peak { min: f32::INFINITY, max: f32::NEG_INFINITY }; CHANNELS],
            info_rate: 50_000.0,
            block_samples: 0,
            blocks_per_s: 0.0,
            link: "тест".into(),
            error: None,
            log: None,
            blocks_total: 0,
            frame_counter: 0,
            generation: 0,
            last: None,
            recent: VecDeque::new(),
            held: None,
            publish_seq: 0,
            state: CaptureState::Free,
            waiting_since: None,
            armed: false,
            single_done: false,
            arm_seq: 0,
            level_outside_since: None,
            level_set_at: None,
            level_rescue: None,
            source_span: None,
            link_state: LinkState::Live,
            last_block_at: None,
            consecutive_errors: 0,
            reopen_attempts: 0,
            log_error: None,
        }
    }

    /// A block whose channel 0 carries `volts`, as the ADC would report them.
    fn block(volts: &[f32], range: Range) -> Block {
        let zero = ZERO;
        let vpc = range.volt_per_count();
        let counts: Vec<u16> = volts
            .iter()
            .map(|v| (zero + v / vpc).round().clamp(0.0, ADC_MAX as f32) as u16)
            .collect();
        let n = counts.len();
        let mut ch: [Option<Vec<u16>>; CHANNELS] = std::array::from_fn(|_| None);
        ch[0] = Some(counts);
        Block { seq: 1, t_ns: 0, n, ch }
    }

    const ZERO: f32 = 2048.0;

    fn sine(n: usize, freq: f64, amp: f32, offset: f32, rate: f64) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / rate;
                offset + amp * ((2.0 * std::f64::consts::PI * freq * t) as f32).sin()
            })
            .collect()
    }

    fn run(g: &mut Inner, cfg: TriggerConfig, volts: &[f32], arm_seq: u64) {
        g.params.trigger = cfg.clone();
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        ingest(g, block(volts, Range::V20), &reset, &pinned, g.generation, 50_000.0, arm_seq);
    }

    #[test]
    fn free_mode_publishes_every_block_without_a_trigger() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Free;
        run(&mut g, cfg.clone(), &sine(1000, 125.0, 1.0, 0.0, 50_000.0), 0);
        let first = g.last.as_ref().map(|s| s.id).unwrap();
        assert!(g.last.as_ref().unwrap().free);
        assert_eq!(g.state, CaptureState::Free);
        run(&mut g, cfg, &sine(1000, 125.0, 1.0, 0.0, 50_000.0), 0);
        assert_ne!(g.last.as_ref().map(|s| s.id).unwrap(), first);
    }

    #[test]
    fn normal_mode_waits_when_the_level_is_never_crossed() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = 5.0;
        run(&mut g, cfg, &sine(1000, 125.0, 1.0, 0.0, 50_000.0), 0);
        assert!(g.last.is_none(), "nothing may be published without an edge");
        assert_eq!(g.state, CaptureState::Waiting);
    }

    /// A bus that sits between 0.2 and 2.6 V — the shape that froze the screen:
    /// every sample above the level, so a rising edge can never exist.
    fn bus() -> Vec<f32> {
        (0..1000)
            .map(|i| if (i % 200) < 100 { 0.2 } else { 2.6 })
            .collect()
    }

    #[test]
    fn an_unreachable_level_is_moved_to_the_middle_of_the_signal() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = -2.0;
        // The level has already been watched for longer than the rescue waits.
        g.level_outside_since = Some(Instant::now() - Duration::from_millis(LEVEL_RESCUE_MS + 50));
        run(&mut g, cfg, &bus(), 0);
        let level = g.params.trigger.level_v;
        assert!((level - 1.4).abs() < 0.05, "level {level}");
        assert!(g.last.is_some(), "the moved level must fire on this very block");
        assert_eq!(g.state, CaptureState::Captured);
        let note = g.level_rescue.expect("the panel has to explain the move");
        assert!(note.level_v > 1.0 && note.level_v < 1.8, "note {note:?}");
        assert!(note.min_v < 0.25 && note.max_v > 2.5, "note {note:?}");
    }

    #[test]
    fn a_level_just_below_the_noise_floor_is_still_unreachable() {
        // The bus turns between 10 mV and 2 V, the level is 0 V: 10 mV away, i.e.
        // inside one hysteresis. A naive "level outside min..max" test calls this
        // reachable and leaves the screen frozen — which is exactly how it froze
        // in the field.
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = 0.0;
        g.level_outside_since = Some(Instant::now() - Duration::from_millis(LEVEL_RESCUE_MS + 50));
        let signal: Vec<f32> = (0..1000)
            .map(|i| if (i % 200) < 100 { 0.01 } else { 2.0 })
            .collect();
        run(&mut g, cfg, &signal, 0);
        assert!(g.level_rescue.is_some(), "the level had to move");
        assert!((g.params.trigger.level_v - 1.0).abs() < 0.1);
        assert!(g.last.is_some(), "and it must trigger on the same block");
    }

    #[test]
    fn a_level_inside_the_signal_is_never_moved() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = 1.0;
        g.level_outside_since = Some(Instant::now() - Duration::from_millis(LEVEL_RESCUE_MS * 4));
        run(&mut g, cfg, &bus(), 0);
        assert!((g.params.trigger.level_v - 1.0).abs() < 1e-6);
        assert!(g.level_rescue.is_none());
        assert!(g.last.is_some(), "a reachable level triggers normally");
    }

    #[test]
    fn a_level_the_user_just_set_gets_its_grace_before_the_rescue() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = -2.0;
        g.level_set_at = Some(Instant::now());
        g.level_outside_since = Some(Instant::now() - Duration::from_millis(LEVEL_RESCUE_MS * 4));
        run(&mut g, cfg, &bus(), 0);
        assert!((g.params.trigger.level_v + 2.0).abs() < 1e-6, "the rescue fought the keyboard");
        assert!(g.level_rescue.is_none());
    }

    #[test]
    fn a_flat_input_is_not_rescued_onto_itself() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = 5.0;
        g.level_outside_since = Some(Instant::now() - Duration::from_millis(LEVEL_RESCUE_MS * 4));
        run(&mut g, cfg, &vec![0.3f32; 1000], 0);
        // Nothing moves: there is no signal to centre on, and pretending there is
        // would be worse than waiting.
        assert!((g.params.trigger.level_v - 5.0).abs() < 1e-6);
        assert!(g.level_rescue.is_none());
        let (lo, hi) = g.source_span.expect("the panel still needs the range");
        assert!((hi - lo).abs() < 0.05, "span {lo}..{hi}");
    }

    #[test]
    fn a_single_railed_sample_never_becomes_a_session_peak() {
        // The signal lives around 0,3 V; one sample comes back on the rail. The
        // frame repair drops it, so the "пики за сеанс" line must not advertise a
        // −20 V event that nothing ever showed.
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Free;
        let mut volts = vec![0.3f32; 1000];
        volts[500] = 0.0;
        // A block is built from counts, so a "rail" sample is count 0.
        let range = Range::V20;
        let vpc = range.volt_per_count();
        let counts: Vec<u16> = volts
            .iter()
            .map(|v| (ZERO + v / vpc).round().clamp(0.0, ADC_MAX as f32) as u16)
            .collect();
        let mut raw = counts.clone();
        raw[500] = 0;
        let mut ch: [Option<Vec<u16>>; CHANNELS] = std::array::from_fn(|_| None);
        ch[0] = Some(raw);
        let block = Block { seq: 1, t_ns: 0, n: counts.len(), ch };
        g.params.trigger = cfg;
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        let generation = g.generation;
        ingest(&mut g, block, &reset, &pinned, generation, 50_000.0, 0);
        let peak = g.peaks[0];
        assert!(
            peak.min > -1.0,
            "session peak kept the railed sample: {}",
            peak.min
        );
        assert!(peak.max < 1.0, "session peak max {}", peak.max);
    }

    #[test]
    fn extremes_ignores_nan_and_empty_blocks() {
        assert_eq!(extremes(&[]), (0.0, 0.0));
        assert_eq!(extremes(&[f32::NAN, 1.0, -2.0]), (-2.0, 1.0));
    }

    #[test]
    fn normal_mode_captures_on_the_edge_and_places_it_at_the_pretrigger() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Normal;
        cfg.level_v = 0.5;
        cfg.pretrigger_pct = 30.0;
        run(&mut g, cfg, &sine(4000, 125.0, 1.0, 0.0, 50_000.0), 0);
        let snap = g.last.as_ref().expect("a frame");
        assert!(!snap.free);
        assert_eq!(g.state, CaptureState::Captured);
        assert!((snap.pct - 30.0).abs() < 0.5, "pct {}", snap.pct);
        // The edge really is where the window claims it is.
        let idx = (snap.pct as f64 / 100.0 * snap.len) as usize;
        let s = &snap.samples[0];
        let at = s[idx.min(s.len() - 1)];
        assert!((at - 0.5).abs() < 0.15, "voltage at T0 is {at}");
    }

    #[test]
    fn single_shot_captures_once_per_arm() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Single;
        cfg.level_v = 0.5;
        g.params.trigger = cfg.clone();
        g.armed = true;
        g.arm_seq = 1;
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        ingest(&mut g, block(&sine(4000, 125.0, 1.0, 0.0, 50_000.0), Range::V20), &reset, &pinned, 0, 50_000.0, 1);
        let id = g.last.as_ref().expect("captured").id;
        assert!(g.single_done && !g.armed);
        assert_eq!(g.state, CaptureState::Captured);

        // A second block without a new arm must not replace the capture.
        ingest(&mut g, block(&sine(4000, 125.0, 1.0, 0.0, 50_000.0), Range::V20), &reset, &pinned, 0, 50_000.0, 1);
        assert_eq!(g.last.as_ref().map(|s| s.id), Some(id));
    }

    #[test]
    fn a_block_read_before_the_arm_cannot_satisfy_it() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Single;
        cfg.level_v = 0.5;
        g.params.trigger = cfg;
        g.armed = true;
        g.arm_seq = 5;
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        // The read started when arm_seq was 4: it belongs to the previous state.
        ingest(&mut g, block(&sine(4000, 125.0, 1.0, 0.0, 50_000.0), Range::V20), &reset, &pinned, 0, 50_000.0, 4);
        assert!(g.last.is_none(), "a stale block must not be published");
        assert!(g.armed);
    }

    #[test]
    fn a_published_frame_keeps_the_metadata_of_its_own_block() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Free;
        g.params.trigger = cfg;
        g.params.range_v = [1.0; CHANNELS];
        g.params.center_v[0] = 0.5;
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        ingest(&mut g, block(&sine(1000, 125.0, 1.0, 0.0, 50_000.0), Range::V20), &reset, &pinned, 7, 50_000.0, 0);

        // The user changes the scale afterwards; the frame must not follow.
        g.params.range_v = [20.0; CHANNELS];
        g.params.center_v[0] = 0.0;
        let snap = g.last.as_ref().unwrap();
        assert_eq!(snap.generation, 7);
        assert!((snap.range_v[0] - 1.0).abs() < 1e-6);
        assert!((snap.center_v[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn statistics_come_from_the_frames_own_samples() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Free;
        run(&mut g, cfg, &sine(4000, 125.0, 1.0, 0.0, 50_000.0), 0);
        let snap = g.last.as_ref().unwrap();
        let m = snap.stats[0];
        assert!((m.vmax - 1.0).abs() < 0.05, "vmax {}", m.vmax);
        assert!((m.vmin + 1.0).abs() < 0.05, "vmin {}", m.vmin);
        assert!((m.freq.unwrap() - 125.0).abs() < 3.0, "freq {:?}", m.freq);
    }

    #[test]
    fn hold_pins_the_frame_while_acquisition_moves_on() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Free;
        run(&mut g, cfg.clone(), &sine(1000, 125.0, 1.0, 0.0, 50_000.0), 0);
        let held = g.last.clone().unwrap();
        g.held = Some(held.clone());
        run(&mut g, cfg, &sine(1000, 250.0, 1.0, 0.0, 50_000.0), 0);
        assert_ne!(g.last.as_ref().unwrap().id, held.id);
        assert_eq!(g.held.as_ref().unwrap().id, held.id);
    }

    #[test]
    fn recent_frames_are_bounded() {
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Free;
        g.params.trigger = cfg;
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        for _ in 0..70 {
            ingest(&mut g, block(&sine(200, 125.0, 1.0, 0.0, 50_000.0), Range::V20), &reset, &pinned, 0, 50_000.0, 0);
        }
        assert_eq!(g.recent.len(), 48);
        assert_eq!(g.publish_seq, 70);
    }

    #[test]
    fn a_stale_acquisition_reports_zero_rate_and_an_age() {
        // The last computed rate must not survive the outage: the UI otherwise
        // shows "16 блок/с" while nothing has arrived for minutes.
        let mut g = inner();
        g.info_rate = 600_000.0;
        g.blocks_per_s = 16.0;
        g.block_samples = 1000;
        g.last_block_at = Some(Instant::now() - Duration::from_secs(4));
        let st = status_from(&g, true);
        assert_eq!(st.blocks_per_s, 0.0);
        assert_eq!(st.block_samples, 0);
        assert!(st.last_block_age_ms.expect("age") >= 3_900);
    }

    #[test]
    fn a_held_frame_keeps_its_age_and_the_link_state_is_reported() {
        // A frozen picture has two very different explanations: a held frame and
        // a dead link. The status must carry both facts separately.
        let mut g = inner();
        let mut cfg = TriggerConfig::default();
        cfg.mode = trigger::Mode::Single;
        cfg.level_v = 0.5;
        g.params.trigger = cfg;
        g.armed = true;
        g.arm_seq = 1;
        let pinned = g.params.clone();
        let reset = AtomicBool::new(false);
        ingest(
            &mut g,
            block(&sine(4000, 125.0, 1.0, 0.0, 50_000.0), Range::V20),
            &reset,
            &pinned,
            0,
            50_000.0,
            1,
        );
        g.held = g.last.clone();
        g.link_state = LinkState::Offline;
        let st = status_from(&g, true);
        assert_eq!(st.link_state, "offline");
        assert!(st.held);
        assert_eq!(st.frame_id, 1);
        assert!(st.frame_age_ms.is_some());
    }

    #[test]
    fn a_missing_device_is_offline_not_a_stopped_acquisition() {
        let mut g = inner();
        g.link_state = LinkState::Offline;
        g.last_block_at = None;
        let st = status_from(&g, true);
        assert_eq!(st.link_state, "offline");
        assert!(st.running);
        assert_eq!(st.blocks_per_s, 0.0);
    }
}
