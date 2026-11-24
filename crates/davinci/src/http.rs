//! HTTP surface for the browser UI.
//!
//! Deliberately small: static files, a status endpoint, one binary frame endpoint
//! kept for older clients, the v2 capture endpoint that carries metadata and the
//! frame of one revision together, and a handful of commands. The UI polls, which
//! is fast enough on loopback and removes any long-lived socket state.
//!
//! Two rules shape the command handlers:
//!
//! * A bad parameter is a 4xx with a reason, never a silent clamp. The UI shows
//!   what the server confirmed, so a clamp would make the screen disagree with
//!   the instrument.
//! * A frame is addressed by `frame_id`. Re-exporting what is on screen must not
//!   depend on which sweep arrived meanwhile.

use crate::capture::base64;
use crate::engine::EngineHandle;
use crate::trigger::{self, Config as TriggerConfig};
use hantek1008::{Range, TimeDiv};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

/// Where the interface comes from.
///
/// The shipped program carries it inside the executable; `--ui <dir>` still
/// works for development, where a Vite rebuild should be visible on refresh.
pub enum Assets {
    Embedded,
    Dir(PathBuf),
}

/// Bind the interface and serve it from a thread of its own.
///
/// Binding happens here, on the caller's thread, so that a caller about to point
/// a window at this URL knows the socket is already accepting — which is what
/// lets the program show its window immediately instead of racing itself.
pub fn start(
    port: u16,
    assets: Assets,
    eng: EngineHandle,
    ui_beat: Arc<AtomicU64>,
) -> Result<(), String> {
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| format!("не удалось занять {addr}: {e}"))?;
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            // A page that is still alive keeps making requests. The window's
            // native watchdog watches this counter: a frozen renderer stops it,
            // and only a reload brings the picture back.
            ui_beat.fetch_add(1, Ordering::Relaxed);
            if let Err(e) = handle(request, &assets, &eng) {
                eprintln!("http: {e}");
            }
        }
    });
    Ok(())
}

/// Serve on this thread and never return — the shape used when there is no
/// window (`--no-open`, `--browser`, or a system without one).
pub fn serve(port: u16, assets: Assets, eng: EngineHandle) -> ! {
    if let Err(e) = start(port, assets, eng, Arc::new(AtomicU64::new(0))) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    loop {
        std::thread::park();
    }
}

