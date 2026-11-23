mod approvals;
mod assets;
mod auth;
mod feeds;
mod http;
mod metrics;
mod pairing;
mod publish;
mod quotas;
mod settings;
mod shortcuts;
mod tls;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use http::App;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_openssl::SslStream;
use tower::ServiceExt;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").ok().and_then(|value| value.parse().ok()).unwrap_or(8765);
    let config_path = config_path();
    let cert_path = config_path.parent().unwrap_or(config_path.as_path()).join("lan-cert.pfx");
    let cfg = http::load_config(&config_path).unwrap_or(serde_json::json!({}));
    let lan_enabled = cfg.pointer("/lan/enabled").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let lan_port = cfg.pointer("/lan/port").and_then(serde_json::Value::as_u64).unwrap_or(8766) as u16;
    let feeds = feeds::Feeds::start(config_path.clone());
    let tls = if lan_enabled { tls::load(&cert_path).map_err(|err| eprintln!("phonebox-agent wifi certificate: {err}")).ok() } else { None };
    let app = Arc::new(App {
        config_path,
        cert_path: cert_path.clone(),
        metrics: metrics::Metrics::start(),
        feeds,
        quotas: quotas::Quotas::start(),
        shortcuts: shortcuts::Memory::new(),
        approvals: approvals::Approvals::new(),
        push: std::sync::Mutex::new(http::PushStore::default()),
        phone_seen: std::sync::atomic::AtomicI64::new(0),
        pairing: pairing::Codes::new(),
        lan_port: tls.as_ref().map(|_| lan_port),
    });
    let router = http::router(app);
    let local = tokio::spawn(serve_http(port, router.clone()));
    let lan = tls.map(|tls| {
        eprintln!("phonebox-agent wifi https://0.0.0.0:{lan_port} fingerprint {}", tls.fingerprint);
        tokio::spawn(serve_https(lan_port, tls.acceptor, router))
    });
    if cfg.pointer("/claude/phoneApprovals").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        let timeout = cfg.pointer("/claude/timeoutSec").and_then(serde_json::Value::as_i64).unwrap_or(90);
        match approvals::install_claude_hook(port, timeout) {
            Ok(true) => eprintln!("phonebox-agent claude permission hook installed"),
            Ok(false) => {}
            Err(err) => eprintln!("phonebox-agent claude permission hook: {err}"),
        }
    }
    eprintln!("phonebox-agent http://127.0.0.1:{port}");
    let _ = tokio::signal::ctrl_c().await;
    local.abort();
    if let Some(lan) = lan { lan.abort(); }
}

fn rewrite_agent_api(request: &mut Request) {
    let path = request.uri().path();
    let Some(rest) = path.strip_prefix("/agent/api") else { return };
    if !rest.is_empty() && !rest.starts_with('/') { return }
    let mut parts = request.uri().clone().into_parts();
    let rewritten = match request.uri().query() {
        Some(query) => format!("/api{rest}?{query}"),
        None => format!("/api{rest}"),
    };
    let Ok(path_and_query) = rewritten.parse() else { return };
    parts.path_and_query = Some(path_and_query);
    if let Ok(uri) = hyper::Uri::from_parts(parts) {
        *request.uri_mut() = uri;
    }
}

fn config_path() -> PathBuf {
    if let Some(path) = std::env::args().skip_while(|arg| arg != "--config").nth(1) {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let mut home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.push(".config");
        home
    });
    base.join("PhoneBox/config.json")
}

async fn serve_http(port: u16, app: axum::Router) {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await.expect("bind loopback");
    loop {
        let Ok((tcp, remote)) = listener.accept().await else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(tcp);
            let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                let app = app.clone();
                async move { dispatch(app, http::Side::Local, remote, req).await }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
        });
    }
}

async fn serve_https(port: u16, acceptor: openssl::ssl::SslAcceptor, app: axum::Router) {
    let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await.expect("bind wifi");
    loop {
        let Ok((tcp, remote)) = listener.accept().await else { continue };
        let app = app.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(ssl) = openssl::ssl::Ssl::new(acceptor.context()) else { return };
            let Ok(mut stream) = SslStream::new(ssl, tcp) else { return };
            if std::pin::Pin::new(&mut stream).accept().await.is_err() { return }
            let io = TokioIo::new(stream);
            let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                let app = app.clone();
                async move { dispatch(app, http::Side::Lan, remote, req).await }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
        });
    }
}

async fn dispatch(app: axum::Router, side: http::Side, remote: SocketAddr, request: hyper::Request<Incoming>) -> Result<hyper::Response<Body>, std::convert::Infallible> {
    let mut request = Request::from(request.map(Body::new));
    if matches!(side, http::Side::Local) {
        rewrite_agent_api(&mut request);
    }
    request.extensions_mut().insert(side);
    request.extensions_mut().insert(remote);
    Ok(app.oneshot(request).await.unwrap_or_else(|_| hyper::Response::new(Body::empty())))
}
