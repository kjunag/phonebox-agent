use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use serde_json::{json, Value};

use crate::approvals::Approvals;
use crate::auth::{self, Access};
use crate::feeds::Feeds;
use crate::metrics::{self, Metrics};
use crate::publish::public_config;
use crate::quotas::Quotas;
use crate::shortcuts::Memory;

#[derive(Clone, Copy)]
pub enum Side {
    Local,
    Lan,
}

pub struct App {
    pub config_path: PathBuf,
    pub cert_path: PathBuf,
    pub metrics: Arc<Metrics>,
    pub feeds: Arc<Feeds>,
    pub quotas: Arc<Quotas>,
    pub shortcuts: Memory,
    pub approvals: Approvals,
    pub push: Mutex<PushStore>,
    pub phone_seen: AtomicI64,
    pub pairing: crate::pairing::Codes,
    pub lan_port: Option<u16>,
}

#[derive(Default)]
pub struct PushStore {
    version: i64,
    items: Vec<(String, Value)>,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(settings_page))
        .route("/panel", get(panel_redirect))
        .route("/panel/", get(panel_index))
        .route("/panel/{*file}", get(panel_file))
        .route("/api/state", get(state))
        .route("/api/stream", get(stream))
        .route("/api/config", get(config_route))
        .route("/api/procs", get(procs))
        .route("/api/procs/icon", get(proc_icon))
        .route("/api/procs/detail", get(proc_detail))
        .route("/api/procs/kill", post(kill_proc))
        .route("/api/info", get(info))
        .route("/api/volume", post(volume))
        .route("/api/art", get(art))
        .route("/api/media/{action}", post(media))
        .route("/api/push", get(push_list).delete(push_clear))
        .route("/api/push/{key}", get(push_get).post(push_set).put(push_set).delete(push_delete))
        .route("/api/notify", post(notify))
        .route("/api/goto/{page}", post(goto_page))
        .route("/api/refresh", post(refresh))
        .route("/api/wake/macs", get(wake_macs))
        .route("/api/lan", get(lan_info))
        .route("/api/pair", get(pair_info))
        .route("/api/pair/start", post(pair_start))
        .route("/api/pair/cancel", post(pair_cancel))
        .route("/api/pair/status", get(pair_status))
        .route("/api/pair/claim", post(pair_claim))
        .route("/api/update", get(update_status))
        .route("/api/calendar", get(calendar))
        .route("/api/work", get(work))
        .route("/api/quotas", get(quotas))
        .route("/api/batteries", get(batteries))
        .route("/api/run/{id}", post(run_shortcut))
        .route("/api/settings", get(settings_get).put(settings_put))
        .route("/api/widget/{page}/{id}/note", post(save_note))
        .route("/api/catalog", get(catalog))
        .route("/api/approvals", get(approvals))
        .route("/api/approvals/{id}", post(decide_approval))
        .route("/api/hooks/claude/permission", post(claude_hook))
        .route("/api/hooks/codex/permission", post(codex_hook))
        .fallback(fallback)
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

