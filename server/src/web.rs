//! HTTP/WebSocket gateway for browsers.
//!
//! The wire protocol is the daemon's own newline-delimited JSON-RPC: each
//! WebSocket text frame is one message. Instead of re-implementing the
//! connection logic, a WebSocket is bridged onto an in-memory byte pipe
//! that feeds [`handle_conn_io`] with [`ConnScope::Remote`], so
//! subscriptions, ordering, size limits and lag handling are identical to
//! the unix socket.
//!
//! Security model (the URL is public — see `web_auth`):
//! - `/api/pair` exchanges a one-time code for a device token (throttled).
//! - `/ws` requires a valid device token (sent as a WebSocket
//!   sub-protocol, never in the URL) and a same-origin `Origin`.
//! - Static assets are the only unauthenticated content.
//! - Responses carry a strict CSP and `no-referrer`.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::rpc_server::{handle_conn_io, ConnScope, Daemon};
use crate::web_auth::PairError;

const SUBPROTOCOL: &str = "agentmux.v1";
const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(RustEmbed)]
#[folder = "../web/dist"]
struct Assets;

#[derive(Clone, Debug, Default)]
pub struct WebOptions {
    /// Serve assets from this directory instead of the embedded build.
    pub web_dir: Option<PathBuf>,
    /// Extra origins (`https://host`) allowed besides the request's own host.
    pub allowed_origins: Vec<String>,
}

struct AppState {
    daemon: Arc<Daemon>,
    opts: WebOptions,
}

/// Serve the gateway on `listener` until the task is dropped.
pub async fn serve_web(
    daemon: Arc<Daemon>,
    listener: TcpListener,
    opts: WebOptions,
) -> std::io::Result<()> {
    let state = Arc::new(AppState { daemon, opts });
    let app = Router::new()
        .route("/api/pair", post(pair))
        .route("/api/whoami", get(whoami))
        .route("/ws", get(ws_upgrade))
        .fallback(get(static_asset))
        .layer(middleware::map_response(security_headers))
        .with_state(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' data:; \
             style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; \
             form-action 'self'",
        ),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    resp
}

/// Best-effort client address. Behind the local tunnel process every peer
/// is loopback, so trust the proxy's forwarded header only in that case.
fn client_key(peer: SocketAddr, headers: &HeaderMap) -> String {
    if peer.ip().is_loopback() {
        for name in ["cf-connecting-ip", "x-forwarded-for"] {
            if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
                if let Some(first) = v.split(',').next() {
                    if let Ok(ip) = first.trim().parse::<IpAddr>() {
                        return ip.to_string();
                    }
                }
            }
        }
    }
    peer.ip().to_string()
}

/// `200` with the device name when the bearer token is valid, else `401`.
/// The SPA uses it to tell "token revoked" from "network down".
async fn whoami(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let device = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| st.daemon.web_auth().verify(t));
    match device {
        Some(d) => Json(serde_json::json!({ "device": d.name })).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default)]
    name: String,
}

async fn pair(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PairBody>,
) -> Response {
    if !origin_ok(&st.opts, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let client = client_key(peer, &headers);
    match st.daemon.web_auth().redeem(&body.code, &body.name, &client) {
        Ok((device, token)) => {
            Json(serde_json::json!({ "token": token, "device": device })).into_response()
        }
        Err(PairError::Invalid) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid or expired code"})),
        )
            .into_response(),
        Err(PairError::Throttled) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "too many attempts; try again later"})),
        )
            .into_response(),
    }
}

/// Same-origin check against `Host` (plus configured extras). A missing
/// `Origin` (non-browser client) passes: such clients still need a token.
fn origin_ok(opts: &WebOptions, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if opts.allowed_origins.iter().any(|o| o == origin) {
        return true;
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin_host = origin.split_once("://").map(|(_, h)| h);
    matches!((host, origin_host), (Some(h), Some(o)) if h.eq_ignore_ascii_case(o))
}

fn token_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("token.").map(str::to_string))
}

async fn ws_upgrade(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&st.opts, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(device) = token_from(&headers).and_then(|t| st.daemon.web_auth().verify(&t)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let daemon = st.daemon.clone();
    ws.protocols([SUBPROTOCOL])
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| bridge(socket, daemon, device.id))
}

