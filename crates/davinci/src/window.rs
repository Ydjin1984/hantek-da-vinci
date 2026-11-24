//! The window the program actually is.
//!
//! Two windows, one process. A frameless dark plate carries the mark while the
//! instrument is opened and the sample rate measured; behind it the real window
//! is already loading, so when the plate has burned for its two seconds and
//! dissolved, the program is simply there. The interface itself is unchanged —
//! still a page talking to `127.0.0.1` — but the frame belongs to this process,
//! so the program is a window and not a browser tab.
//!
//! Why an event loop on the main thread: `tao` insists on it. Everything slow
//! (opening the instrument, measuring the rate, binding the socket) is moved to
//! a thread by `app`, which reports back with [`Msg::Ready`]. If the instrument
//! is slow the mark simply stays up a little longer — which is honest, and much
//! better than a window that says "нет прибора" for a second and then changes
//! its mind.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Icon, Theme, Window, WindowBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

include!(concat!(env!("OUT_DIR"), "/window_icon.rs"));

/// Also used to find the running copy when a second one is started.
pub const TITLE: &str = "Осциллограф Хелтег да Винчи";

/// How long the mark burns before it starts to dissolve.
const HOLD: Duration = Duration::from_millis(2000);
/// The dissolve. The plate is gone when it ends.
const FADE: Duration = Duration::from_millis(520);
/// Edge of the square the mark sits in.
const SPLASH: f64 = 480.0;

/// The program's own background, `#080b10`, so the webview never flashes white.
const BG: wry::RGBA = (8, 11, 16, 255);

/// Messages from the rest of the program to the window thread.
pub enum Msg {
    /// Instrument, engine and interface server are up; the program may open.
    Ready,
    /// The two seconds are over — fade if the program is ready, wait if not.
    Tick,
    /// The dissolve finished; drop the plate.
    Swap,
    /// A message from the page that only exists to wake the loop.
    Ping,
    /// The renderer reported a failure: reload instead of leaving the last
    /// painted frame on screen.
    Reload,
}

/// How often the watchdog looks at the page's request counter.
const WATCHDOG_EVERY: Duration = Duration::from_secs(5);
/// How long the page may stop making requests before it is considered frozen.
const WATCHDOG_SILENCE: Duration = Duration::from_secs(15);
/// Reload attempts before giving up and leaving the window alone.
const WATCHDOG_MAX_RELOADS: u32 = 5;

