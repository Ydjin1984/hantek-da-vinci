//! Embeds the already-built `davinci.exe` so `DaVinci-Setup.exe` is a single
//! file the user can carry on a USB stick.
//!
//! The payload is read from the workspace release directory instead of being
//! rebuilt here: a nested `cargo` call inside a build script risks deadlocking
//! on the target lock, and the ordering is already expressed by
//! `tools/build-dist.ps1`.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let target = out_dir.join("payload.exe");

    icon(&manifest);

    let root = manifest.join("../..");
    let candidates = [
        root.join("target/release/davinci.exe"),
        root.join("target/x86_64-pc-windows-msvc/release/davinci.exe"),
    ];

    println!("cargo:rerun-if-changed={}", candidates[0].display());
    println!("cargo:rerun-if-changed={}", candidates[1].display());

    let mut written = false;
    for path in &candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if bytes.len() > 100_000 {
                std::fs::write(&target, &bytes).expect("write payload");
                println!(
                    "cargo:warning=payload: {} ({} КБ)",
                    path.display(),
                    bytes.len() / 1024
                );
                written = true;
                break;
            }
        }
    }
    if !written {
        // A 4-byte placeholder keeps the build green; the installer refuses to
        // run with it and says exactly what to build first.
        std::fs::write(&target, b"\0\0\0\0").expect("write placeholder payload");
        println!("cargo:warning=НЕТ ПРОГРАММЫ: сначала cargo build --release -p davinci");
    }
}

/// The icon and the version strings, straight from the program's own artwork.
fn icon(manifest: &std::path::Path) {
    let ico = manifest.join("../../crates/davinci/assets/davinci.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    if !ico.exists() {
        println!("cargo:warning=нет {}: установщик останется без иконки", ico.display());
        return;
    }
    #[cfg(windows)]
    {
        let mut res = tauri_winres::WindowsResource::new();
        res.set_icon(ico.to_string_lossy().as_ref());
        res.set("FileDescription", "Установщик «Осциллограф Хелтег да Винчи»");
        res.set("ProductName", "DaVinci Oscilloscope");
        res.set("CompanyName", "DaVinci Cyber Engineering");
        res.set("LegalCopyright", "MIT");
        res.set("OriginalFilename", "DaVinci-Setup.exe");
        match res.compile() {
            Ok(()) => {}
            Err(e) => println!("cargo:warning=ресурсы установщика не собраны: {e}"),
        }
    }
}