fn handle(mut request: Request, assets: &Assets, eng: &EngineHandle) -> std::io::Result<()> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    let is_post = request.method() == &Method::Post;
    let writable = matches!(
        path.as_str(),
        "/api/trigger" | "/api/trigger/arm" | "/api/hold" | "/api/control" | "/api/link/reconnect"
    );
    if is_post && !writable {
        return request.respond(Response::empty(405));
    }
    if !is_post && request.method() != &Method::Get {
        return request.respond(Response::empty(405));
    }

    // A POST may carry the same parameters in its body, either as JSON or as a
    // form-encoded string; commands that predate JSON keep working unchanged.
    let body = if is_post {
        let mut text = String::new();
        request.as_reader().read_to_string(&mut text).ok();
        text
    } else {
        String::new()
    };
    let params = if is_post && !body.trim_start().starts_with('{') {
        if body.trim().is_empty() {
            query.clone()
        } else {
            body.trim().to_string()
        }
    } else {
        query.clone()
    };
    let get = |key: &str| -> Option<String> {
        params.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == key {
                Some(urldecode(v))
            } else {
                None
            }
        })
    };

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
                "window_clamped": st.window_clamped,
                "error": st.error,
                "state": st.state,
                "frame_id": st.frame_id,
                "held": st.held,
                "rec": st.rec,
                "trigger": serde_json::to_value(&st.trigger).unwrap_or(serde_json::Value::Null),
                "source_span": st.source_span.map(|(lo, hi)| serde_json::json!([lo, hi])),
                "level_rescue": st.level_rescue.map(|r| {
                    serde_json::json!({
                        "min_v": r.min_v,
                        "max_v": r.max_v,
                        "level_v": r.level_v,
                        "age_ms": r.age_ms,
                    })
                }),
                "log": {
                    "path": st.log_path,
                    "bytes": st.log_bytes,
                    "seconds": st.log_seconds,
                    "error": st.log_error,
                },
                "blocks_total": st.blocks_total,
                // Link health, in the same answer as the numbers: the UI can
                // tell "Single holds a capture" from "the instrument stopped
                // answering" without a second request.
                "link_state": st.link_state,
                "frame_age_ms": st.frame_age_ms,
                "last_block_age_ms": st.last_block_age_ms,
                "consecutive_errors": st.consecutive_errors,
                "reopen_attempts": st.reopen_attempts,
            });
            json(request, &body.to_string())
        }
        "/api/channels" => {
            let body = serde_json::Value::Array(eng.channels());
            json(request, &body.to_string())
        }

        // The original endpoint: one revision, no metadata, `DVF1` on the wire.
        // Kept because an older interface (or a script someone wrote) must keep
        // working after the upgrade.
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

        // Metadata and the frame it describes, in one answer: a client that gets
        // this cannot show numbers from one sweep next to a trace from another.
        "/api/capture/v2" => {
            let width = match get("width") {
                Some(v) => match v.parse::<usize>() {
                    Ok(w) if (16..=8192).contains(&w) => w,
                    _ => return bad(request, "ширина кадра должна быть числом 16..8192"),
                },
                None => 1200,
            };
            let frame_id = match get("frame_id") {
                Some(v) => match v.parse::<u64>() {
                    Ok(0) | Err(_) => return bad(request, "frame_id должен быть положительным целым"),
                    Ok(id) => Some(id),
                },
                None => None,
            };
            let Some(c) = eng.capture(frame_id) else {
                let msg = match frame_id {
                    Some(id) => format!("кадр {id} уже недоступен"),
                    None => "кадров пока нет".to_string(),
                };
                return json_code(request, 404, &format!("{{\"ok\":false,\"error\":\"{msg}\"}}"));
            };
            let cols = c.snapshot.columns_i16(width);
            let mut meta = c.snapshot.meta_json(width, c.held, c.state, c.running);
            meta["columns_b64"] = serde_json::Value::String(base64(&cols));
            meta["columns_bytes"] = serde_json::Value::from(cols.len());
            json(request, &meta.to_string())
        }

        // CSV of exactly one frame — the one on screen, or a named one.
        "/api/frame.csv" => {
            let frame_id = match get("frame_id") {
                Some(v) => match v.parse::<u64>() {
                    Ok(id) => Some(id),
                    Err(_) => return bad(request, "frame_id должен быть целым числом"),
                },
                None => None,
            };
            let Some(csv) = eng.frame_csv(frame_id) else {
                return json_code(
                    request,
                    404,
                    "{\"ok\":false,\"error\":\"этот кадр уже недоступен\"}",
                );
            };
            let name = format!(
                "attachment; filename=\"davinci-frame-{}.csv\"",
                frame_id.unwrap_or_default()
            );
            let mut resp = Response::from_data(csv.into_bytes());
            resp.add_header(header("Content-Type", "text/csv; charset=utf-8"));
            resp.add_header(header("Content-Disposition", &name));
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

        "/api/trigger" => {
            let base = eng.status().trigger;
            let cfg = if is_post && body.trim_start().starts_with('{') {
                parse_trigger_json(&body, &base)
            } else {
                parse_trigger_query(&get, &base)
            };
            let cfg = match cfg {
                Ok(c) => c,
                Err(e) => return bad(request, &e),
            };
            match eng.set_trigger(cfg.clone()) {
                Ok(()) => json(
                    request,
                    &serde_json::json!({"ok": true, "trigger": cfg}).to_string(),
                ),
                Err(e) => bad(request, &e),
            }
        }

        "/api/trigger/arm" => {
            eng.arm();
            let st = eng.status();
            json(
                request,
                &serde_json::json!({"ok": true, "state": st.state, "trigger": st.trigger}).to_string(),
            )
        }

        "/api/hold" => {
            let on = hold_flag(&body, &get);
            eng.set_hold(on);
            let st = eng.status();
            json(
                request,
                &serde_json::json!({"ok": true, "held": st.held, "frame_id": st.frame_id}).to_string(),
            )
        }

        // Drop the USB handle and open the instrument again. For a handle that
        // stopped answering, and for the "Переподключить" button.
        "/api/link/reconnect" => {
            eng.reconnect();
            let st = eng.status();
            json(
                request,
                &serde_json::json!({
                    "ok": true,
                    "link_state": st.link_state,
                    "reopen_attempts": st.reopen_attempts,
                })
                .to_string(),
            )
        }

        "/api/control" => control(request, eng, &get, is_post),

        _ => serve_static(request, assets, &path),
    }
}

