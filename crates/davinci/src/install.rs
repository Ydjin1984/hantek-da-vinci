//! Installing and removing the program itself.
//!
//! There is exactly one implementation of "install", and both entry points use
//! it: `davinci install` (what a person can type) and `DaVinci-Setup.exe` (which
//! only unpacks the payload and calls the first one). That keeps the shortcut
//! list, the uninstall entry and the driver step from drifting apart.
//!
//! Everything that writes outside the user profile runs elevated; the process
//! re-launches itself through `driver::elevate_and_wait`, so the UAC prompt is
//! always attached to the same program the user started.

use crate::driver;
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_KEY: &str = "DaVinciHantek1008";
const PUBLISHER: &str = "DaVinci Cyber Engineering";
const REPO_URL: &str = "https://github.com/Ydjin1984/hantek-da-vinci";

const ICON: &[u8] = include_bytes!("../assets/davinci.ico");

#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub shortcuts: bool,
    pub driver: bool,
    pub launch: bool,
    pub quiet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dir: default_dir(),
            shortcuts: true,
            driver: true,
            launch: false,
            quiet: false,
        }
    }
}

pub fn default_dir() -> PathBuf {
    let base = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    PathBuf::from(base).join("DaVinci")
}

fn start_menu_dir() -> PathBuf {
    let base = std::env::var("ProgramData")
        .unwrap_or_else(|_| r"C:\ProgramData".to_string());
    PathBuf::from(base).join(r"Microsoft\Windows\Start Menu\Programs")
}

fn public_desktop() -> PathBuf {
    let base = std::env::var("PUBLIC").unwrap_or_else(|_| r"C:\Users\Public".to_string());
    PathBuf::from(base).join("Desktop")
}

fn user_desktop() -> Option<PathBuf> {
    let base = std::env::var("USERPROFILE").ok()?;
    let p = PathBuf::from(base).join("Desktop");
    p.is_dir().then_some(p)
}

fn say(opts: &Options, text: &str) {
    if !opts.quiet {
        println!("{text}");
    }
}

/// `davinci install [--dir D] [--no-shortcuts] [--no-driver] [--launch] [--quiet]`
pub fn cmd(args: &[String]) -> i32 {
    let mut opts = Options::default();
    if let Some(dir) = crate::arg_value(args, "--dir") {
        opts.dir = PathBuf::from(dir);
    }
    opts.shortcuts = !args.iter().any(|a| a == "--no-shortcuts");
    opts.driver = !args.iter().any(|a| a == "--no-driver");
    opts.launch = args.iter().any(|a| a == "--launch");
    opts.quiet = args.iter().any(|a| a == "--quiet");

    if !driver::is_admin() {
        let mut again = vec!["install".to_string()];
        again.extend(args.iter().cloned());
        return match driver::elevate_and_wait(&again) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
    }

    match install(&opts) {
        Ok(exe) => {
            if opts.launch {
                launch_installed(&exe);
            }
            0
        }
        Err(e) => {
            eprintln!("установка не удалась: {e}");
            1
        }
    }
}

