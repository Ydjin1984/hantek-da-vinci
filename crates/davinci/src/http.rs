//! HTTP surface for the browser UI.
//!
//! Deliberately small: static files, one JSON status endpoint, one binary frame
//! endpoint and a command endpoint. The browser polls a frame at ~30 Hz, which
//! is fast enough on loopback and removes any long-lived socket state.

use crate::engine::EngineHandle;
use hantek1008::{Range, TimeDiv};
use std::path::{Path, PathBuf};
use tiny_http::{Header, Method, Request, Response, Server};

/// Where the interface comes from.
///
/// The shipped program carries it inside the executable; `--ui <dir>` still
/// works for development, where a Vite rebuild should be visible on refresh.
pub enum Assets {
    Embedded,
    Dir(PathBuf),
}

pub fn serve(port: u16, assets: Assets, eng: EngineHandle) -> ! {
    let addr = format!("127.0.0.1:{port}");
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("не удалось занять {addr}: {e}");
            std::process::exit(1);
        }
    };
    for request in server.incoming_requests() {
        if let Err(e) = handle(request, &assets, &eng) {
            eprintln!("http: {e}");
        }
    }
    unreachable!()
}

fn handle(request: Request, assets: &Assets, eng: &EngineHandle) -> std::io::Result<()> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };
    let get = |key: &str| -> Option<String> {
        query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == key {
                Some(v.to_string())
            } else {
                None
            }
        })
    };

    if request.method() != &Method::Get {
        return request.respond(Response::empty(405));
    }

    match path.as_str() {
        "/api/info" => {
            let st = eng.status();
            let body = serde_json::json!({
                "running": st.running,
                "link": st.link,
                "sample_rate": st.sample_rate,
                "block_samples": st.block_samples,
                "blocks_per_s": st.blocks_per_s,
                "time_div": st.time_div,
                "time_div_id": st.time_div_id,
                "window_s": st.window_s,
                "error": st.error,
                "log": {
                    "path": st.log_path,
                    "bytes": st.log_bytes,
                    "seconds": st.log_seconds,
                },
                "blocks_total": st.blocks_total,
            });
            json(request, &body.to_string())
        }
        "/api/channels" => {
            let body = serde_json::Value::Array(eng.channels());
            json(request, &body.to_string())
        }
        "/api/frame" => {
            let width: usize = get("width").and_then(|v| v.parse().ok()).unwrap_or(1600);
            let span: f64 = get("span").and_then(|v| v.parse().ok()).unwrap_or(0.2);
            let frame = eng.frame(width, span);
            let bytes = frame.to_bytes(&[]);
            let mut resp = Response::from_data(bytes);
            resp.add_header(header("Content-Type", "application/octet-stream"));
            resp.add_header(header("Cache-Control", "no-store"));
            request.respond(resp)
        }
        "/api/export.csv" => {
            let body = eng.export_csv();
            let name = format!("attachment; filename=\"davinci-{}.csv\"", crate::engine::now_unix());
            let mut resp = Response::from_data(body.into_bytes());
            resp.add_header(header("Content-Type", "text/csv; charset=utf-8"));
            resp.add_header(header("Content-Disposition", &name));
            resp.add_header(header("Cache-Control", "no-store"));
            request.respond(resp)
        }
        "/api/control" => {
            let cmd = get("cmd").unwrap_or_default();
            match cmd.as_str() {
                "run" => eng.set_running(true),
                "stop" => eng.set_running(false),
                "reset" => eng.reset_peaks(),
                "channels" => {
                    let list: Vec<usize> = get("list")
                        .unwrap_or_default()
                        .split(',')
                        .filter_map(|s| s.parse::<usize>().ok())
                        .filter(|c| *c >= 1 && *c <= 8)
                        .map(|c| c - 1)
                        .collect();
                    eng.set_channels(list);
                }
                "time_div" => {
                    if let Some(id) = get("id").and_then(|v| v.parse::<u8>().ok()) {
                        if let Ok(td) = TimeDiv::from_id(id) {
                            eng.set_time_div(td);
                        }
                    }
                }
                "autoscale" => {
                    let on = get("on").map(|v| v == "1" || v == "true").unwrap_or(false);
                    let ch = get("ch")
                        .and_then(|v| v.parse::<usize>().ok())
                        .map(|c| c.saturating_sub(1));
                    eng.set_autoscale(ch, on);
                }
                "range" => {
                    if let (Some(ch), Some(r)) = (get("ch"), get("r")) {
                        if let (Ok(ch), Ok(r)) = (ch.parse::<usize>(), r.parse::<usize>()) {
                            eng.set_range(ch.saturating_sub(1), Range::from_index(r.saturating_sub(1)));
                        }
                    }
                }
                "range_all" => {
                    if let Some(r) = get("r").and_then(|v| v.parse::<usize>().ok()) {
                        eng.set_range_all(Range::from_index(r.saturating_sub(1)));
                        let _ = TimeDiv::DEFAULT;
                    }
                }
                "log_start" => {
                    let dir = std::env::var("DAVINCI_LOGS").unwrap_or_else(|_| "logs".into());
                    let name = format!("davinci-{}.dav", crate::engine::now_unix());
                    let path = PathBuf::from(dir).join(name);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = eng.start_log(path) {
                        return json(request, &format!("{{\"ok\":false,\"error\":\"{e}\"}}"));
                    }
                }
                "log_stop" => eng.stop_log(),
                other => {
                    return json(request, &format!("{{\"ok\":false,\"error\":\"unknown cmd {other}\"}}"));
                }
            }
            json(request, "{\"ok\":true}")
        }
        _ => serve_static(request, assets, &path),
    }
}

fn serve_static(request: Request, assets: &Assets, path: &str) -> std::io::Result<()> {
    let rel = if path == "/" || path.is_empty() { "index.html" } else { path.trim_start_matches('/') };
    if rel.contains("..") {
        return request.respond(Response::empty(403));
    }

    if let Assets::Embedded = assets {
        return match crate::assets::lookup(rel) {
            Some((body, content_type)) => {
                let mut resp = Response::from_data(body.to_vec());
                resp.add_header(header("Content-Type", content_type));
                resp.add_header(header("Cache-Control", "no-cache"));
                request.respond(resp)
            }
            None => request.respond(Response::empty(404)),
        };
    }

    let Assets::Dir(ui_dir) = assets else { unreachable!() };
    let file = ui_dir.join(rel);
    let file = if file.exists() {
        file
    } else {
        // Single-page app: unknown routes fall back to the shell.
        ui_dir.join("index.html")
    };
    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut resp = Response::from_data(bytes);
            resp.add_header(header("Content-Type", mime(&file)));
            resp.add_header(header("Cache-Control", "no-cache"));
            request.respond(resp)
        }
        Err(_) => request.respond(Response::empty(404)),
    }
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

fn json(request: Request, body: &str) -> std::io::Result<()> {
    let mut resp = Response::from_string(body);
    resp.add_header(header("Content-Type", "application/json; charset=utf-8"));
    resp.add_header(header("Cache-Control", "no-store"));
    request.respond(resp)
}
