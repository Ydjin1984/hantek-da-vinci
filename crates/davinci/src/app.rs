//! The entry point the user actually runs: one window, one process, one URL.
//!
//! Responsibilities kept here (and nowhere else): single instance per machine,
//! choosing a port, putting the state directory somewhere writable, and turning
//! "no instrument" into a UI that says so instead of a console that dies.
//! Acquisition stays in `engine`, HTTP in `http`, the native window in `window`.

use crate::engine::{Engine, EngineHandle};
use crate::http::{self, Assets};
use hantek1008::Device;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 7411;
pub const APP_NAME: &str = "Осциллограф Хелтег да Винчи";
const MUTEX_NAME: &str = "Local\\DaVinci-Hantek-1008C";

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attr: *mut std::ffi::c_void, initial: i32, name: *const u16) -> *mut std::ffi::c_void;
    fn GetLastError() -> u32;
}

const ERROR_ALREADY_EXISTS: u32 = 183;

struct Instance {
    is_first: bool,
    /// Kept alive for the process lifetime; dropping it would release the lock.
    _handle: *mut std::ffi::c_void,
}

fn acquire_instance() -> Instance {
    let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        let is_first = GetLastError() != ERROR_ALREADY_EXISTS;
        Instance { is_first, _handle: handle }
    }
}

/// Where the program keeps its own files: calibration, logs, the port number.
///
/// `Program Files` is not writable for a normal user, so nothing that changes at
/// runtime may live next to the executable.
pub fn state_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = PathBuf::from(base).join("DaVinci");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn port_file() -> PathBuf {
    state_dir().join("port.txt")
}

fn remember_port(port: u16) {
    let _ = std::fs::write(port_file(), port.to_string());
}

fn remembered_port() -> Option<u16> {
    std::fs::read_to_string(port_file()).ok()?.trim().parse().ok()
}

/// Ask the remembered port whether *our* server is behind it.
fn probe_running() -> Option<u16> {
    let port = remembered_port()?;
    let mut stream = TcpStream::connect_timeout(
        &([127, 0, 0, 1], port).into(),
        Duration::from_millis(400),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(800))).ok()?;
    stream.write_all(b"GET /api/info HTTP/1.0\r\n\r\n").ok()?;
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    if text.starts_with("HTTP/") && text.contains("\"link\"") {
        Some(port)
    } else {
        None
    }
}

pub fn open_browser(url: &str) {
    use std::os::windows::process::CommandExt;
    // `raw_arg` keeps our own quoting: `start "" <url>` needs the empty title,
    // and the Rust runtime would re-quote the whole thing for a C runtime that
    // cmd.exe does not parse the same way.
    let _ = std::process::Command::new("cmd")
        .raw_arg("/c")
        .raw_arg(format!("start \"\" \"{url}\""))
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn();
}

