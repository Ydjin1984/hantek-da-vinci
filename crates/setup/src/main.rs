//! Установщик «Осциллограф Хелтег да Винчи».
//!
//! Он делает ровно две вещи, и обе намеренно скучные: распаковывает программу
//! и передаёт установку ей самой.
//!
//! Почему так, а не «всё в установщике»: ярлыки, запись об удалении и драйвер
//! уже умеет `davinci install`. Второй экземпляр этой логики в установщике
//! неизбежно разошёлся бы с первым, и разошёлся бы молча. Здесь же любая
//! установка — из мастера, из консоли или повторная — идёт одним и тем же путём.
//!
//! Права администратора запрашивает сама программа (один UAC на всё): мастер
//! работает от обычного пользователя, поэтому может показать окно и объяснить,
//! что происходит, до того как система спросит пароль.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Вложенная программа, собранная `tools/build-dist.ps1`.
const PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.exe"));

const APP_NAME: &str = "Осциллограф Хелтег да Винчи";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let silent = has(&args, "--silent") || has(&args, "--quiet");
    let keep = has(&args, "--keep-temp");

    if has(&args, "-h") || has(&args, "--help") {
        usage();
        return;
    }

    println!();
    println!("  {APP_NAME}");
    println!("  установка, версия {VERSION}");
    println!("  ─────────────────────────────────────────────");

    if PAYLOAD.len() < 100_000 {
        eprintln!("\n  В установщик не вложена программа.");
        eprintln!("  Соберите её и установщик заново:");
        eprintln!("      pwsh tools\\build-dist.ps1");
        fail(silent);
        return;
    }

    let dir = arg_value(&args, "--dir").map(PathBuf::from).unwrap_or_else(default_dir);
    println!("\n  программа : {} КБ, вложена в этот файл", PAYLOAD.len() / 1024);
    println!("  папка     : {}", dir.display());
    println!("  ярлыки    : меню «Пуск» и рабочий стол");
    if !has(&args, "--no-driver") {
        println!("  драйвер   : WinUSB для прибора (если ещё не стоит)");
    }
    println!();
    println!("  Дальше система спросит разрешение администратора —");
    println!("  это нужно, чтобы записать программу в Program Files и поставить драйвер.");
    println!();

    if !silent && !confirm("  Установить? [Y/n]: ") {
        println!("  отменено.");
        return;
    }

    // Payload goes to a scratch directory: the file we are running from is the
    // installer, and it must not overwrite itself while it is still executing.
    let temp = std::env::temp_dir().join(format!("davinci-setup-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&temp) {
        eprintln!("  не удалось создать временную папку: {e}");
        fail(silent);
        return;
    }
    let exe = temp.join("davinci.exe");
    if let Err(e) = std::fs::write(&exe, PAYLOAD) {
        eprintln!("  не удалось распаковать программу: {e}");
        fail(silent);
        return;
    }

    let mut cmd = Command::new(&exe);
    cmd.arg("install").arg("--dir").arg(&dir).arg("--launch");
    if has(&args, "--no-driver") {
        cmd.arg("--no-driver");
    }
    if has(&args, "--no-shortcuts") {
        cmd.arg("--no-shortcuts");
    }
    if silent {
        cmd.arg("--quiet");
    }

    println!();
    let status = cmd.status();

    if !keep {
        let _ = std::fs::remove_dir_all(&temp);
    }

    match status {
        Ok(s) if s.success() => {
            println!("\n  Готово.");
            println!("  Запуск: «Пуск» → {APP_NAME}, или {}", dir.join("davinci.exe").display());
            println!("  Если прибор не подключён — программа всё равно откроется");
            println!("  и покажет, чего не хватает. Диагностика: davinci doctor");
        }
        Ok(s) => {
            eprintln!("\n  Установка завершилась с кодом {}.", s.code().unwrap_or(-1));
            eprintln!("  Подробности — в сообщениях выше.");
            fail(silent);
        }
        Err(e) => {
            eprintln!("\n  Не удалось запустить программу установки: {e}");
            fail(silent);
        }
    }
}

fn has(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

fn default_dir() -> PathBuf {
    let base = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    PathBuf::from(base).join("DaVinci")
}

fn confirm(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return true;
    }
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes" | "д" | "да")
}

/// Keep the console open on failure: this window is the only place the reason
/// is written, and a double-clicked installer closes it instantly otherwise.
fn fail(silent: bool) {
    if silent {
        return;
    }
    if std::io::stdin().is_terminal() {
        print!("\n  Нажмите Enter, чтобы закрыть…");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}

fn usage() {
    println!(
        "DaVinci-Setup — установщик «{APP_NAME}» {VERSION}

  DaVinci-Setup.exe                     обычная установка с подтверждениями
  DaVinci-Setup.exe --silent            без вопросов
  DaVinci-Setup.exe --dir \"D:\\Tools\\DaVinci\"
                                        поставить в другую папку
  DaVinci-Setup.exe --no-driver         не трогать драйверы
  DaVinci-Setup.exe --no-shortcuts      без ярлыков
  DaVinci-Setup.exe --keep-temp         не удалять распакованное (для отладки)
"
    );
}

#[allow(dead_code)]
fn exists(path: &Path) -> bool {
    path.exists()
}
