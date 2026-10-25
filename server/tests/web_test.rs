//! The browser gateway end to end: pairing, token-gated WebSocket,
//! origin checks, remote-scope method limits and revocation.

use std::sync::Arc;
use std::time::Duration;

use agentmux_core::{AgentRegistry, Config, Orchestrator, Store};
use agentmux_server::{serve_web, Daemon, WebOptions};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

struct Gw {
    daemon: Arc<Daemon>,
    addr: std::net::SocketAddr,
    _dir: tempfile::TempDir,
}

async fn gateway() -> Gw {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let orch = Orchestrator::new(
        store,
        AgentRegistry::from_config(&Config::default()),
        dir.path().to_path_buf(),
    );
    let daemon = Daemon::new(orch);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_web(daemon.clone(), listener, WebOptions::default()));
    Gw {
        daemon,
        addr,
        _dir: dir,
    }
}

/// Minimal HTTP/1.1 client: (status, body).
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &str,
    extra: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let status = out.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = out.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

async fn pair(gw: &Gw) -> String {
    let code = gw.daemon.web_auth().create_code();
    let (status, body) = http(
        gw.addr,
        "POST",
        "/api/pair",
        &json!({"code": code, "name": "t"}).to_string(),
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn connect(
    gw: &Gw,
    token: &str,
    origin: Option<&str>,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{}/ws", gw.addr)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&format!("agentmux.v1, token.{token}")).unwrap(),
    );
    if let Some(o) = origin {
        req.headers_mut()
            .insert("Origin", HeaderValue::from_str(o).unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

async fn call(ws: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    ws.send(Message::text(
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string(),
    ))
    .await
    .unwrap();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(t.as_str()).unwrap();
            if v["id"] == json!(id) {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn paired_device_can_call_rpc_over_websocket() {
    let gw = gateway().await;
    let token = pair(&gw).await;
    let mut ws = connect(&gw, &token, Some(&format!("http://{}", gw.addr)))
        .await
        .unwrap();
    let r = call(&mut ws, 1, "server/status", Value::Null).await;
    assert!(r["result"]["version"].is_string(), "{r}");
    let r = call(&mut ws, 2, "project/list", Value::Null).await;
    assert!(r["result"].is_object() || r["result"].is_array(), "{r}");
}

#[tokio::test]
async fn bad_token_and_foreign_origin_are_rejected() {
    let gw = gateway().await;
    assert!(connect(&gw, "nope", None).await.is_err());
    let token = pair(&gw).await;
    assert!(connect(&gw, &token, Some("https://evil.example"))
        .await
        .is_err());
    assert!(connect(&gw, &token, None).await.is_ok());
}

#[tokio::test]
async fn pairing_codes_are_single_use_and_wrong_ones_fail() {
    let gw = gateway().await;
    let code = gw.daemon.web_auth().create_code();
    let body = json!({"code": code, "name": "a"}).to_string();
    assert_eq!(http(gw.addr, "POST", "/api/pair", &body, "").await.0, 200);
    assert_eq!(http(gw.addr, "POST", "/api/pair", &body, "").await.0, 401);
    let evil = http(
        gw.addr,
        "POST",
        "/api/pair",
        &body,
        "Origin: https://evil.example\r\n",
    )
    .await;
    assert_eq!(evil.0, 403);
}

#[tokio::test]
async fn remote_connections_cannot_shut_down_or_mint_codes() {
    let gw = gateway().await;
    let token = pair(&gw).await;
    let mut ws = connect(&gw, &token, None).await.unwrap();
    for (i, m) in [
        "server/shutdown",
        "web/pair_create",
        "web/device_list",
        "web/device_revoke",
    ]
    .iter()
    .enumerate()
    {
        let r = call(&mut ws, i as u64 + 1, m, json!({})).await;
        assert_eq!(r["error"]["code"], -32601, "{m}: {r}");
    }
    // The daemon is still serving.
    assert!(call(&mut ws, 9, "server/status", Value::Null).await["result"].is_object());
}

#[tokio::test]
async fn revoked_tokens_cannot_connect_again() {
    let gw = gateway().await;
    let token = pair(&gw).await;
    let dev = gw.daemon.web_auth().list().remove(0);
    assert!(gw.daemon.web_auth().revoke(&dev.id));
    assert!(connect(&gw, &token, None).await.is_err());
}

#[tokio::test]
async fn static_shell_is_served_with_security_headers() {
    let gw = gateway().await;
    let mut s = TcpStream::connect(gw.addr).await.unwrap();
    s.write_all(
        format!(
            "GET /some/spa/route HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            gw.addr
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let lower = out.to_lowercase();
    assert!(lower.starts_with("http/1.1 200"), "{out}");
    assert!(lower.contains("content-security-policy"));
    assert!(lower.contains("referrer-policy: no-referrer"));
    assert!(lower.contains("<!doctype html>") || lower.contains("<!doctype"));
}

#[tokio::test]
async fn whoami_distinguishes_valid_and_revoked_tokens() {
    let gw = gateway().await;
    let token = pair(&gw).await;
    let ok = http(
        gw.addr,
        "GET",
        "/api/whoami",
        "",
        &format!("Authorization: Bearer {token}\r\n"),
    )
    .await;
    assert_eq!(ok.0, 200);
    assert_eq!(http(gw.addr, "GET", "/api/whoami", "", "").await.0, 401);
    let dev = gw.daemon.web_auth().list().remove(0);
    gw.daemon.web_auth().revoke(&dev.id);
    let gone = http(
        gw.addr,
        "GET",
        "/api/whoami",
        "",
        &format!("Authorization: Bearer {token}\r\n"),
    )
    .await;
    assert_eq!(gone.0, 401);
}