fn install(opts: &Options) -> Result<PathBuf, String> {
    println!("  {APP_NAME} {}", env!("CARGO_PKG_VERSION"));
    println!("  установка в {}\n", opts.dir.display());

    let exe = opts.dir.join("davinci.exe");
    std::fs::create_dir_all(&opts.dir).map_err(|e| format!("не удалось создать {}: {e}", opts.dir.display()))?;

    // Copy ourselves unless we already *are* the installed copy.
    let me = std::env::current_exe().map_err(|e| format!("не удалось найти себя: {e}"))?;
    let same = me
        .canonicalize()
        .ok()
        .zip(exe.canonicalize().ok())
        .map(|(a, b)| a == b)
        .unwrap_or(false);
    if !same {
        std::fs::copy(&me, &exe).map_err(|e| format!("не удалось скопировать программу: {e}"))?;
        say(opts, &format!("  программа:  {} ({} КБ)", exe.display(), size_kb(&exe)));
    } else {
        say(opts, &format!("  программа:  {} (уже на месте)", exe.display()));
    }

    let icon = opts.dir.join("davinci.ico");
    if std::fs::write(&icon, ICON).is_ok() {
        say(opts, &format!("  значок:     {}", icon.display()));
    }

    if opts.shortcuts {
        match create_shortcuts(&exe, &opts.dir, &icon) {
            Ok(paths) => {
                for p in paths {
                    say(opts, &format!("  ярлык:      {}", p.display()));
                }
            }
            Err(e) => eprintln!("  ярлыки не созданы: {e}"),
        }
    }

    write_uninstall_entry(&exe, &opts.dir)?;
    say(opts, "  удаление:   «Программы и компоненты» → Осциллограф да Винчи");

    if opts.driver {
        if driver::winusb_ready() {
            say(opts, "  драйвер:    WinUSB уже стоит — ничего не меняю");
        } else {
            let st = driver::status();
            say(opts, &format!("  драйвер:    {}", st.summary()));
            match driver::install() {
                Ok(()) => say(opts, "  драйвер:    WinUSB установлен, прибор готов"),
                Err(e) => {
                    // A missing instrument must not fail the whole install.
                    eprintln!("  драйвер не установлен: {e}");
                    eprintln!("  это не мешает установке — при подключении прибора запустите «davinci doctor»");
                }
            }
        }
    }

    println!("\nготово. запуск: {}", exe.display());
    Ok(exe)
}

fn size_kb(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len() / 1024).unwrap_or(0)
}

/// Launch the installed copy in its own console window.
///
/// Spawning it directly would attach the program to the caller's console, and
/// that console disappears the moment the installer exits — taking the program
/// with it. Going through `start` gives it a window of its own.
fn launch_installed(exe: &Path) {
    use std::os::windows::process::CommandExt;
    // `raw_arg`: the command line is written by us, not re-quoted by the Rust
    // runtime, whose C-style escaping cmd.exe does not understand.
    let _ = Command::new("cmd")
        .raw_arg("/c")
        .raw_arg(format!("start \"\" \"{}\"", exe.display()))
        .spawn();
}

/// Shortcuts through the shell's own COM object.
///
/// A `.lnk` is a binary shell item; writing one by hand is a bad trade for a few
/// lines of PowerShell that Windows ships anyway.
fn create_shortcuts(exe: &Path, dir: &Path, icon: &Path) -> Result<Vec<PathBuf>, String> {
    let mut targets: Vec<PathBuf> = vec![start_menu_dir().join("Осциллограф да Винчи.lnk")];
    // Only the per-user desktop: the public one is merged into the same view, so
    // writing both puts two identical icons in front of the user.
    if let Some(desktop) = user_desktop() {
        targets.push(desktop.join("Осциллограф да Винчи.lnk"));
    }

    let list = targets
        .iter()
        .map(|t| format!("'{}'", t.display()))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$target = '{exe}'
$workdir = '{dir}'
$icon = '{icon}'
$ws = New-Object -ComObject WScript.Shell
foreach ($p in @({list})) {{
  $parent = Split-Path -Parent $p
  if (-not (Test-Path $parent)) {{ continue }}
  $s = $ws.CreateShortcut($p)
  $s.TargetPath = $target
  $s.WorkingDirectory = $workdir
  $s.IconLocation = "$icon,0"
  $s.Description = '{desc}'
  $s.WindowStyle = 1
  $s.Save()
}}
"#,
        exe = exe.display(),
        dir = dir.display(),
        icon = icon.display(),
        list = list,
        desc = APP_NAME,
    );

    // Windows PowerShell reads a script file as ANSI unless it starts with a
    // BOM, which turned every Cyrillic shortcut name into mojibake. Passing the
    // script as `-EncodedCommand` (UTF-16LE) sidesteps the encoding question
    // entirely and leaves no temp file behind.
    let encoded = encode_utf16_base64(&script);
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ])
        .output()
        .map_err(|e| format!("PowerShell не запустился: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() { "PowerShell вернул ошибку без текста".into() } else { err });
    }
    Ok(targets.into_iter().filter(|t| t.exists()).collect())
}