/// Run the program's window. Never returns: closing it ends the process.
pub fn run(
    event_loop: EventLoop<Msg>,
    proxy: EventLoopProxy<Msg>,
    url: String,
    ui_beat: Arc<AtomicU64>,
) -> ! {
    let icon = window_icon();
    trace("run: start");

    // One browser environment for both windows. The data directory must be
    // writable: WebView2's default lives next to the executable, which after
    // installation is `Program Files` — and a user cannot write there.
    let mut context = WebContext::new(Some(crate::app::state_dir().join("webview")));

    let splash_window = build_splash(&event_loop, icon.clone());
    let splash_report = proxy.clone();
    let splash_view = WebViewBuilder::new_with_web_context(&mut context)
        .with_background_color(BG)
        .with_html(splash_html())
        .with_ipc_handler(move |request| {
            trace(&format!("splash: {}", request.body()));
            let _ = splash_report.send_event(Msg::Ping);
        })
        .build(&splash_window)
        .expect("заставка");
    splash_window.set_visible(true);
    splash_window.set_focus();
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        // Asked for after the window is on screen: DWM ignores the corner
        // preference of a window it has not composed yet.
        native::round_corners(splash_window.hwnd());
    }
    let shown_at = Instant::now();
    trace("run: plate up");

    // Loads underneath the plate, so its own start-up is not on the clock.
    let (app_window, app_view) = build_app(&event_loop, &mut context, icon, &url);
    trace("run: app loaded");

    // A renderer or GPU process that died takes the canvas with it; WebView2's
    // recovery is invisible from the page, so ask the event loop to reload.
    {
        use wry::WebViewExtWindows;
        let webview = app_view.webview();
        let proxy = proxy.clone();
        let handler = webview2_com::ProcessFailedEventHandler::create(Box::new(
            move |_sender, args| {
                let mut kind = Default::default();
                let label = match args {
                    Some(args) if unsafe { args.ProcessFailedKind(&mut kind) }.is_ok() => {
                        format!("{kind:?}")
                    }
                    _ => "?".into(),
                };
                trace(&format!("webview: процесс упал: {label}"));
                let _ = proxy.send_event(Msg::Reload);
                Ok(())
            },
        ));
        let mut token = 0i64;
        unsafe {
            let _ = webview.add_ProcessFailed(&handler, &mut token);
        }
    }

    let timer = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(HOLD);
        let _ = timer.send_event(Msg::Tick);
    });

    let mut splash = Some((splash_window, splash_view));
    let app_id = app_window.id();
    let app = Some((app_window, app_view));
    let mut ready = false;
    let mut fading = false;
    let mut swap = false;
    let mut last_beat = ui_beat.load(Ordering::Relaxed);
    let mut last_beat_at = Instant::now();
    let mut reloads = 0u32;

    event_loop.run(move |event, _target, control_flow| {
        // A timer, not an endless wait: the watchdog below must run even when
        // nothing else happens in the window.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + WATCHDOG_EVERY);

        match event {
            // Only a close click matters; mouse, resize and focus noise does not.
            Event::WindowEvent { event: WindowEvent::CloseRequested, window_id, .. } => {
                if app_id == window_id || splash.is_some() {
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            Event::UserEvent(Msg::Ready) => {
                ready = true;
                trace("msg: ready");
            }
            Event::UserEvent(Msg::Tick) => trace("msg: tick"),
            Event::UserEvent(Msg::Swap) => swap = true,
            Event::UserEvent(Msg::Ping) => {}
            Event::UserEvent(Msg::Reload) => {
                trace("msg: reload");
                if let Some((_, view)) = app.as_ref() {
                    let _ = view.reload();
                }
                last_beat = ui_beat.load(Ordering::Relaxed);
                last_beat_at = Instant::now();
            }
            _ => {}
        }

        // One place decides the transition, so "ready" and "two seconds are up"
        // may arrive in either order and neither is lost.
        if ready && !fading && shown_at.elapsed() >= HOLD {
            fading = true;
            trace("fade: start");
            if let Some((_, view)) = &splash {
                match view.evaluate_script(
                    "document.body.classList.add('gone');",
                ) {
                    Ok(()) => trace("fade: script ok"),
                    Err(e) => trace(&format!("fade: script failed: {e}")),
                }
            }
            let proxy = proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(FADE);
                let _ = proxy.send_event(Msg::Swap);
            });
        }

        if swap && splash.is_some() {
            trace("swap: drop plate");
            if let Some((window, view)) = splash.take() {
                drop(view);
                drop(window);
            }
            // The program takes the place the plate leaves behind.
            if let Some((window, _)) = &app {
                window.set_visible(true);
                window.set_focus();
            }
            last_beat = ui_beat.load(Ordering::Relaxed);
            last_beat_at = Instant::now();
        }

        // The page polls the local server many times a second; that counter is
        // the only honest sign the *display* is alive. The renderer can wedge
        // while the acquisition thread keeps running, and then the window shows
        // the last painted frame for ever. Reload instead of asking the user to
        // restart the program.
        if swap {
            if let Some((window, view)) = app.as_ref() {
                if window.is_visible() && !window.is_minimized() {
                    let beat = ui_beat.load(Ordering::Relaxed);
                    if beat != last_beat {
                        last_beat = beat;
                        last_beat_at = Instant::now();
                    } else if last_beat_at.elapsed() >= WATCHDOG_SILENCE {
                        if reloads < WATCHDOG_MAX_RELOADS {
                            trace("watchdog: страница молчит — перезагружаю");
                            let _ = view.reload();
                            reloads += 1;
                        } else {
                            trace("watchdog: страница молчит, перезагрузки исчерпаны");
                        }
                        last_beat_at = Instant::now();
                    }
                } else {
                    // A minimised or hidden window stops timers and rAF by
                    // design; that is not a freeze.
                    last_beat = ui_beat.load(Ordering::Relaxed);
                    last_beat_at = Instant::now();
                }
            }
        }
    })
}

/// The mark, on a dark plate: no frame, no title, no taskbar entry.
fn build_splash<T>(target: &EventLoopWindowTarget<T>, icon: Option<Icon>) -> Window {
    let window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(SPLASH, SPLASH))
        .with_resizable(false)
        .with_decorations(false)
        .with_always_on_top(true)
        .with_window_icon(icon)
        .build(target)
        .expect("окно заставки");
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        // A shadow under a frameless window is drawn as a grey rectangle.
        window.set_undecorated_shadow(false);
        // Two seconds of start-up is not a taskbar presence.
        let _ = window.set_skip_taskbar(true);
        let hwnd = window.hwnd();
        native::dark_brush(hwnd);
    }
    place_center(&window);
    window
}

fn build_app<T>(
    target: &EventLoopWindowTarget<T>,
    context: &mut WebContext,
    icon: Option<Icon>,
    url: &str,
) -> (Window, WebView) {
    let window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(main_size(target))
        .with_min_inner_size(LogicalSize::new(960.0, 600.0))
        .with_theme(Some(Theme::Dark))
        .with_visible(false)
        .with_window_icon(icon)
        .build(target)
        .expect("окно программы");
    place_center(&window);

    let view = WebViewBuilder::new_with_web_context(context)
        .with_url(url)
        .with_background_color(BG)
        // A hard cut between a dissolved mark and a full window looks like a
        // flicker, so the content itself arrives with a short fade.
        .with_initialization_script(
            r#"addEventListener("DOMContentLoaded", function () {
                 document.documentElement.animate(
                   [{ opacity: 0 }, { opacity: 1 }],
                   { duration: 260, easing: "ease-out" }
                 );
               });"#,
        )
        .build(&window)
        .expect("интерфейс");
    (window, view)
}

