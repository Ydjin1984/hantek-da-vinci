//! The user interface, compiled into the executable.
//!
//! `build.rs` turns `ui/dist` into a table of `(name, bytes, mime)`; this module
//! is the only place that knows about it. Serving is a slice copy, so a page
//! load costs no disk access at all — which matters when the program lives on a
//! USB stick or inside `Program Files` with an antivirus watching reads.

include!(concat!(env!("OUT_DIR"), "/assets_generated.rs"));

/// Case-sensitive lookup with the single-page-app fallback.
pub fn lookup(path: &str) -> Option<(&'static [u8], &'static str)> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    if let Some(hit) = FILES.iter().find(|(name, _, _)| *name == rel) {
        return Some((hit.1, hit.2));
    }
    // Unknown routes without an extension are app routes, not missing files.
    if !rel.rsplit('/').next().unwrap_or("").contains('.') {
        if let Some(hit) = FILES.iter().find(|(name, _, _)| *name == "index.html") {
            return Some((hit.1, hit.2));
        }
    }
    None
}

/// How many files were baked in — shown by `davinci doctor`.
pub fn count() -> usize {
    FILES.len()
}

/// Total size of the baked interface, for the same report.
pub fn bytes() -> usize {
    FILES.iter().map(|(_, body, _)| body.len()).sum()
}