async fn guard(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    if req.headers().contains_key("x-phonebox-app") {
        app.phone_seen.store(unix_ms(), Ordering::Relaxed);
    }
    let side = req.extensions().get::<Side>().copied().unwrap_or(Side::Local);
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    match side {
        Side::Local => {
            let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("");
            let origin = header_str(req.headers(), header::ORIGIN);
            if host != "127.0.0.1" && host != "localhost" {
                return StatusCode::FORBIDDEN.into_response();
            }
            if !origin.is_empty() && !origin.starts_with("http://127.0.0.1:") && !origin.starts_with("http://localhost:") {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        Side::Lan => {
            let Ok(cfg) = load_config(&app.config_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
            let lan = lan_of(&cfg);
            let origin = header_str(req.headers(), header::ORIGIN);
            let authorization = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
            let query_token = req.uri().query().and_then(query_value);
            match auth::authorize(lan.enabled, &lan.token, &lan.webhook, &method, &path, &origin, authorization, query_token.as_deref()) {
                Access::Denied => return StatusCode::FORBIDDEN.into_response(),
                Access::PairingClaim => {}
                Access::Phone | Access::Webhook => {}
            }
        }
    }
    req.extensions_mut().insert(side);
    next.run(req).await
}

async fn state(State(app): State<Arc<App>>) -> Json<Value> {
    let version = config_version(&app.config_path);
    let mut body = app.metrics.state(&version);
    if let Ok(push) = app.push.lock() {
        body["pushVersion"] = json!(push.version);
    }
    if let Ok(cfg) = load_config(&app.config_path) {
        let muted = body["audio"]["muted"].as_bool();
        body["toggles"] = crate::shortcuts::toggles(&cfg, muted, &app.shortcuts);
        body["cycles"] = crate::shortcuts::cycles(&cfg, &app.shortcuts);
    } else if let Some(muted) = body["audio"]["muted"].as_bool() {
        body["toggles"] = json!({ "mute": muted });
    }
    Json(body)
}

async fn stream(State(app): State<Arc<App>>) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let app = app.clone();
    let events = stream::unfold(true, move |first| {
        let app = app.clone();
        async move {
            if !first {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            let version = config_version(&app.config_path);
            let body = app.metrics.state(&version).to_string();
            Some((Ok(Event::default().event("state").data(body)), false))
        }
    });
    Sse::new(events)
}

async fn config_route(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Ok(cfg) = load_config(&app.config_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    let device = headers.get("x-phonebox-device").and_then(|v| v.to_str().ok());
    Json(json!({ "version": config_version(&app.config_path), "config": public_config(&cfg, device) })).into_response()
}

async fn procs(State(app): State<Arc<App>>, Query(query): Query<Vec<(String, String)>>) -> Response {
    let top = query.iter().find(|(key, _)| key == "top").map(|(_, value)| value.as_str());
    let top = match top {
        None => 40usize,
        Some(value) => match value.parse::<i32>() {
            Ok(n) => n.clamp(1, 200) as usize,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
    };
    Json(app.metrics.procs(top)).into_response()
}

async fn proc_icon(Query(query): Query<Vec<(String, String)>>) -> Response {
    let name = query.iter().find(|(key, _)| key == "name").map(|(_, value)| value.as_str()).unwrap_or("");
    match metrics::proc_icon(name) {
        Some((bytes, mime)) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-store")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn proc_detail(State(app): State<Arc<App>>, Query(query): Query<Vec<(String, String)>>) -> Json<Value> {
    let name = query.iter().find(|(key, _)| key == "name").map(|(_, value)| value.as_str()).unwrap_or("");
    Json(app.metrics.detail(name))
}

async fn kill_proc(State(app): State<Arc<App>>, Query(query): Query<Vec<(String, String)>>) -> Response {
    let cfg = load_config(&app.config_path).unwrap_or_else(|_| json!({}));
    if !allow_kill(&cfg) {
        return (StatusCode::FORBIDDEN, Json(json!({"killed": 0, "failed": 0, "error": "Terminating processes is disabled in the settings"}))).into_response();
    }
    let pid = query.iter().find(|(key, _)| key == "pid").and_then(|(_, value)| value.parse::<i32>().ok());
    let name = query.iter().find(|(key, _)| key == "name").map(|(_, value)| value.clone()).unwrap_or_default();
    let result = match pid {
        Some(pid) => metrics::kill_pid(pid),
        None => metrics::kill_named(&name),
    };
    let status = if result.killed > 0 { StatusCode::OK } else { StatusCode::CONFLICT };
    (status, Json(json!({"killed": result.killed, "failed": result.failed, "error": result.error}))).into_response()
}

fn allow_kill(cfg: &Value) -> bool {
    cfg.get("pages").and_then(Value::as_array).is_some_and(|pages| pages.iter().any(|page| {
        page.get("enabled").and_then(Value::as_bool).unwrap_or(true)
            && page.get("widgets").and_then(Value::as_array).is_some_and(|widgets| widgets.iter().any(|widget| {
                widget.get("type").and_then(Value::as_str) == Some("processes")
                    && widget.pointer("/options/allowKill").and_then(Value::as_bool) != Some(false)
            }))
    }))
}

async fn info(State(app): State<Arc<App>>) -> Json<Value> {
    let seen = app.phone_seen.load(Ordering::Relaxed);
    let connected = seen > 0 && unix_ms() - seen < 15_000;
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": "linux",
        "configPath": app.config_path,
        "logPath": log_path(),
        "adb": null,
        "connected": connected,
        "via": if connected { json!("Wi-Fi") } else { Value::Null },
        "devices": [],
        "appMissing": [],
        "usbProblem": null,
        "webviewProblem": null,
    }))
}

async fn volume(Query(query): Query<Vec<(String, String)>>) -> Response {
    let op = query.iter().find(|(k, _)| k == "op").map(|(_, v)| v.as_str());
    let level = query.iter().find(|(k, _)| k == "level").and_then(|(_, v)| v.parse::<i32>().ok());
    let ok = match op {
        Some("mute") => metrics::toggle_mute(),
        Some("up") => metrics::step_volume(5),
        Some("down") => metrics::step_volume(-5),
        _ => level.is_some_and(metrics::set_volume),
    };
    if ok { Json(metrics::current_audio()).into_response() } else { StatusCode::BAD_REQUEST.into_response() }
}

async fn art() -> Response {
    match metrics::album_art() {
        Some((bytes, mime)) => ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "private, max-age=86400")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn media(UrlPath(action): UrlPath<String>, Query(query): Query<Vec<(String, String)>>) -> Response {
    let ms = query.iter().find(|(k, _)| k == "ms").and_then(|(_, v)| v.parse::<i64>().ok());
    if metrics::media_action(&action, ms) {
        Json(json!({"ok": true, "media": metrics::current_media()})).into_response()
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

async fn push_list(State(app): State<Arc<App>>) -> Json<Value> {
    let Ok(push) = app.push.lock() else { return Json(json!({"version": 0, "items": []})) };
    Json(json!({ "version": push.version, "items": push.items }))
}

async fn push_get(State(app): State<Arc<App>>, UrlPath(key): UrlPath<String>) -> Response {
    let Ok(push) = app.push.lock() else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    push.items.iter().find(|(name, _)| name == &key).map(|(_, value)| Json(value.clone()).into_response()).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

async fn push_set(State(app): State<Arc<App>>, UrlPath(key): UrlPath<String>, body: Bytes) -> Response {
    let data = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::String(String::from_utf8_lossy(&body).into_owned())) };
    let Ok(mut push) = app.push.lock() else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    push.version += 1;
    if let Some(slot) = push.items.iter_mut().find(|(name, _)| name == &key) {
        slot.1 = data;
    } else {
        push.items.push((key.clone(), data));
    }
    let version = push.version;
    Json(json!({"ok": true, "key": key, "version": version})).into_response()
}

async fn push_delete(State(app): State<Arc<App>>, UrlPath(key): UrlPath<String>) -> StatusCode {
    let Ok(mut push) = app.push.lock() else { return StatusCode::INTERNAL_SERVER_ERROR };
    let before = push.items.len();
    push.items.retain(|(name, _)| name != &key);
    if push.items.len() == before { StatusCode::NOT_FOUND } else { push.version += 1; StatusCode::OK }
}

async fn push_clear(State(app): State<Arc<App>>) -> StatusCode {
    if let Ok(mut push) = app.push.lock() {
        push.items.clear();
        push.version += 1;
    }
    StatusCode::OK
}

async fn notify(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let data = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let has_text = data.get("text").is_some() || data.get("title").is_some() || data.as_str().is_some();
    if !has_text { return (StatusCode::BAD_REQUEST, Json(json!({"error": "give a text or a title"}))).into_response() }
    let Ok(mut push) = app.push.lock() else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    push.version += 1;
    Json(json!({"ok": true, "version": push.version})).into_response()
}

async fn goto_page(State(app): State<Arc<App>>, UrlPath(page): UrlPath<String>) -> Json<Value> {
    let Ok(mut push) = app.push.lock() else { return Json(json!({"ok": false})) };
    push.version += 1;
    Json(json!({"ok": true, "page": page, "version": push.version}))
}

async fn refresh(State(app): State<Arc<App>>) -> Json<Value> {
    let Ok(mut push) = app.push.lock() else { return Json(json!({"ok": false})) };
    push.version += 1;
    Json(json!({"ok": true, "version": push.version}))
}

async fn wake_macs() -> Json<Value> {
    Json(json!({"macs": crate::pairing::macs()}))
}

fn pair_payload(app: &App, include_token: bool) -> Value {
    let Ok(cfg) = load_config(&app.config_path) else { return json!({"enabled": false, "pc": crate::pairing::hostname()}) };
    let lan = lan_of(&cfg);
    let fingerprint = crate::tls::fingerprint(&app.cert_path);
    if !lan.enabled || fingerprint.is_none() || app.lan_port.is_none() {
        return json!({"enabled": false, "pc": crate::pairing::hostname()});
    }
    let mut body = json!({
        "enabled": true,
        "pc": crate::pairing::hostname(),
        "port": lan.port,
        "fingerprint": fingerprint,
        "hosts": crate::pairing::addresses(),
        "macs": crate::pairing::macs(),
    });
    if include_token {
        body["token"] = Value::String(lan.token);
    }
    body
}

async fn pair_info(State(app): State<Arc<App>>) -> Json<Value> {
    Json(pair_payload(&app, true))
}

async fn pair_start(State(app): State<Arc<App>>) -> Json<Value> {
    let Ok(cfg) = load_config(&app.config_path) else { return Json(json!({"enabled": false, "configured": false})) };
    let lan = lan_of(&cfg);
    let Some(fingerprint) = crate::tls::fingerprint(&app.cert_path) else {
        return Json(json!({"enabled": false, "configured": lan.enabled}));
    };
    if !lan.enabled || app.lan_port.is_none() {
        return Json(json!({"enabled": false, "configured": lan.enabled}));
    }
    let Some((code, expires_at, attempt)) = app.pairing.start(crate::pairing::now_ms()) else {
        return Json(json!({"enabled": false, "configured": true}));
    };
    let hosts = crate::pairing::addresses();
    let link = crate::pairing::link(&crate::pairing::hostname(), &fingerprint, lan.port, &hosts, &code);
    let app_url = crate::pairing::app_download();
    Json(json!({
        "enabled": true,
        "pc": crate::pairing::hostname(),
        "hosts": hosts,
        "port": lan.port,
        "expiresAt": expires_at,
        "claims": app.pairing.claims(),
        "attempt": attempt,
        "check": crate::pairing::check(&fingerprint),
        "link": link,
        "qr": crate::pairing::svg(&link),
        "app": app_url,
        "appQr": crate::pairing::svg(&app_url),
    }))
}

async fn pair_cancel(State(app): State<Arc<App>>, Query(query): Query<Vec<(String, String)>>) -> StatusCode {
    let attempt = query.iter().find(|(key, _)| key == "attempt").and_then(|(_, value)| value.parse().ok()).unwrap_or(0);
    app.pairing.cancel(attempt);
    StatusCode::OK
}

async fn pair_status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({"claims": app.pairing.claims()}))
}

async fn pair_claim(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    if body.len() > 1024 { return StatusCode::FORBIDDEN.into_response() }
    let Ok(cfg) = load_config(&app.config_path) else { return StatusCode::FORBIDDEN.into_response() };
    let lan = lan_of(&cfg);
    if !lan.enabled || app.lan_port.is_none() || crate::tls::fingerprint(&app.cert_path).is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or("");
    let Some(code) = crate::pairing::read_code(content_type, &body) else { return StatusCode::FORBIDDEN.into_response() };
    if !app.pairing.claim(&code, crate::pairing::now_ms()) { return StatusCode::FORBIDDEN.into_response() }
    eprintln!("phonebox-agent phone paired over Wi-Fi");
    Json(pair_payload(&app, true)).into_response()
}

async fn update_status() -> Json<Value> {
    Json(json!({
        "status": {
            "agentVersion": env!("CARGO_PKG_VERSION"),
            "agentUpdate": false,
            "appVersion": null,
            "appUpdate": false,
            "latest": null,
            "lastCheck": null,
            "state": "idle",
        },
        "auto": true,
        "installed": false,
    }))
}

async fn calendar(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.feeds.calendar())
}

async fn work(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = load_config(&app.config_path).unwrap_or_else(|_| json!({}));
    Json(app.feeds.work(&cfg))
}

async fn batteries() -> Json<Value> {
    Json(metrics::batteries())
}

async fn quotas(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.quotas.snapshot())
}

async fn settings_get(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Ok(cfg) = load_config(&app.config_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    let body = crate::settings::public_settings(&cfg, desktop_settings(&headers));
    let mut response = Json(body).into_response();
    response.headers_mut().insert("x-config-version", crate::http::config_version(&app.config_path).parse().unwrap_or_else(|_| header::HeaderValue::from_static("0")));
    response
}

async fn settings_put(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or("");
    if !content_type.starts_with("application/json") { return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response() }
    let Ok(mut incoming) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "Invalid JSON"}))).into_response();
    };
    let existing = load_config(&app.config_path).unwrap_or_else(|_| json!({}));
    let port = std::env::var("PORT").ok().and_then(|value| value.parse().ok()).unwrap_or(8765);
    if let Err(error) = crate::settings::prepare_save(&mut incoming, &existing, port) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response();
    }
    let expected = headers.get("x-config-version").and_then(|value| value.to_str().ok());
    match crate::settings::save(&app.config_path, &incoming, expected) {
        Ok(version) => Json(json!({"version": version})).into_response(),
        Err(error) if error == crate::settings::CONFLICT => (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    }
}

