//! Command line entry point.
//!
//! With no arguments this starts the program a person installs: the interface is
//! baked into the executable, the browser opens by itself, and a missing
//! instrument produces a window that says so rather than an error.
//!
//! `davinci info` checks the link, `davinci doctor` explains the whole chain,
//! `--sim` swaps the hardware for a deterministic simulator so the UI and the
//! logging path can be exercised on any machine.

mod app;
mod assets;
mod driver;
mod engine;
mod http;
mod install;

use hantek1008::protocol::Measure;
use hantek1008::{Device, Range, TimeDiv, CHANNELS};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let code = match cmd {
        // No arguments: this is the program, not a subcommand.
        "" => app::run(&[]),
        "run" | "serve" => app::run(&args[1..]),
        "doctor" => app::doctor(&args[1..]),
        "driver" => driver::cmd(&args[1..]),
        "install" => install::cmd(&args[1..]),
        "uninstall" => install::uninstall(&args[1..]),
        "info" => info(),
        "capture" => capture(&args[1..]),
        "zero" => zero(&args[1..]),
        "raw" => raw(&args[1..]),
        "dump" => dump(&args[1..]),
        #[cfg(windows)]
        "paths" => {
            for p in hantek1008::transport::winusb::device_paths() {
                println!("{p}");
            }
            0
        }
        "-h" | "--help" | "help" => {
            usage();
            0
        }
        // `davinci --sim`, `davinci --port 7500`: a bare flag is still "run",
        // not an unknown command.
        other if other.starts_with('-') && other != "-h" => app::run(&args),
        other => {
            eprintln!("неизвестная команда: {other}");
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    println!(
        "Осциллограф Хелтег да Винчи (Hantek 1008C) {version}

  davinci                         запустить программу и интерфейс
  davinci --sim                   то же на симуляторе, без прибора
  davinci doctor                  диагноз: прибор, драйвер, интерфейс, порт
  davinci driver status           что за драйвер сейчас владеет прибором
  davinci driver install          поставить WinUSB (нужен администратор)
  davinci install [--dir D]       установить программу в систему
  davinci uninstall [--purge]     удалить программу
  davinci info                    проверить связь, измерить реальную частоту
  davinci capture [N] [ch,ch]     снять N блоков и напечатать измерения
  davinci zero [N]                измерить нули АЦП (входы на землю!)
  davinci paths                   показать пути устройств WinUSB

  --port P    порт интерфейса (по умолчанию {port})
  --no-open   не открывать браузер
  --multi     разрешить второй экземпляр
",
        version = env!("CARGO_PKG_VERSION"),
        port = app::DEFAULT_PORT
    );
}

pub(crate) fn open() -> Result<Device, String> {
    let dev = open_silent()?;
    println!("прибор: {}", dev.info.link);
    Ok(dev)
}

/// Same, without the identity line — for reports that print their own layout.
pub(crate) fn open_silent() -> Result<Device, String> {
    let mut dev = Device::open().map_err(|e| {
        format!(
            "не удалось открыть прибор: {e}\n\
             Драйвер прибора должен быть WinUSB — установщик ставит его сам.\n\
             Если драйвер уже стоит, прибор мог занять другой экземпляр программы.\n\
             Проверка и починка: davinci doctor --fix"
        )
    })?;
    dev.init().map_err(|e| format!("инициализация не прошла: {e}"))?;
    Ok(dev)
}

fn info() -> i32 {
    let mut dev = match open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("инициализация: ок");

    let td = TimeDiv::DEFAULT;
    if let Err(e) = dev.set_time_div(td) {
        eprintln!("не удалось выставить время/деление: {e}");
        return 1;
    }
    if let Err(e) = dev.set_ranges(&[Range::V20; CHANNELS]) {
        eprintln!("не удалось выставить диапазон: {e}");
        return 1;
    }
    println!("развёртка: {}", td.label());
    println!("диапазон:  {}", Range::V20.label());

    let t = std::time::Instant::now();
    let info = match dev.measure_rate(10) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("захват не удался: {e}");
            return 1;
        }
    };
    let elapsed = t.elapsed().as_secs_f64();

    println!();
    println!("--- измерено на живом железе ---");
    println!("сэмплов в блоке (на канал): {}", info.block_samples);
    println!("блоков в секунду:           {:.1}", info.blocks_per_s);
    println!("частота на канал:           {:.0} Гц", info.sample_rate);
    println!(
        "поток 8 каналов (i16):      {:.2} МБ/с",
        info.sample_rate * 8.0 * 2.0 / 1e6
    );
    println!("время измерения:            {elapsed:.2} с");
    println!();

    match dev.read_block() {
        Ok(b) => {
            let dt = if info.sample_rate > 0.0 { 1.0 / info.sample_rate as f32 } else { 0.0 };
            println!("канал  Vmin      Vmax      Vpp      среднее   RMS      Гц");
            for ch in 0..CHANNELS {
                let Some(raw) = &b.ch[ch] else { continue };
                let zero = dev.calib.zero(Range::V20, ch);
                let vpc = Range::V20.volt_per_count();
                let volts: Vec<f32> = raw.iter().map(|&v| (v as f32 - zero) * vpc).collect();
                let m = Measure::of(&volts, dt);
                println!(
                    "CH{:<3} {:>8.3}  {:>8.3}  {:>7.3}  {:>8.3}  {:>7.3}  {}",
                    ch + 1,
                    m.vmin,
                    m.vmax,
                    m.vpp,
                    m.mean,
                    m.rms,
                    m.freq.map(|f| format!("{f:.0}")).unwrap_or_else(|| "-".into())
                );
            }
        }
        Err(e) => {
            eprintln!("не удалось прочитать блок: {e}");
            return 1;
        }
    }
    0
}