/// The command endpoint the UI has always used, plus the newer settings.
fn control(
    request: Request,
    eng: &EngineHandle,
    get: &dyn Fn(&str) -> Option<String>,
    is_post: bool,
) -> std::io::Result<()> {
    let Some(cmd) = get("cmd") else {
        return bad(request, "не указана команда (cmd)");
    };
    match cmd.as_str() {
        "run" => {
            let on = get("on").map(|v| v != "0" && v != "false").unwrap_or(true);
            eng.set_running(on);
        }
        "peaks" => eng.reset_peaks(),
        "channels" => {
            let list: Vec<usize> = get("list")
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse::<usize>().ok())
                .filter(|c| *c >= 1 && *c <= 8)
                .map(|c| c - 1)
                .collect();
            if list.is_empty() {
                return bad(request, "список каналов пуст или вне 1..8");
            }
            eng.set_channels(list);
        }
        "time_div" => {
            let Some(id) = get("id").and_then(|v| v.parse::<u8>().ok()) else {
                return bad(request, "id развёртки должен быть числом");
            };
            match TimeDiv::from_id(id) {
                Ok(td) => eng.set_time_div(td),
                Err(_) => return bad(request, &format!("нет развёртки с id {id}")),
            }
        }
        "autoscale" => {
            let on = get("on").map(|v| v == "1" || v == "true").unwrap_or(false);
            let ch = get("ch").and_then(|v| v.parse::<usize>().ok()).map(|c| c.saturating_sub(1));
            eng.set_autoscale(ch, on);
        }
        "range_v" => {
            let ch = get("ch").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let Some(v) = get("v").and_then(|v| v.parse::<f32>().ok()) else {
                return bad(request, "напряжение должно быть числом");
            };
            if !v.is_finite() || !(0.05..=20.0).contains(&v) {
                return bad(request, "полная шкала должна быть 0,05..20 В");
            }
            if ch < 1 || ch > 8 {
                return bad(request, "канал должен быть 1..8");
            }
            eng.set_range_v(ch - 1, v);
        }
        "range" => {
            let r = get("id").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
            if r < 1 || r > Range::ALL.len() {
                return bad(request, "предел должен быть 1..3");
            }
            eng.set_range_all(Range::from_index(r - 1));
        }
        "center" => {
            let ch = get("ch").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
            let Some(v) = get("v").and_then(|v| v.parse::<f32>().ok()) else {
                return bad(request, "центр должен быть числом");
            };
            if ch < 1 || ch > 8 {
                return bad(request, "канал должен быть 1..8");
            }
            match eng.set_center(ch - 1, v) {
                Ok(()) => {}
                Err(e) => return bad(request, &e),
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
                return bad(request, &format!("не удалось начать запись: {e}"));
            }
        }
        "log_stop" => eng.stop_log(),
        other => return bad(request, &format!("неизвестная команда {other}")),
    }
    let _ = is_post;
    let st = eng.status();
    json(
        request,
        &serde_json::json!({
            "ok": true,
            "running": st.running,
            "state": st.state,
            "held": st.held,
            "rec": st.rec,
            "time_div_id": st.time_div_id,
            "time_div": st.time_div,
            "window_s": st.window_s,
            "trigger": st.trigger,
            "frame_id": st.frame_id,
        })
        .to_string(),
    )
}

/// "Удержать экран" takes a boolean, from JSON or from the query string.
///
/// This exists as a named function because getting it wrong is invisible: a JSON
/// body `{"on":false}` that fell through to the query parser used to mean "on",
/// so the button could hold the screen but never release it.
fn hold_flag(body: &str, get: &dyn Fn(&str) -> Option<String>) -> bool {
    if body.trim_start().starts_with('{') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            match value.get("on") {
                Some(serde_json::Value::Bool(b)) => return *b,
                Some(serde_json::Value::Number(n)) => {
                    return n.as_f64().map(|v| v != 0.0).unwrap_or(true)
                }
                Some(serde_json::Value::String(s)) => {
                    return s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("on");
                }
                Some(serde_json::Value::Null) | None => {}
                Some(_) => return true,
            }
        }
    }
    match get("on") {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("yes"),
        None => true,
    }
}