/// Pump one WebSocket through the shared connection handler.
async fn bridge(socket: WebSocket, daemon: Arc<Daemon>, device_id: String) {
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(server_io);
    let conn = tokio::spawn(handle_conn_io(sr, sw, daemon.clone(), ConnScope::Remote));
    let (cr, mut cw) = tokio::io::split(client_io);
    let (mut ws_tx, mut ws_rx) = socket.split();

    // daemon -> browser; also owns keep-alive pings and revocation checks.
    let auth = daemon.web_auth().clone();
    let mut outbound = tokio::spawn(async move {
        let mut lines = BufReader::new(cr).lines();
        // Revocation is checked every tick; pings go out every fourth.
        let mut ping = tokio::time::interval(Duration::from_secs(5));
        let mut ticks = 0u32;
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        if ws_tx.send(Message::Text(l.into())).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                },
                _ = ping.tick() => {
                    if !auth.is_active(&device_id) {
                        let _ = ws_tx.send(Message::Close(None)).await;
                        break;
                    }
                    ticks += 1;
                    if ticks.is_multiple_of(4) && ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // browser -> daemon: one frame = one line.
    let inbound = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(t) => {
                    let line = match serde_json::from_str::<Value>(t.as_str()) {
                        Ok(v) => v.to_string(),
                        Err(_) => t.as_str().replace(['\n', '\r'], " "),
                    };
                    if cw.write_all(line.as_bytes()).await.is_err()
                        || cw.write_all(b"\n").await.is_err()
                    {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = inbound => {}
        _ = &mut outbound => {}
    }
    outbound.abort();
    conn.abort();
}

async fn static_asset(State(st): State<Arc<AppState>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let load = |p: &str| -> Option<Vec<u8>> {
        match &st.opts.web_dir {
            Some(dir) => std::fs::read(dir.join(p)).ok(),
            None => Assets::get(p).map(|f| f.data.into_owned()),
        }
    };
    // SPA: unknown paths without an extension fall back to the app shell.
    let (name, body) = match load(path) {
        Some(b) => (path.to_string(), b),
        None if !path.contains('.') => match load("index.html") {
            Some(b) => ("index.html".to_string(), b),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let mime = mime_guess::from_path(&name).first_or_octet_stream();
    let cache = if name == "index.html" {
        "no-store"
    } else {
        "public, max-age=31536000, immutable"
    };
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tunnel: reach the gateway from the internet without opening ports
// ---------------------------------------------------------------------------

/// Run `cloudflared` pointing at the local gateway and publish the URL it
/// reports. Restarts with backoff if it exits. Needs `cloudflared` on PATH;
/// a named tunnel is used when `AGENTMUX_TUNNEL_TOKEN` and
/// `AGENTMUX_PUBLIC_URL` are set (stable URL), otherwise a temporary
/// `trycloudflare.com` one.
pub async fn run_tunnel(daemon: Arc<Daemon>, port: u16) {
    let token = std::env::var("AGENTMUX_TUNNEL_TOKEN").ok();
    let fixed = std::env::var("AGENTMUX_PUBLIC_URL").ok();
    let mut backoff = Duration::from_secs(2);
    loop {
        let mut cmd = tokio::process::Command::new("cloudflared");
        cmd.arg("tunnel").arg("--no-autoupdate");
        match &token {
            Some(t) => cmd
                .args(["run", "--token", t, "--url"])
                .arg(format!("http://127.0.0.1:{port}")),
            None => cmd.arg("--url").arg(format!("http://127.0.0.1:{port}")),
        };
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "agentmux-server: cannot start `cloudflared` ({e}). Install it \
                     (https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) \
                     or use --web-expose local with your own proxy."
                );
                return;
            }
        };
        if let Some(url) = &fixed {
            announce(&daemon, url);
        }
        if let Some(stderr) = child.stderr.take() {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if fixed.is_none() {
                    if let Some(url) = find_trycloudflare_url(&line) {
                        backoff = Duration::from_secs(2);
                        announce(&daemon, &url);
                    }
                }
            }
        }
        let _ = child.wait().await;
        daemon.set_web_url(None);
        eprintln!("agentmux-server: tunnel exited; restarting in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

fn find_trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

/// Record the public URL and print a ready-to-open pairing link.
pub fn announce(daemon: &Daemon, base: &str) {
    daemon.set_web_url(Some(base.to_string()));
    let code = daemon.web_auth().create_code();
    eprintln!("agentmux-server: web UI at {base}");
    eprintln!("agentmux-server: pair a device (valid 5 min, single use): {base}/#pair={code}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_url_is_extracted_from_cloudflared_logs() {
        let l =
            "2026-10-25T00:00:00Z INF |  https://quick-fox-1234.trycloudflare.com                |";
        assert_eq!(
            find_trycloudflare_url(l).as_deref(),
            Some("https://quick-fox-1234.trycloudflare.com")
        );
        assert!(find_trycloudflare_url("INF see https://developers.cloudflare.com/x").is_none());
    }

    #[test]
    fn origin_must_match_host_or_allowlist() {
        let opts = WebOptions {
            allowed_origins: vec!["https://ok.example".into()],
            ..Default::default()
        };
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "box.example".parse().unwrap());
        assert!(origin_ok(&opts, &h)); // no Origin: non-browser
        h.insert(header::ORIGIN, "https://box.example".parse().unwrap());
        assert!(origin_ok(&opts, &h));
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!origin_ok(&opts, &h));
        h.insert(header::ORIGIN, "https://ok.example".parse().unwrap());
        assert!(origin_ok(&opts, &h));
    }

    #[test]
    fn forwarded_ip_is_trusted_only_from_loopback() {
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        assert_eq!(
            client_key("127.0.0.1:1".parse().unwrap(), &h),
            "203.0.113.9"
        );
        assert_eq!(
            client_key("198.51.100.2:1".parse().unwrap(), &h),
            "198.51.100.2"
        );
    }

    #[test]
    fn token_comes_from_the_subprotocol_header() {
        let mut h = HeaderMap::new();
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "agentmux.v1, token.abc".parse().unwrap(),
        );
        assert_eq!(token_from(&h).as_deref(), Some("abc"));
    }
}