fn desktop_settings(headers: &HeaderMap) -> bool {
    header_str(headers, header::HeaderName::from_static("sec-fetch-site")) == "same-origin"
        && header_str(headers, header::HeaderName::from_static("sec-fetch-mode")) == "cors"
        && header_str(headers, header::HeaderName::from_static("sec-fetch-dest")) == "empty"
}

async fn save_note(State(app): State<Arc<App>>, UrlPath((page, id)): UrlPath<(String, String)>, Query(query): Query<Vec<(String, String)>>) -> Response {
    let text = query.iter().find(|(key, _)| key == "text").map(|(_, value)| value.as_str());
    let version = query.iter().find(|(key, _)| key == "version").map(|(_, value)| value.as_str());
    match crate::settings::apply_note(&app.config_path, &page, &id, text, version) {
        Ok(version) => Json(json!({"version": version})).into_response(),
        Err((409, error)) => (StatusCode::CONFLICT, Json(json!({"error": error}))).into_response(),
        Err((404, error)) => (StatusCode::NOT_FOUND, Json(json!({"error": error}))).into_response(),
        Err((_, error)) => (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response(),
    }
}

async fn catalog() -> Response {
    ([(header::CONTENT_TYPE, "application/json")], crate::settings::catalog()).into_response()
}

async fn approvals(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({"items": app.approvals.snapshot()}))
}