/// Base64 of UTF-16LE — the encoding `powershell -EncodedCommand` expects.
fn encode_utf16_base64(text: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn write_uninstall_entry(exe: &Path, dir: &Path) -> Result<(), String> {
    let key = format!(r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{APP_KEY}");
    let icon = dir.join("davinci.ico");
    let size = std::fs::metadata(exe).map(|m| m.len() / 1024).unwrap_or(0).to_string();
    let pairs: [(&str, &str, &str); 10] = [
        ("DisplayName", "REG_SZ", "Осциллограф Хелтег да Винчи"),
        ("DisplayVersion", "REG_SZ", env!("CARGO_PKG_VERSION")),
        ("Publisher", "REG_SZ", PUBLISHER),
        ("URLInfoAbout", "REG_SZ", REPO_URL),
        ("InstallLocation", "REG_SZ", &dir.to_string_lossy()),
        ("DisplayIcon", "REG_SZ", &icon.to_string_lossy()),
        ("UninstallString", "REG_SZ", &format!("\"{}\" uninstall", exe.display())),
        ("QuietUninstallString", "REG_SZ", &format!("\"{}\" uninstall --quiet", exe.display())),
        ("EstimatedSize", "REG_DWORD", &size),
        ("NoModify", "REG_DWORD", "1"),
    ];
    for (name, kind, value) in pairs {
        let out = Command::new("reg")
            .args(["add", &key, "/v", name, "/t", kind, "/d", value, "/f"])
            .output()
            .map_err(|e| format!("reg add не запустился: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "не удалось записать запись об удалении ({name}): {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ));
        }
    }
    Ok(())
}

/// `davinci uninstall [--purge] [--quiet]`
pub fn uninstall(args: &[String]) -> i32 {
    let purge = args.iter().any(|a| a == "--purge");
    let quiet = args.iter().any(|a| a == "--quiet");

    if !driver::is_admin() {
        let mut again = vec!["uninstall".to_string()];
        again.extend(args.iter().cloned());
        return match driver::elevate_and_wait(&again) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
    }

    let exe = std::env::current_exe().unwrap_or_else(|_| default_dir().join("davinci.exe"));
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_else(default_dir);

    // Deleting "the directory I happen to live in" is too sharp a tool: a copy
    // run straight out of `target\release` would take the build tree with it.
    // Only a folder that is the installation (or the registered one) qualifies.
    let named_like_install = dir
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case("DaVinci"))
        .unwrap_or(false);
    let key = format!(r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{APP_KEY}");
    let registered = Command::new("reg")
        .args(["query", &key, "/v", "InstallLocation"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&*dir.to_string_lossy()))
        .unwrap_or(false);
    if !named_like_install && !registered {
        eprintln!(
            "  это не установленная копия: {}\n  удаление из такой папки не выполняю.",
            dir.display()
        );
        return 2;
    }

    if !quiet {
        println!("  удаление из {}\n", dir.display());
    }

    let key = format!(r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{APP_KEY}");
    let _ = Command::new("reg").args(["delete", &key, "/f"]).output();

    let mut shortcuts = vec![start_menu_dir().join("Осциллограф да Винчи.lnk"), public_desktop().join("Осциллограф да Винчи.lnk")];
    if let Some(d) = user_desktop() {
        shortcuts.push(d.join("Осциллограф да Винчи.lnk"));
    }
    for s in &shortcuts {
        if s.exists() && std::fs::remove_file(s).is_ok() && !quiet {
            println!("  убран ярлык: {}", s.display());
        }
    }

    if purge {
        let state = crate::app::state_dir();
        let _ = std::fs::remove_dir_all(&state);
        if !quiet {
            println!("  удалены журналы и калибровка: {}", state.display());
        }
    }

    if !quiet {
        println!("\n  драйвер WinUSB оставлен: он принадлежит системе, а не программе.");
        println!("  снять его вручную: pnputil /delete-driver oem184.inf /uninstall /force");
    }

    // We are executing from inside the directory we need to delete, so let a
    // detached shell do it after this process is gone.
    use std::os::windows::process::CommandExt;
    let _ = Command::new("cmd")
        .raw_arg("/c")
        .raw_arg(format!(
            "ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"",
            dir.display()
        ))
        .spawn();

    0
}

use crate::app::APP_NAME;