/// Trigger from a JSON body; every field is optional and merges into `base`.
fn parse_trigger_json(body: &str, base: &TriggerConfig) -> Result<TriggerConfig, String> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("тело запроса не JSON: {e}"))?;
    if !v.is_object() {
        return Err("тело запроса должно быть объектом".into());
    }
    let mut cfg = base.clone();
    if let Some(raw) = field(&v, "mode")? {
        cfg.mode = trigger::Mode::from_str(&raw).ok_or_else(|| format!("неизвестный режим {raw}"))?;
    }
    if let Some(raw) = field(&v, "edge")? {
        cfg.edge = trigger::Edge::from_str(&raw).ok_or_else(|| format!("неизвестный фронт {raw}"))?;
    }
    if let Some(n) = number(&v, "source")? {
        if n < 0.0 || n > 7.0 || n.fract() != 0.0 {
            return Err("источник триггера должен быть каналом 1..8".into());
        }
        cfg.source = n as usize;
    }
    if let Some(n) = number(&v, "level_v")? {
        cfg.level_v = n as f32;
    }
    if let Some(n) = number(&v, "pretrigger_pct")? {
        cfg.pretrigger_pct = n as f32;
    }
    if let Some(n) = number(&v, "timeout_ms")? {
        if n < 0.0 || n > u32::MAX as f64 || n.fract() != 0.0 {
            return Err("таймаут должен быть целым числом миллисекунд".into());
        }
        cfg.timeout_ms = n as u32;
    }
    if let Some(n) = number(&v, "hysteresis_v")? {
        cfg.hysteresis_v = Some(n as f32);
    }
    cfg.validate()?;
    Ok(cfg)
}

/// Trigger from query parameters, for `curl` and for the old client's habits.
fn parse_trigger_query(
    get: &dyn Fn(&str) -> Option<String>,
    base: &TriggerConfig,
) -> Result<TriggerConfig, String> {
    let mut cfg = base.clone();
    if let Some(raw) = get("mode") {
        cfg.mode = trigger::Mode::from_str(&raw).ok_or_else(|| format!("неизвестный режим {raw}"))?;
    }
    if let Some(raw) = get("edge") {
        cfg.edge = trigger::Edge::from_str(&raw).ok_or_else(|| format!("неизвестный фронт {raw}"))?;
    }
    if let Some(raw) = get("source") {
        let n: usize = raw.parse().map_err(|_| "источник должен быть числом".to_string())?;
        cfg.source = n.checked_sub(1).ok_or_else(|| "источник должен быть 1..8".to_string())?;
    }
    if let Some(raw) = get("level") {
        cfg.level_v = raw.parse().map_err(|_| "уровень должен быть числом".to_string())?;
    }
    if let Some(raw) = get("pre") {
        cfg.pretrigger_pct = raw.parse().map_err(|_| "предыстория должна быть числом".to_string())?;
    }
    if let Some(raw) = get("timeout") {
        cfg.timeout_ms = raw.parse().map_err(|_| "таймаут должен быть числом".to_string())?;
    }
    cfg.validate()?;
    Ok(cfg)
}

fn field(v: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match v.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("поле {key} должно быть строкой")),
    }
}