async fn decide_approval(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Query(query): Query<Vec<(String, String)>>) -> StatusCode {
    let decision = query.iter().find(|(key, _)| key == "decision").map(|(_, value)| value.as_str()).unwrap_or("");
    let value = query.iter().find(|(key, _)| key == "value").map(|(_, value)| value.as_str());
    if app.approvals.decide(&id, decision, value) { StatusCode::OK } else { StatusCode::NOT_FOUND }
}

async fn claude_hook(State(app): State<Arc<App>>, body: Bytes) -> Response {
    permission_hook(&app, &body, "Claude Code", true).await
}

async fn codex_hook(State(app): State<Arc<App>>, body: Bytes) -> Response {
    permission_hook(&app, &body, "Codex", false).await
}

async fn permission_hook(app: &App, body: &[u8], source: &str, allow_questions: bool) -> Response {
    let Ok(input) = serde_json::from_slice::<Value>(body) else { return StatusCode::OK.into_response() };
    let cfg = load_config(&app.config_path).unwrap_or_else(|_| json!({}));
    let seen = app.phone_seen.load(Ordering::Relaxed);
    match crate::approvals::handle(&input, &cfg, &app.approvals, seen, source, allow_questions).await {
        Some(output) => Json(output).into_response(),
        None => StatusCode::OK.into_response(),
    }
}