/// `1440x900`, unless the screen is smaller than that plus a margin.
fn main_size<T: 'static>(target: &EventLoopWindowTarget<T>) -> LogicalSize<f64> {
    let (mut w, mut h): (f64, f64) = (1440.0, 900.0);
    if let Some(monitor) = target.primary_monitor() {
        let screen = monitor.size().to_logical::<f64>(monitor.scale_factor());
        w = w.min(screen.width - 60.0);
        h = h.min(screen.height - 100.0);
    }
    LogicalSize::new(w.max(960.0), h.max(600.0))
}

fn place_center(window: &Window) {
    let Some(monitor) = window.current_monitor() else { return };
    let screen = monitor.size();
    let origin = monitor.position();
    let size = window.outer_size();
    let x = origin.x + (screen.width as i32 - size.width as i32) / 2;
    let y = origin.y + (screen.height as i32 - size.height as i32) / 2;
    window.set_outer_position(PhysicalPosition::new(x.max(origin.x), y.max(origin.y)));
}

/// The icon in the executable, as the pixels a window wants.
fn window_icon() -> Option<Icon> {
    if ICON_W == 0 || ICON_H == 0 {
        return None;
    }
    Icon::from_rgba(ICON_RGBA.to_vec(), ICON_W, ICON_H).ok()
}

/// `assets/splash.html` with the mark inlined.
///
/// Inlined, not linked: this page is shown before the interface server exists,
/// so it cannot fetch anything. WebP keeps the picture at 70 КБ instead of the
/// 485 КБ the PNG needed, which matters because it travels as a data: URL.
fn splash_html() -> String {
    const LOGO: &[u8] = include_bytes!("../assets/splash-logo.webp");
    let mut data = String::from("data:image/webp;base64,");
    base64(LOGO, &mut data);
    include_str!("../assets/splash.html").replace("{{LOGO}}", &data)
}

/// Standard base64. Twenty lines beats a dependency used exactly once.
fn base64(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - i * 6)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
}

/// Bring the window of an already running copy to the front.
///
/// Windows will do this by window title, so a second start needs no IPC and
/// works even when the first copy came from a different shortcut.
#[cfg(windows)]
pub fn raise_existing() -> bool {
    use std::ffi::c_void;
    #[link(name = "user32")]
    extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut c_void;
        fn ShowWindow(window: *mut c_void, cmd: i32) -> i32;
        fn IsIconic(window: *mut c_void) -> i32;
        fn SetForegroundWindow(window: *mut c_void) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    let title: Vec<u16> = TITLE.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let window = FindWindowW(std::ptr::null(), title.as_ptr());
        if window.is_null() {
            return false;
        }
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window);
        true
    }
}

/// The two Windows calls that make the plate look intentional.#[cfg(windows)]
mod native {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        fn SetClassLongPtrW(window: *mut c_void, index: i32, value: isize) -> isize;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateSolidBrush(color: u32) -> *mut c_void;
    }
    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(window: *mut c_void, attribute: u32, value: *const c_void, size: u32) -> i32;
    }

    /// A window erases itself with white until a webview paints over it. The
    /// plate must be dark from the first frame, or the start of the program is
    /// a white flash — so the class brush is replaced before the window shows.
    ///
    /// The brush deliberately outlives this call: freeing it would leave every
    /// later window of this class erasing with a dangling handle.
    pub fn dark_brush(hwnd: isize) {
        const GCLP_HBRBACKGROUND: i32 = -10;
        // COLORREF is 0x00BBGGRR: #080b10 -> 0x10 0b 08.
        let brush = unsafe { CreateSolidBrush(0x0010_0b08) };
        if brush.is_null() {
            return;
        }
        unsafe {
            SetClassLongPtrW(hwnd as *mut c_void, GCLP_HBRBACKGROUND, brush as isize);
        }
    }

    /// Windows 11 rounds the corners of a frameless window; older systems just
    /// say no and the plate stays square, which is not a problem.
    pub fn round_corners(hwnd: isize) {
        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;
        unsafe {
            DwmSetWindowAttribute(
                hwnd as *mut c_void,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &DWMWCP_ROUND as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
}

/// Append a line to `%LOCALAPPDATA%\DaVinci\window.log` when `DAVINCI_DEBUG`
/// is set.
///
/// Start-up timing is the one thing that cannot be checked after the fact and
/// is exactly what breaks on someone else's machine, so the sequence leaves a
/// trace instead of being guessed at.
pub fn trace(message: &str) {
    if std::env::var_os("DAVINCI_DEBUG").is_none() {
        return;
    }
    use std::io::Write;
    let path = crate::app::state_dir().join("window.log");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{:?} {message}", std::time::SystemTime::now());
    }
}