fn capture(args: &[String]) -> i32 {
    let count: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(20);
    let chans: Vec<usize> = match args.get(1) {
        Some(s) => s
            .split(',')
            .filter_map(|x| x.trim().parse::<usize>().ok())
            .filter(|c| *c >= 1 && *c <= CHANNELS)
            .map(|c| c - 1)
            .collect(),
        None => (0..CHANNELS).collect(),
    };
    let mut dev = match open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if let Err(e) = dev.set_channels(&chans) {
        eprintln!("{e}");
        return 1;
    }
    let info = match dev.measure_rate(5) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!(
        "каналы: {:?}, {:.0} Гц/канал, {} сэмплов/блок",
        chans.iter().map(|c| c + 1).collect::<Vec<_>>(),
        info.sample_rate,
        info.block_samples
    );
    let dt = if info.sample_rate > 0.0 { 1.0 / info.sample_rate as f32 } else { 0.0 };
    for i in 0..count {
        match dev.read_block() {
            Ok(b) => {
                let mut parts = Vec::new();
                for &c in &chans {
                    let zero = dev.calib.zero(Range::V20, c);
                    let vpc = Range::V20.volt_per_count();
                    let volts: Vec<f32> = b.ch[c]
                        .as_ref()
                        .map(|r| r.iter().map(|&v| (v as f32 - zero) * vpc).collect())
                        .unwrap_or_default();
                    let m = Measure::of(&volts, dt);
                    parts.push(format!("CH{} {:+.3}..{:.3} V", c + 1, m.vmin, m.vmax));
                }
                println!("{i:>4}  {}", parts.join("  "));
            }
            Err(e) => {
                eprintln!("блок {i}: {e}");
                return 1;
            }
        }
    }
    0
}

fn zero(args: &[String]) -> i32 {
    let blocks: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut dev = match open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("измеряю нули АЦП по трём диапазонам, по {blocks} блоков на диапазон...");
    println!("ВАЖНО: входы должны быть на земле или отключены.");
    match dev.measure_zeros(blocks) {
        Ok(calib) => {
            for range in Range::ALL {
                let z: Vec<String> = (0..CHANNELS)
                    .map(|c| format!("{:7.1}", calib.zero(range, c)))
                    .collect();
                println!("{:<7} {}", range.label(), z.join(" "));
            }
            let path = std::path::Path::new("calibration.json");
            match calib.save(path) {
                Ok(()) => println!("\nсохранено в {}", path.display()),
                Err(e) => eprintln!("\nне удалось сохранить: {e}"),
            }
            0
        }
        Err(e) => {
            eprintln!("не удалось измерить нули: {e}");
            1
        }
    }
}

pub(crate) fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

/// Send raw command frames and print exactly what comes back.
///
/// Each argument is `hex:readlen`, for example `B0:1`, `B5:1` or
/// `B9 01 BF 04 00 00:1`. A `~` prefix means "sleep 100 ms first".
fn raw(args: &[String]) -> i32 {
    let mut io = match hantek1008::transport::open_default() {
        Ok(io) => io,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("{}", io.describe());
    for spec in args {
        let (hex, len) = match spec.rsplit_once(':') {
            Some((h, l)) => (h, l.parse::<usize>().unwrap_or(1)),
            None => (spec.as_str(), 1),
        };
        if let Some(rest) = hex.strip_prefix('~') {
            std::thread::sleep(std::time::Duration::from_millis(100));
            return raw_one(&mut io, rest, len);
        }
        if raw_one(&mut io, hex, len) != 0 {
            return 1;
        }
    }
    0
}

fn dump(args: &[String]) -> i32 {
    let mut io = match hantek1008::transport::open_default() {
        Ok(io) => io,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("{}", io.describe());
    for spec in args {
        let clean: String = spec.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        let bytes: Vec<u8> = (0..clean.len() / 2)
            .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect();
        let shown: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
        print!("> {:<28}", shown.join(" "));
        std::io::Write::flush(&mut std::io::stdout()).ok();
        if let Err(e) = io.write(&bytes) {
            println!("write err: {e}");
            return 1;
        }
        match io.drain(512, 150) {
            Ok(r) => {
                let got: Vec<String> = r.iter().take(16).map(|b| format!("{b:02X}")).collect();
                let tail = if r.len() > 16 { format!(" … ({}) ", r.len()) } else { String::new() };
                println!("{:>3} Б: {}{}", r.len(), got.join(" "), tail);
            }
            Err(e) => {
                println!("drain err: {e}");
                return 1;
            }
        }
    }
    0
}

fn raw_one(io: &mut Box<dyn hantek1008::transport::Transport>, hex: &str, len: usize) -> i32 {
    let clean: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let bytes: Vec<u8> = (0..clean.len() / 2)
        .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
    let shown: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
    print!("> {} : ", shown.join(" "));
    std::io::Write::flush(&mut std::io::stdout()).ok();
    if let Err(e) = io.write(&bytes) {
        println!("write err: {e}");
        return 1;
    }
    match io.read(len) {
        Ok(r) => {
            let got: Vec<String> = r.iter().map(|b| format!("{b:02X}")).collect();
            println!("{}", got.join(" "));
            0
        }
        Err(e) => {
            println!("read err: {e}");
            1
        }
    }
}
