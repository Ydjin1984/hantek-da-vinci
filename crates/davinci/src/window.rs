//! The window the program actually is.
//!
//! Up to three windows, one process. A frameless dark plate carries the mark
//! while the instrument is opened and the sample rate measured; behind it the
//! real window is already loading, so when the plate has burned for its two
//! seconds and dissolved, the program is simply there. The interface itself is
//! unchanged — still a page talking to `127.0.0.1` — but the frame belongs to
//! this process, so the program is a window and not a browser tab.
//!
//! The third window is mini mode: a decorated, always-on-top window with the
//! first three channels. It is built on demand, minimises the main window and
//! disappears again when its button, its close box or a taskbar restore of the
//! main window asks for it. It shares the same HTTP server and capture engine,
//! so switching windows never touches the instrument.
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

/// Заголовок отдельного окна мини-режима.
const MINI_TITLE: &str = "DaVinci Scope — мини-режим";
/// Стартовый размер при 100 % масштабе Windows. Это внутренний размер: рамку
/// заголовка, как и любому обычному окну, добавляет система.
const MINI_W: f64 = 360.0;
const MINI_H: f64 = 280.0;
/// Ниже этого три графика перестают помещаться.
const MINI_MIN_W: f64 = 320.0;
const MINI_MIN_H: f64 = 230.0;
/// Отступ от края рабочей области, когда позиция ещё не запомнена.
const MINI_MARGIN: i32 = 24;
/// Как часто разрешено писать позицию мини-окна на диск во время перетаскивания.
const MINI_SAVE_EVERY: Duration = Duration::from_millis(400);

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
    /// The main page asked for the mini window: minimise the main window and
    /// put the small always-on-top window on screen.
    MiniOpen,
    /// The mini page asked to go back (its button; the native close box arrives
    /// as `CloseRequested`).
    MiniClose,
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
    let (app_window, app_view) =
        build_app(&event_loop, &mut context, icon.clone(), &url, proxy.clone());
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
    let mini_url = format!("{url}mini.html");
    // At most one mini window, ever: every "open" either focuses this one or
    // builds it, and every "close" takes it out of the same slot.
    let mut mini: Option<(Window, WebView)> = None;
    let mut mini_opened_at = Instant::now();
    let mut mini_saved_at = Instant::now();
    let mut ready = false;
    let mut fading = false;
    let mut swap = false;
    let mut last_beat = ui_beat.load(Ordering::Relaxed);
    let mut last_beat_at = Instant::now();
    let mut reloads = 0u32;

    event_loop.run(move |event, target, control_flow| {
        // A timer, not an endless wait: the watchdog below must run even when
        // nothing else happens in the window.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + WATCHDOG_EVERY);

        match event {
            // Only a close click matters; mouse, resize and focus noise does not.
            Event::WindowEvent { event: WindowEvent::CloseRequested, window_id, .. } => {
                if app_id == window_id || splash.is_some() {
                    if let Some((window, _)) = &mini {
                        if let Ok(pos) = window.outer_position() {
                            save_mini_position(pos);
                        }
                    }
                    *control_flow = ControlFlow::Exit;
                    return;
                }
                // The mini window's own close box means "back to the program",
                // not "leave the program".
                if mini.as_ref().is_some_and(|(window, _)| window.id() == window_id) {
                    close_mini(&mut mini, &app);
                    return;
                }
            }
            Event::WindowEvent { event: WindowEvent::Moved(position), window_id, .. } => {
                // Dragging produces a flood of positions; writing every one of
                // them would make the disk the busiest part of the program. The
                // last position is saved on close and on exit as well.
                if mini.as_ref().is_some_and(|(window, _)| window.id() == window_id)
                    && mini_saved_at.elapsed() >= MINI_SAVE_EVERY
                {
                    save_mini_position(position);
                    mini_saved_at = Instant::now();
                }
            }
            // The taskbar icon restores the main window; that is the user
            // leaving mini mode, so the small window goes away with it. The
            // short delay ignores a focus event left over from the click that
            // opened the small window; the fallback below covers a real restore
            // inside that window too.
            Event::WindowEvent { event: WindowEvent::Focused(true), window_id, .. }
                if window_id == app_id
                    && mini.is_some()
                    && mini_opened_at.elapsed() >= Duration::from_millis(600) =>
            {
                close_mini(&mut mini, &app);
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
            Event::UserEvent(Msg::MiniOpen) => {
                if let Some((window, _)) = &mini {
                    window.set_minimized(false);
                    window.set_visible(true);
                    window.set_focus();
                } else {
                    trace("mini: open");
                    if let Some((window, view)) = build_mini(
                        target,
                        &mut context,
                        icon.clone(),
                        &mini_url,
                        proxy.clone(),
                    ) {
                        // Minimise only after the small window exists: a
                        // failure to build it must never leave the program
                        // with no visible window at all.
                        if let Some((main, _)) = &app {
                            main.set_minimized(true);
                        }
                        window.set_focus();
                        mini_opened_at = Instant::now();
                        mini_saved_at = Instant::now();
                        mini = Some((window, view));
                    }
                }
            }
            Event::UserEvent(Msg::MiniClose) => close_mini(&mut mini, &app),
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

        // A restore from the taskbar is also visible as "the main window is no
        // longer minimised". Focus is the usual signal, but the invariant is
        // kept even if that event is missed; the short delay lets the minimise
        // requested on open settle first.
        if mini.is_some()
            && mini_opened_at.elapsed() >= Duration::from_millis(600)
            && app.as_ref().is_some_and(|(window, _)| !window.is_minimized())
        {
            close_mini(&mut mini, &app);
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
    events: EventLoopProxy<Msg>,
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
        // The page cannot create a native topmost window, so it asks this side
        // through the tiny wry IPC bridge. Nothing else is accepted here.
        .with_ipc_handler(move |request| {
            if request.body().trim() == "mini-open" {
                let _ = events.send_event(Msg::MiniOpen);
            }
        })
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

/// The mini window: a real decorated window, so it is dragged by its title and
/// can sit on any monitor, and topmost, so a borderless full-screen program does
/// not bury the traces. It is not modal: the user works in another application
/// while it stays on top.
fn build_mini<T>(
    target: &EventLoopWindowTarget<T>,
    context: &mut WebContext,
    icon: Option<Icon>,
    url: &str,
    events: EventLoopProxy<Msg>,
) -> Option<(Window, WebView)> {
    let window = WindowBuilder::new()
        .with_title(MINI_TITLE)
        .with_inner_size(LogicalSize::new(MINI_W, MINI_H))
        .with_min_inner_size(LogicalSize::new(MINI_MIN_W, MINI_MIN_H))
        .with_theme(Some(Theme::Dark))
        .with_always_on_top(true)
        .with_visible(false)
        .with_window_icon(icon)
        .build(target)
        .expect("мини-окно");
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        native::dark_brush(window.hwnd());
        native::dark_title(window.hwnd());
    }
    place_mini(&window);

    let view = match WebViewBuilder::new_with_web_context(context)
        .with_url(url)
        .with_background_color(BG)
        .with_ipc_handler(move |request| {
            if request.body().trim() == "mini-close" {
                let _ = events.send_event(Msg::MiniClose);
            }
        })
        .build(&window)
    {
        Ok(view) => view,
        Err(e) => {
            // The small window is a convenience; failing to build it must leave
            // the main window exactly where it was, not abort the program.
            trace(&format!("mini: не создан: {e}"));
            drop(window);
            return None;
        }
    };
    window.set_visible(true);
    window.set_focus();
    Some((window, view))
}

/// Put the mini window where it was last time, clamped to a work area that
/// actually exists. A monitor unplugged since the previous run must not leave
/// the window at coordinates nobody can see.
fn place_mini(window: &Window) {
    let size = window.outer_size();
    let (x, y) = match load_mini_position() {
        Some(pos) => native::clamp_to_work_area(pos.x, pos.y, size.width as i32, size.height as i32),
        None => native::default_mini_position(size.width as i32, size.height as i32),
    };
    window.set_outer_position(PhysicalPosition::new(x, y));
}

fn mini_position_file() -> std::path::PathBuf {
    crate::app::state_dir().join("mini-window.txt")
}

fn load_mini_position() -> Option<PhysicalPosition<i32>> {
    let text = std::fs::read_to_string(mini_position_file()).ok()?;
    let (x, y) = text.trim().split_once(',')?;
    Some(PhysicalPosition::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn save_mini_position(pos: PhysicalPosition<i32>) {
    let _ = std::fs::write(mini_position_file(), format!("{},{}", pos.x, pos.y));
}

/// Drop the mini window and bring the main one back. Used by the back button,
/// the native close box and a restore from the taskbar.
fn close_mini(mini: &mut Option<(Window, WebView)>, app: &Option<(Window, WebView)>) {
    let Some((window, view)) = mini.take() else { return };
    if let Ok(pos) = window.outer_position() {
        save_mini_position(pos);
    }
    // The webview first: it borrows the window's handle.
    drop(view);
    drop(window);
    if let Some((window, _)) = app {
        window.set_minimized(false);
        window.set_visible(true);
        window.set_focus();
    }
    trace("mini: closed, main restored");
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

/// The Windows calls that make the windows look intentional.
#[cfg(windows)]
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

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: Rect,
        rc_work: Rect,
        flags: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn MonitorFromPoint(point: Point, flags: u32) -> *mut c_void;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
    }

    const MONITOR_DEFAULTTONULL: u32 = 0;
    const MONITOR_DEFAULTTOPRIMARY: u32 = 1;

    /// The part of a monitor not covered by the taskbar, for the monitor that
    /// owns a point. `None` when the point is on no monitor at all — which is
    /// exactly what a disconnected second screen leaves behind.
    fn work_area_at(point: Point) -> Option<Rect> {
        unsafe {
            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONULL);
            if monitor.is_null() {
                return None;
            }
            let mut info = MonitorInfo {
                cb_size: std::mem::size_of::<MonitorInfo>() as u32,
                rc_monitor: Rect { left: 0, top: 0, right: 0, bottom: 0 },
                rc_work: Rect { left: 0, top: 0, right: 0, bottom: 0 },
                flags: 0,
            };
            if GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            Some(info.rc_work)
        }
    }

    fn cursor_work_area() -> Option<Rect> {
        let mut point = Point { x: 0, y: 0 };
        let ok = unsafe { GetCursorPos(&mut point) };
        let area = if ok != 0 { work_area_at(point) } else { None };
        area.or_else(|| {
            // No cursor (a service, a locked session): the primary monitor is
            // still the safe answer.
            unsafe {
                let monitor = MonitorFromPoint(Point { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
                if monitor.is_null() {
                    return None;
                }
                let mut info = MonitorInfo {
                    cb_size: std::mem::size_of::<MonitorInfo>() as u32,
                    rc_monitor: Rect { left: 0, top: 0, right: 0, bottom: 0 },
                    rc_work: Rect { left: 0, top: 0, right: 0, bottom: 0 },
                    flags: 0,
                };
                (GetMonitorInfoW(monitor, &mut info) != 0).then_some(info.rc_work)
            }
        })
    }

    /// `(x, y)` for a saved mini-window position, kept fully inside a working
    /// area. A position from a monitor that is no longer connected is clamped
    /// into the work area of the monitor under the cursor instead.
    pub fn clamp_to_work_area(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
        // The title bar is the part the user must be able to grab; checking its
        // middle also rejects a saved point that is a few pixels off-screen.
        let saved = work_area_at(Point { x: x + width / 2, y: y + 12 });
        let some_area = saved.or_else(cursor_work_area);
        let Some(area) = some_area else {
            return (x.max(0), y.max(0));
        };
        (
            x.clamp(area.left, (area.right - width).max(area.left)),
            y.clamp(area.top, (area.bottom - height).max(area.top)),
        )
    }

    /// First position: top-right of the monitor under the cursor, clear of the
    /// taskbar and of the screen edge.
    pub fn default_mini_position(width: i32, height: i32) -> (i32, i32) {
        let Some(area) = cursor_work_area() else {
            return (100, 100);
        };
        (
            (area.right - width - super::MINI_MARGIN)
                .clamp(area.left, (area.right - width).max(area.left)),
            (area.top + super::MINI_MARGIN)
                .clamp(area.top, (area.bottom - height).max(area.top)),
        )
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

    /// A dark title bar, so the mini window does not look like a white strip
    /// pasted onto the dark interface. Windows 10 1809+ and Windows 11 accept
    /// 20; the earlier attribute number was 19.
    pub fn dark_title(hwnd: isize) {
        const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
        const DWMWA_USE_IMMERSIVE_DARK_MODE_OLD: u32 = 19;
        let on: i32 = 1;
        unsafe {
            let attr = &on as *const i32 as *const c_void;
            let size = std::mem::size_of::<i32>() as u32;
            let code = DwmSetWindowAttribute(hwnd as *mut c_void, DWMWA_USE_IMMERSIVE_DARK_MODE, attr, size);
            if code != 0 {
                DwmSetWindowAttribute(
                    hwnd as *mut c_void,
                    DWMWA_USE_IMMERSIVE_DARK_MODE_OLD,
                    attr,
                    size,
                );
            }
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