fn number(v: &serde_json::Value, key: &str) -> Result<Option<f64>, String> {
    match v.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("поле {key} вне диапазона чисел")),
        Some(_) => Err(format!("поле {key} должно быть числом")),
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn serve_static(request: Request, assets: &Assets, path: &str) -> std::io::Result<()> {
    let rel = if path == "/" || path.is_empty() { "index.html" } else { path.trim_start_matches('/') };
    if rel.contains("..") {
        return request.respond(Response::empty(403));
    }

    if let Assets::Embedded = assets {
        return match crate::assets::lookup(rel) {
            Some((bytes, mime_type)) => {
                let mut resp = Response::from_data(bytes);
                resp.add_header(header("Content-Type", mime_type));
                let cc = if rel == "index.html" { "no-store" } else { "no-cache" };
                resp.add_header(header("Cache-Control", cc));
                request.respond(resp)
            }
            None => request.respond(Response::empty(404)),
        };
    }

    let Assets::Dir(ui_dir) = assets else { unreachable!() };
    let file = ui_dir.join(rel);
    match std::fs::read(&file) {
        Ok(bytes) => {
            let mut resp = Response::from_data(bytes);
            resp.add_header(header("Content-Type", mime(&file)));
            let cc = if file.ends_with("index.html") { "no-store" } else { "no-cache" };
            resp.add_header(header("Cache-Control", cc));
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
    json_code(request, 200, body)
}

fn json_code(request: Request, code: u16, body: &str) -> std::io::Result<()> {
    let mut resp = Response::from_string(body).with_status_code(code);
    resp.add_header(header("Content-Type", "application/json; charset=utf-8"));
    resp.add_header(header("Cache-Control", "no-store"));
    request.respond(resp)
}

/// 400 with a reason the UI can show verbatim.
fn bad(request: Request, message: &str) -> std::io::Result<()> {
    let body = serde_json::json!({ "ok": false, "error": message }).to_string();
    json_code(request, 400, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_trigger_merges_and_validates() {
        let base = TriggerConfig::default();
        let cfg = parse_trigger_json(
            r#"{"mode":"normal","edge":"fall","source":2,"level_v":1.5,"pretrigger_pct":40}"#,
            &base,
        )
        .expect("valid");
        assert_eq!(cfg.mode, trigger::Mode::Normal);
        assert_eq!(cfg.edge, trigger::Edge::Fall);
        assert_eq!(cfg.source, 2);
        assert!((cfg.level_v - 1.5).abs() < 1e-6);
        assert!((cfg.pretrigger_pct - 40.0).abs() < 1e-6);
        assert_eq!(cfg.timeout_ms, base.timeout_ms);
    }

    #[test]
    fn json_trigger_rejects_bad_input() {
        let base = TriggerConfig::default();
        assert!(parse_trigger_json(r#"{"level_v":"high"}"#, &base).is_err());
        assert!(parse_trigger_json(r#"{"mode":"sometimes"}"#, &base).is_err());
        assert!(parse_trigger_json(r#"{"level_v":1e9}"#, &base).is_err());
        assert!(parse_trigger_json(r#"{"pretrigger_pct":99}"#, &base).is_err());
        assert!(parse_trigger_json(r#"{"source":9}"#, &base).is_err());
        assert!(parse_trigger_json("[1,2]", &base).is_err());
        assert!(parse_trigger_json("not json", &base).is_err());
    }

    #[test]
    fn query_trigger_is_one_based_for_the_source() {
        let base = TriggerConfig::default();
        let params = "mode=single&edge=rise&source=3&level=-0.25&pre=20";
        let get = |key: &str| -> Option<String> {
            params.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == key).then(|| v.to_string())
            })
        };
        let cfg = parse_trigger_query(&get, &base).expect("valid");
        assert_eq!(cfg.source, 2);
        assert_eq!(cfg.mode, trigger::Mode::Single);
        assert!((cfg.level_v + 0.25).abs() < 1e-6);
        assert!((cfg.pretrigger_pct - 20.0).abs() < 1e-6);
    }

    #[test]
    fn url_decoding_handles_percent_and_plus() {
        assert_eq!(urldecode("a%20b"), "a b");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("%D0%92"), "В");
        assert_eq!(urldecode("100%"), "100%");
    }
}
    #[test]
    fn hold_flag_reads_json_bodies() {
        let none = |_: &str| None;
        assert!(hold_flag("{\"on\":true}", &none));
        assert!(!hold_flag("{\"on\":false}", &none));
        assert!(!hold_flag("{\"on\":0}", &none));
        assert!(!hold_flag("{\"on\":\"false\"}", &none));
        // No body at all means "hold", which is what the plain button sends.
        assert!(hold_flag("", &none));
        assert!(hold_flag("{\"other\":1}", &none));
    }

    #[test]
    fn hold_flag_falls_back_to_the_query() {
        let params = "on=0";
        let get = |key: &str| -> Option<String> {
            params.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == key).then(|| v.to_string())
            })
        };
        assert!(!hold_flag("", &get));
        assert!(!hold_flag("{\"on\":false}", &get));
    }
