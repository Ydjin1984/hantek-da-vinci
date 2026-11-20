//! Command line entry point.
//!
//! `davinci info` checks the link, `davinci serve` runs the daemon and the UI,
//! `--sim` swaps the hardware for a deterministic simulator so the UI and the
//! logging path can be exercised on any machine.

mod engine;
mod http;

use engine::Engine;
use hantek1008::protocol::Measure;
use hantek1008::sim::SimTransport;
use hantek1008::{Device, Range, TimeDiv, CHANNELS};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("info");
    let code = match cmd {
        "info" => info(),
        "capture" => capture(&args[1..]),
        "zero" => zero(&args[1..]),
        "serve" => serve(&args[1..]),
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
        "davinci — осциллограф Хелтег да Винчи (Hantek 1008C)

  davinci info                    проверить связь, измерить реальную частоту
  davinci capture [N] [ch,ch]     снять N блоков и напечатать измерения
  davinci zero [N]                измерить нули АЦП (входы на землю!)
  davinci serve [--sim] [--port P] [--ui DIR]
                                  запустить демон и веб-интерфейс
  davinci paths                   показать пути устройств WinUSB
"
    );
}

fn open() -> Result<Device, String> {
    let mut dev = Device::open().map_err(|e| {
        format!(
            "не удалось открыть прибор: {e}\n\
             Прибор должен быть привязан к WinUSB (Zadig → Hantek 1008C → WinUSB).\n\
             Вендорский драйвер Hantek для нашей программы не подходит."
        )
    })?;
    println!("прибор: {}", dev.info.link);
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

fn serve(args: &[String]) -> i32 {
    let sim = args.iter().any(|a| a == "--sim");
    let port: u16 = arg_value(args, "--port").and_then(|v| v.parse().ok()).unwrap_or(7411);
    let ui = arg_value(args, "--ui").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("ui/dist"));

    let mut dev = if sim {
        println!("режим симулятора: железо не используется");
        // The simulator has a fixed time base; let the engine trust it.
        std::env::set_var("DAVINCI_FIXED_RATE", "50000");
        let mut d = Device::with_transport(Box::new(SimTransport::new()));
        d.set_channels(&(0..CHANNELS).collect::<Vec<_>>()).unwrap();
        d.info.link = "simulator".into();
        d
    } else {
        match open() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{e}");
                eprintln!("подсказка: запустите с --sim, чтобы посмотреть интерфейс без прибора");
                return 1;
            }
        }
    };

    if !sim {
        match dev.measure_rate(8) {
            Ok(i) => println!(
                "частота: {:.0} Гц/канал, {} сэмплов в блоке, {:.0} блоков/с",
                i.sample_rate, i.block_samples, i.blocks_per_s
            ),
            Err(e) => {
                eprintln!("не удалось измерить частоту: {e}");
                return 1;
            }
        }
    } else {
        // The simulator answers with 2000 samples per channel per block.
        dev.info.sample_rate = 50_000.0;
        dev.info.block_samples = 500;
        dev.info.blocks_per_s = 100.0;
    }

    let calib_path = PathBuf::from("calibration.json");
    dev.calib = hantek1008::Calibration::load(&calib_path);
    if dev.calib.measured_at.is_some() {
        println!("калибровка нулей загружена из {}", calib_path.display());
    }

    let eng = Engine::new(&dev);
    let handle = eng.handle();
    std::thread::spawn(move || eng.run(dev));
    http::serve(port, ui, handle)
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}