fn pick_port(want: u16) -> u16 {
    for port in want..want.saturating_add(40) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    want
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Start the whole program. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let headless = has(args, "--no-open");
    let browser = has(args, "--browser");
    let ui_dir = crate::arg_value(args, "--ui").map(PathBuf::from);
    let want: u16 = crate::arg_value(args, "--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let instance = acquire_instance();
    if !instance.is_first && !has(args, "--multi") {
        if let Some(port) = probe_running() {
            let url = format!("http://127.0.0.1:{port}/");
            println!("{APP_NAME} уже запущен: {url}");
            if !headless && !show_existing(&url, browser) {
                println!("окно не найдено — откройте {url} вручную");
            }
            return 0;
        }
    }

    let port = pick_port(want);
    remember_port(port);

    let state = state_dir();
    let logs = state.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    std::env::set_var("DAVINCI_LOGS", &logs);

    let assets = match &ui_dir {
        Some(dir) => Assets::Dir(dir.clone()),
        None => Assets::Embedded,
    };

    banner(port, &assets, &state);

    // A window needs the main thread for its event loop, so the instrument and
    // the interface server move to a thread and the picture appears at once.
    // `--no-open` (interface only) and `--browser` (a page instead of a window)
    // keep the old shape and stay useful for development.
    #[cfg(windows)]
    {
        if !headless && !browser {
            return run_windowed(state, assets, port);
        }
    }

    let eng = start_engine(&state);
    let url = format!("http://127.0.0.1:{port}/");
    println!("\nинтерфейс: {url}");
    println!("остановить: Ctrl+C\n");
    if browser && !headless {
        open_browser(&url);
    }

    http::serve(port, assets, eng)
}

/// Bring the running copy forward instead of starting a second one.
fn show_existing(url: &str, browser: bool) -> bool {
    #[cfg(windows)]
    {
        if crate::window::raise_existing() {
            return true;
        }
    }
    if browser {
        open_browser(url);
        return true;
    }
    false
}

/// The program as a window: the mark for two seconds, then the interface.
///
/// The worker thread does everything that used to happen before the interface
/// was served, and reports when the socket is listening — the window loads that
/// address the instant it is created, so "ready" must mean ready.
#[cfg(windows)]
fn run_windowed(state: PathBuf, assets: Assets, port: u16) -> i32 {
    use tao::event_loop::EventLoopBuilder;

    // Started from Explorer or a shortcut we own a console nobody asked for;
    // it would sit behind the window as a black rectangle all session.
    hide_console();

    let mut builder = EventLoopBuilder::<crate::window::Msg>::with_user_event();
    let event_loop = builder.build();
    let proxy = event_loop.create_proxy();
    let url = format!("http://127.0.0.1:{port}/");

    // The page increments this on every request. The window's watchdog reads it:
    // a frozen renderer stops making requests, and only a reload brings it back.
    let ui_beat = Arc::new(AtomicU64::new(0));
    let beat_for_http = Arc::clone(&ui_beat);

    let worker = proxy.clone();
    std::thread::spawn(move || {
        let eng = start_engine(&state);
        if let Err(e) = http::start(port, assets, eng, beat_for_http) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        let _ = worker.send_event(crate::window::Msg::Ready);
    });

    crate::window::run(event_loop, proxy, url, ui_beat)
}

/// Hide the console window, but only if it is ours.
///
/// Launched from a terminal the console belongs to the person typing, and
/// hiding it would take their window away. `GetConsoleProcessList` answers
/// "how many processes are attached": one means the console was created for
/// this process alone.
#[cfg(windows)]
fn hide_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> *mut std::ffi::c_void;
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn ShowWindow(window: *mut std::ffi::c_void, cmd: i32) -> i32;
    }
    const SW_HIDE: i32 = 0;
    unsafe {
        let console = GetConsoleWindow();
        if console.is_null() {
            return;
        }
        let mut attached = [0u32; 2];
        let processes = GetConsoleProcessList(attached.as_mut_ptr(), 2);
        if processes > 1 {
            crate::window::trace(&format!("console: shared with {processes}, left alone"));
            return;
        }
        let was_visible = ShowWindow(console, SW_HIDE);
        crate::window::trace(&format!("console: hidden, previous state {was_visible}"));
    }
}

/// Instrument, engine thread, and the settings the previous run left behind.
///
/// Runs on a worker thread in window mode and on the main thread otherwise; it
/// never touches the window, which is why it can be either.
fn start_engine(state: &Path) -> EngineHandle {
    match open_hardware() {
        Some(mut dev) => {
            match dev.measure_rate(8) {
                Ok(i) => println!(
                    "частота: {:.0} Гц/канал, {} сэмплов в блоке, {:.0} блоков/с",
                    i.sample_rate, i.block_samples, i.blocks_per_s
                ),
                Err(e) => eprintln!("частота не измерена: {e}"),
            }

            let calib_path = state.join("calibration.json");
            dev.calib = hantek1008::Calibration::load(&calib_path);
            if dev.calib.measured_at.is_some() {
                println!("калибровка нулей: {}", calib_path.display());
            }

            // Last state from the previous run: which channels were on, their
            // ranges, the time base. Applied to the instrument before the engine
            // starts so the very first acquisition is already the right one.
            let settings = crate::settings::Settings::load();
            settings.apply(&mut dev);
            println!(
                "настройки: каналы {:?}, развёртка {}",
                settings.enabled.iter().map(|c| c + 1).collect::<Vec<_>>(),
                hantek1008::TimeDiv::from_id(settings.time_div_id)
                    .map(|t| t.label())
                    .unwrap_or_else(|_| "—".into())
            );

            let engine = Engine::new(&dev);
            let handle = engine.handle();
            handle.restore(&settings);
            std::thread::spawn(move || engine.run(Some(dev)));
            handle
        }
        None => {
            let link = "нет прибора".to_string();
            let reason = "Осциллограф не отвечает. Проверьте кабель и запустите «davinci doctor».".to_string();
            println!("\n{reason}\nинтерфейс всё равно откроется — в нём видно состояние связи.");
            // Still spawn the loop: it keeps trying to open the instrument, so
            // plugging the cable in later is enough. A program that has to be
            // restarted after the device appears is the bug this fixes.
            let engine = Engine::offline(link, reason);
            let handle = engine.handle();
            let settings = crate::settings::Settings::load();
            handle.restore(&settings);
            std::thread::spawn(move || engine.run(None));
            handle
        }
    }
}

