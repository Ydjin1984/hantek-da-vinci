//! The entry point the user actually runs: no arguments, one window, one URL.
//!
//! Responsibilities kept here (and nowhere else): single instance per machine,
//! choosing a port, opening the browser, putting the state directory somewhere
//! writable, and turning "no instrument" into a UI that says so instead of a
//! console that dies. Acquisition stays in `engine`, HTTP in `http`.

use crate::engine::Engine;
use crate::http::{self, Assets};
use hantek1008::sim::SimTransport;
use hantek1008::{Device, CHANNELS};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
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
    let sim = has(args, "--sim");
    let no_open = has(args, "--no-open");
    let ui_dir = crate::arg_value(args, "--ui").map(PathBuf::from);
    let want: u16 = crate::arg_value(args, "--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let instance = acquire_instance();
    if !instance.is_first && !has(args, "--multi") {
        if let Some(port) = probe_running() {
            let url = format!("http://127.0.0.1:{port}/");
            println!("{APP_NAME} уже запущен: {url}");
            if !no_open {
                open_browser(&url);
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

    banner(port, sim, &assets, &state);

    let device = if sim {
        println!("режим симулятора: железо не используется");
        std::env::set_var("DAVINCI_FIXED_RATE", "50000");
        Some(sim_device())
    } else {
        open_hardware()
    };

    let eng = match device {
        Some(mut dev) => {
            if !sim {
                match dev.measure_rate(8) {
                    Ok(i) => println!(
                        "частота: {:.0} Гц/канал, {} сэмплов в блоке, {:.0} блоков/с",
                        i.sample_rate, i.block_samples, i.blocks_per_s
                    ),
                    Err(e) => eprintln!("частота не измерена: {e}"),
                }
            } else {
                dev.info.sample_rate = 50_000.0;
                dev.info.block_samples = 500;
                dev.info.blocks_per_s = 100.0;
            }

            let calib_path = state.join("calibration.json");
            dev.calib = hantek1008::Calibration::load(&calib_path);
            if dev.calib.measured_at.is_some() {
                println!("калибровка нулей: {}", calib_path.display());
            }

            let engine = Engine::new(&dev);
            let handle = engine.handle();
            std::thread::spawn(move || engine.run(dev));
            handle
        }
        None => {
            let link = "нет прибора".to_string();
            let reason = "Осциллограф не отвечает. Проверьте кабель и запустите «davinci doctor».".to_string();
            println!("\n{reason}\nинтерфейс всё равно откроется — в нём видно состояние связи.");
            Engine::offline(link, reason).handle()
        }
    };

    let url = format!("http://127.0.0.1:{port}/");
    println!("\nинтерфейс: {url}");
    println!("остановить: Ctrl+C\n");
    if !no_open {
        open_browser(&url);
    }

    http::serve(port, assets, eng)
}

fn sim_device() -> Device {
    let mut d = Device::with_transport(Box::new(SimTransport::new()));
    d.set_channels(&(0..CHANNELS).collect::<Vec<_>>()).unwrap();
    d.info.link = "simulator".into();
    d
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
                eprintln!("подсказка: подключите прибор по USB. Без него доступен режим --sim.");
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

fn banner(port: u16, sim: bool, assets: &Assets, state: &std::path::Path) {
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
    if sim {
        println!("  прибор:    симулятор");
    } else {
        let st = crate::driver::status();
        println!("  прибор:    {}", st.summary());
    }
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

    if ready {
        match crate::open() {
            Ok(_) => println!("  связь     : прибор открылся и прошёл инициализацию"),
            Err(e) => {
                println!("  связь     : ОШИБКА — {e}");
                problems += 1;
            }
        }
    }

    let port = remembered_port().unwrap_or(DEFAULT_PORT);
    let busy = TcpListener::bind(("127.0.0.1", port)).is_err();
    println!(
        "  порт      : {port} {}",
        if busy { "(занят — вероятно, программа уже запущена)" } else { "(свободен)" }
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