async fn run_shortcut(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Query(query): Query<Vec<(String, String)>>) -> Response {
    let hold = query.iter().any(|(key, value)| key == "hold" && value == "true");
    let Ok(cfg) = load_config(&app.config_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    match crate::shortcuts::run(&cfg, &id, hold, &app.shortcuts) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            eprintln!("shortcut {id} failed: {error}");
            (StatusCode::BAD_REQUEST, Json(json!({"error": "The shortcut failed. The agent log on the computer has the details."}))).into_response()
        }
    }
}

async fn lan_info(State(app): State<Arc<App>>) -> Response {
    let Ok(cfg) = load_config(&app.config_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    let lan = lan_of(&cfg);
    Json(json!({
        "configured": lan.enabled,
        "active": lan.enabled && app.cert_path.is_file(),
        "port": lan.port,
        "restartNeeded": lan.enabled != app.lan_port.is_some() || app.lan_port.is_some_and(|bound| bound != lan.port),
        "hosts": crate::pairing::addresses(),
        "fingerprint": fingerprint_of(&app.cert_path),
    })).into_response()
}

fn asset(name: &str) -> PathBuf {
    crate::assets::asset(name)
}

fn panel_root() -> PathBuf {
    asset("www")
}

fn panel_name_ok(file: &str) -> bool {
    let mut parts = file.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), None, None) => file_ok(name),
        (Some("widgets"), Some(name), None) => file_ok(name),
        _ => false,
    }
}