fn open_hardware() -> Option<Device> {
    match crate::open() {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("\n{e}");
            if crate::driver::winusb_ready() {
                return None;
            }
            let st = crate::driver::status();
            if !st.found {
                eprintln!("подсказка: прибор не найден. Подключите его по USB и запустите программу заново.");
                return None;
            }
            eprintln!("{}", st.summary());
            if offer_driver_install() {
                if let Err(err) = crate::driver::install() {
                    eprintln!("{err}");
                    return None;
                }
                println!("драйвер установлен, пробую открыть прибор заново…");
                return crate::open().ok();
            }
            None
        }
    }
}

/// One question, default answer "yes", only when someone can answer it.
fn offer_driver_install() -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return false;
    }
    print!("Переключить прибор на WinUSB? Потребуется подтверждение администратора [Y/n]: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes" | "д" | "да")
}

fn banner(port: u16, assets: &Assets, state: &std::path::Path) {
    println!();
    println!("  {APP_NAME} {}", env!("CARGO_PKG_VERSION"));
    println!("  ─────────────────────────────────────────────");
    match assets {
        Assets::Embedded => println!(
            "  интерфейс: встроен в программу ({} файлов, {} КБ)",
            crate::assets::count(),
            crate::assets::bytes().div_ceil(1024)
        ),
        Assets::Dir(dir) => println!("  интерфейс: {} (с диска)", dir.display()),
    }
    println!("  адрес:     http://127.0.0.1:{port}/");
    println!("  состояние: {}", state.display());
    let st = crate::driver::status();
    println!("  прибор:    {}", st.summary());
}

/// `davinci doctor` — the whole chain in one report.
///
/// Written for the moment something does not work: every link between "the program
/// is installed" and "samples arrive" is checked, and each failure names the one
/// command that fixes it.
pub fn doctor(args: &[String]) -> i32 {
    let fix = args.iter().any(|a| a == "--fix");
    println!();
    println!("  {APP_NAME} {} — диагностика", env!("CARGO_PKG_VERSION"));
    println!("  ───────────────────────────────────────────────────────");

    let mut problems = 0;

    println!(
        "  интерфейс : встроен, {} файлов, {} КБ",
        crate::assets::count(),
        crate::assets::bytes().div_ceil(1024)
    );

    let state = state_dir();
    let probe = state.join(".probe");
    let writable = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    println!(
        "  состояние : {}{}",
        state.display(),
        if writable { " (запись ок)" } else { " (НЕТ ЗАПИСИ)" }
    );
    if !writable {
        problems += 1;
    }

    let st = crate::driver::status();
    println!("  прибор    : {}", st.summary());
    if let Some(id) = &st.instance_id {
        println!("  устройство: {id}");
    }
    if st.found {
        println!("  служба    : {}", st.service.as_deref().unwrap_or("—"));
        println!("  класс     : {}", st.class.as_deref().unwrap_or("—"));
        if let Some(p) = st.problem {
            println!("  код ошибки: {p}");
        }
    }

    let ready = crate::driver::winusb_ready();
    println!(
        "  winusb    : {}",
        if ready { "интерфейс найден" } else { "интерфейса нет" }
    );
    if !ready {
        problems += 1;
    }

    // WinUSB is opened exclusively, so a running copy of this program makes the
    // device look broken. Check that first and report it as "busy", not "error".
    let port = remembered_port().unwrap_or(DEFAULT_PORT);
    let busy = TcpListener::bind(("127.0.0.1", port)).is_err();

    if ready {
        if busy {
            println!("  связь     : не проверялась — прибор занят запущенной программой");
        } else {
            match crate::open_silent() {
                Ok(_) => println!("  связь     : прибор открылся и прошёл инициализацию"),
                Err(e) => {
                    println!("  связь     : ОШИБКА — {e}");
                    problems += 1;
                }
            }
        }
    }

    println!(
        "  порт      : {port} {}",
        if busy { "(занят — программа уже запущена)" } else { "(свободен)" }
    );
    println!("  журналы   : {}", state.join("logs").display());

    let (inf, cat, cer) = crate::driver::package_sizes();
    println!("  пакет     : inf {inf} Б, cat {cat} Б, сертификат {cer} Б");

    println!();
    if problems == 0 {
        println!("  всё в порядке.");
        return 0;
    }
    println!("  проблем: {problems}");

    if fix && st.found && !ready {
        println!("\n  ставлю WinUSB…\n");
        return match crate::driver::install() {
            Ok(()) => {
                println!("  готово: прибор на WinUSB, запускайте программу.");
                0
            }
            Err(e) => {
                eprintln!("  не удалось: {e}");
                1
            }
        };
    }

    if !st.found {
        println!("  что делать: подключить прибор по USB и повторить проверку.");
    } else if !ready {
        println!("  что делать: davinci doctor --fix   (или davinci driver install)");
    }
    1
}