fn file_ok(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn panel_response(file: &str) -> Response {
    if !panel_name_ok(file) { return StatusCode::NOT_FOUND.into_response() }
    let path = panel_root().join(file);
    let Ok(bytes) = std::fs::read(&path) else { return StatusCode::NOT_FOUND.into_response() };
    let kind = match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "no-store")], bytes).into_response()
}

async fn panel_redirect(req: Request) -> Response {
    let query = req.uri().query().map(|value| format!("?{value}")).unwrap_or_default();
    (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, format!("/panel/{query}"))]).into_response()
}

async fn panel_index() -> Response {
    panel_response("index.html")
}

async fn panel_file(UrlPath(file): UrlPath<String>) -> Response {
    panel_response(&file)
}

async fn settings_page() -> Response {
    let path = asset("settings.html");
    match std::fs::read_to_string(path) {
        Ok(html) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html).into_response(),
        Err(_) => (StatusCode::OK, "PhoneBox agent").into_response(),
    }
}

async fn fallback(req: Request) -> Response {
    let path = req.uri().path();
    if req.method() == axum::http::Method::GET && path.starts_with("/api/") {
        return Json(empty_for(path)).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn empty_for(path: &str) -> Value {
    if path.starts_with("/api/docker") { json!({"snapshot": [], "control": false}) }
    else if path.starts_with("/api/feeds") { json!({"items": [], "updated": null, "errors": [], "canOpen": false}) }
    else if path.starts_with("/api/photos") { json!({"ids": [], "error": null, "scanning": false}) }
    else if path.starts_with("/api/calendar") { json!({"events": []}) }
    else if path.starts_with("/api/quotas") { json!({"providers": [], "overview": null}) }
    else if path.starts_with("/api/work") { json!({"items": []}) }
    else if path.starts_with("/api/approvals") { json!({"items": []}) }
    else if path.starts_with("/api/apps") { json!({"apps": []}) }
    else if path.starts_with("/api/batteries") { json!({"devices": []}) }
    else if path.starts_with("/api/spectrum") { json!({"shown": false, "active": false, "fps": 0, "b": null}) }
    else { json!({}) }
}

struct Lan {
    enabled: bool,
    port: u16,
    token: String,
    webhook: String,
}

fn lan_of(cfg: &Value) -> Lan {
    Lan {
        enabled: cfg.pointer("/lan/enabled").and_then(Value::as_bool).unwrap_or(false),
        port: cfg.pointer("/lan/port").and_then(Value::as_u64).unwrap_or(8766) as u16,
        token: cfg.pointer("/lan/token").and_then(Value::as_str).unwrap_or("").to_string(),
        webhook: cfg.get("webhookToken").and_then(Value::as_str).unwrap_or("").to_string(),
    }
}

pub fn load_config(path: &Path) -> Result<Value, ()> {
    let text = std::fs::read_to_string(path).map_err(|_| ())?;
    let stripped = json_comments::StripComments::new(text.as_bytes());
    serde_json::from_reader(stripped).map_err(|_| ())
}

pub fn config_version(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| time.as_millis().to_string())
        .unwrap_or_else(|| "0".into())
}

fn fingerprint_of(path: &Path) -> Value {
    crate::tls::fingerprint(path).map(|value| Value::String(value)).unwrap_or(Value::Null)
}

fn log_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let mut home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.push(".local/share");
        home
    });
    base.join("PhoneBox/agent.log")
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> String {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

fn query_value(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "token" { return Some(value.to_string()) }
    }
    None
}

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|time| time.as_millis() as i64).unwrap_or(0)
}
