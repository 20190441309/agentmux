//! The agentmux daemon: newline-delimited JSON-RPC 2.0 over a unix socket.
//!
//! # Wire framing
//!
//! One JSON-RPC message per line (`\n`-delimited only — U+2028/2029 are
//! *not* treated as separators). Requests get one `RpcResponse` line;
//! subscribed connections additionally receive `session/event`
//! `RpcNotification` lines, which may interleave with responses.
//!
//! # Connection lifecycle
//!
//! Each accepted connection runs [`handle_conn`]: a read loop plus a
//! dedicated writer task fed by a bounded outbound queue, so event
//! notifications and responses share one ordered channel without
//! interleaving writes. `session/subscribe` acks first, *then* arms an
//! event-forwarder task that drains [`Orchestrator::subscribe`] into the
//! queue — so the subscribe response can never race a notification.
//!
//! Requests on one connection are dispatched sequentially — `prompt`
//! awaits the whole turn, so interleaved requests would reorder replies
//! anyway. A consequence: to `session/cancel` a hung prompt, the client
//! must use a *second* connection (the first is busy awaiting it). The
//! orchestrator is shared as `Arc<Orchestrator>` (all its
//! methods take `&self`); there is deliberately **no global lock** — a
//! turn-long lock would make `cancel`/`kill`/`status` unreachable while
//! a prompt is in flight. Per-session prompt serialization stays inside
//! the orchestrator's own `prompt_lock`.
//!
//! # Error mapping
//!
//! - Unparseable JSON line → `-32700` parse error, **connection stays
//!   open**.
//! - Valid JSON that is not a Request object → `-32600`.
//! - A request without `id` is a notification: dispatched
//!   fire-and-forget, no response.
//! - Unknown method → `-32601`; param decode failures → `-32602`;
//!   orchestrator `anyhow` errors → `-32603` via `From<anyhow::Error>`.
//!
//! # Lagged subscribers
//!
//! The event bus is lossy (`broadcast`). On `RecvError::Lagged(n)` the
//! forwarder emits a synthetic `session/event` carrying an
//! [`EventKind::Orchestrator`] note under the nil session id, so clients
//! learn a gap exists (the per-session JSONL log is authoritative).
//!
//! # Shutdown
//!
//! `server/shutdown` is answered and flushed to the socket *before* the
//! accept loop is signalled — the writer acks each outbound line, and the
//! handler awaits that ack before [`Daemon::request_shutdown`] fires.
//! `serve` then stops accepting, aborts remaining connections, unlinks
//! the socket file and returns.

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use agentmux_core::rpc::{
    self, RpcError, RpcNotification, RpcRequest, RpcResponse, M_AGENT_LIST, M_AGENT_REGISTER,
    M_PROJECT_LIST, M_PROJECT_REGISTER, M_PROJECT_REMOVE, M_SERVER_SHUTDOWN, M_SERVER_STATUS,
    M_SESSION_CANCEL, M_SESSION_CREATE, M_SESSION_KILL, M_SESSION_LIST, M_SESSION_PROMPT,
    M_SESSION_RESUME, M_SESSION_SUBSCRIBE, M_WORKSPACE_CREATE, M_WORKSPACE_LIST,
    M_WORKSPACE_REMOVE, N_SESSION_EVENT,
};
use agentmux_core::{
    AgentRegistry, Config, Event, EventKind, Orchestrator, Result, SessionId, Store,
};
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::task::JoinHandle;

/// Directory name under the XDG data/config roots.
const APP_DIR: &str = "agentmux";
/// Socket file name inside the data dir.
const SOCKET_FILE: &str = "agentmux.sock";
/// Config file name inside the config dir.
const CONFIG_FILE: &str = "config.toml";

/// `AGENTMUX_SOCK` overrides the socket path (below `--socket`).
const ENV_SOCK: &str = "AGENTMUX_SOCK";
/// `AGENTMUX_DATA_DIR` overrides the data dir (below `--data-dir`).
const ENV_DATA_DIR: &str = "AGENTMUX_DATA_DIR";
/// `AGENTMUX_CONFIG` overrides the config path (below `--config`).
const ENV_CONFIG: &str = "AGENTMUX_CONFIG";

/// One inbound line may be at most this large; longer lines get a
/// `-32700` and are drained to their terminating newline so the
/// connection stays usable.
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;

/// Per-connection outbound queue depth. A client that stops reading
/// stalls only its own writer; its event forwarder then starts lagging
/// behind the bus (handled per the module docs).
const OUTBOUND_QUEUE: usize = 256;

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Filesystem locations the daemon uses.
#[derive(Debug, Clone)]
pub struct ServerPaths {
    /// Metadata store + session JSONL logs (`<data_dir>/db.sqlite`,
    /// `<data_dir>/sessions/`).
    pub data_dir: PathBuf,
    /// The unix socket the daemon listens on.
    pub socket_path: PathBuf,
    /// TOML agent configuration.
    pub config_path: PathBuf,
}

impl ServerPaths {
    /// Resolve effective paths: flag > env var > default.
    ///
    /// - data dir: `--data-dir` > `AGENTMUX_DATA_DIR` > `$XDG_DATA_HOME/
    ///   agentmux` (default `~/.local/share/agentmux`)
    /// - socket: `--socket` > `AGENTMUX_SOCK` > `<data_dir>/agentmux.sock`
    /// - config: `--config` > `AGENTMUX_CONFIG` > `$XDG_CONFIG_HOME/
    ///   agentmux/config.toml` (default `~/.config/agentmux/config.toml`)
    pub fn resolve(
        socket: Option<PathBuf>,
        data_dir: Option<PathBuf>,
        config: Option<PathBuf>,
    ) -> ServerPaths {
        let data_dir = data_dir
            .or_else(|| env::var_os(ENV_DATA_DIR).map(PathBuf::from))
            .unwrap_or_else(default_data_dir);
        let socket_path = socket
            .or_else(|| env::var_os(ENV_SOCK).map(PathBuf::from))
            .unwrap_or_else(|| data_dir.join(SOCKET_FILE));
        let config_path = config
            .or_else(|| env::var_os(ENV_CONFIG).map(PathBuf::from))
            .unwrap_or_else(default_config_path);
        ServerPaths {
            data_dir,
            socket_path,
            config_path,
        }
    }
}

/// `$XDG_DATA_HOME/agentmux`, defaulting to `~/.local/share/agentmux`.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join(APP_DIR);
    }
    home_dir().join(".local/share").join(APP_DIR)
}

/// `$XDG_CONFIG_HOME/agentmux/config.toml`, defaulting to
/// `~/.config/agentmux/config.toml`.
pub fn default_config_path() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join(APP_DIR).join(CONFIG_FILE);
    }
    home_dir().join(".config").join(APP_DIR).join(CONFIG_FILE)
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Build a [`Daemon`] from resolved paths: open the store, load the
/// config, build a **probed** registry + the orchestrator.
///
/// `AgentRegistry::probed` probes each adapter's `command` once at boot —
/// config-loaded profiles are all `available: false`, and
/// `create_session`/`resume` gate on that flag. `list_agents` re-probes
/// on every call (writing back), so runtime installs get picked up.
pub fn build_daemon(paths: &ServerPaths) -> Result<Arc<Daemon>> {
    let store = Store::open(&paths.data_dir)?;
    // The data dir holds session logs + the socket — keep other local
    // users out (`create_dir_all` inherits the umask).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.data_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let config = Config::load(&paths.config_path)?;
    let orch = Orchestrator::new(
        store,
        AgentRegistry::probed(&config),
        paths.data_dir.clone(),
    );
    Ok(Daemon::new(orch))
}

// ---------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------

/// Bind a unix listener at `path`, creating the parent directory and
/// cleaning up a stale socket file.
///
/// A stale socket (path exists, connect fails — e.g. a previous daemon
/// died without unlinking) is removed and the bind retried. When the
/// connect *succeeds* another daemon is live and `AddrInUse` is
/// returned unchanged.
pub async fn bind_unix_listener(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match UnixListener::bind(path) {
        Ok(listener) => tighten_socket_perms(path).map(|()| listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            match UnixStream::connect(path).await {
                Ok(_) => Err(e), // live daemon owns the socket
                Err(_) => {
                    // Nothing is listening. Only unlink when the path is
                    // actually a socket — `bind` also fails EADDRINUSE over
                    // a regular file, which must never be deleted.
                    #[cfg(unix)]
                    let is_socket = {
                        use std::os::unix::fs::FileTypeExt;
                        path.symlink_metadata()
                            .map(|m| m.file_type().is_socket())
                            .unwrap_or(false)
                    };
                    // Non-unix has no unix sockets; never auto-remove.
                    #[cfg(not(unix))]
                    let is_socket = false;
                    if !is_socket {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{} exists and is not a socket; refusing to remove it",
                                path.display()
                            ),
                        ));
                    }
                    std::fs::remove_file(path)?;
                    let listener = UnixListener::bind(path)?;
                    tighten_socket_perms(path).map(|()| listener)
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// `0600` on the socket file — the daemon's RPC surface can spawn
/// arbitrary commands, so other local users must not be able to connect.
/// No-op on non-unix (no unix sockets there anyway).
#[cfg(unix)]
fn tighten_socket_perms(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn tighten_socket_perms(_path: &Path) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// The running daemon: the shared orchestrator plus bookkeeping the RPC
/// surface needs (uptime, shutdown signalling).
pub struct Daemon {
    orchestrator: Arc<Orchestrator>,
    started: Instant,
    shutdown: Notify,
}

impl Daemon {
    pub fn new(orch: Orchestrator) -> Arc<Daemon> {
        Arc::new(Daemon {
            orchestrator: Arc::new(orch),
            started: Instant::now(),
            shutdown: Notify::new(),
        })
    }

    /// The shared orchestrator.
    pub fn orchestrator(&self) -> &Arc<Orchestrator> {
        &self.orchestrator
    }

    /// Signal the accept loop to stop (see module docs for ordering).
    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Dispatch one request. `server/*` methods are answered here;
    /// everything else delegates to [`dispatch`] on the orchestrator.
    ///
    /// `server/shutdown` only *produces the response* — the connection
    /// handler fires [`request_shutdown`](Self::request_shutdown) after
    /// the response has been flushed, so the ack can never be lost.
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            M_SERVER_STATUS => {
                parse_params::<rpc::ServerStatusParams>(params)?;
                let result = rpc::ServerStatusResult {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    uptime_secs: self.started.elapsed().as_secs(),
                    sessions: self.orchestrator.session_count(),
                };
                to_value(&result)
            }
            M_SERVER_SHUTDOWN => {
                parse_params::<rpc::ServerShutdownParams>(params)?;
                Ok(Value::Null)
            }
            _ => dispatch(&self.orchestrator, method, params).await,
        }
    }

    /// Accept connections until [`request_shutdown`](Self::request_shutdown)
    /// fires, then unlink the socket and return.
    ///
    /// The socket path is recovered from `listener` itself
    /// ([`UnixListener::local_addr`]), so callers cannot unlink the wrong
    /// file by passing a stale path.
    pub async fn serve(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        let socket_path = listener
            .local_addr()
            .ok()
            .and_then(|a| a.as_pathname().map(|p| p.to_path_buf()));
        let mut conns: Vec<JoinHandle<()>> = Vec::new();
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _addr)) => {
                            // Reap finished connection tasks so the vec
                            // does not grow with the daemon's lifetime.
                            conns.retain(|h| !h.is_finished());
                            conns.push(tokio::spawn(handle_conn(stream, self.clone())));
                        }
                        // Transient accept failure (EMFILE, a reset peer):
                        // log and keep serving rather than killing the daemon.
                        Err(e) => eprintln!("agentmux-server: accept failed: {e}"),
                    }
                }
                _ = self.shutdown.notified() => break,
            }
        }
        // `server/shutdown` already flushed its response (the handler
        // awaited the writer's ack before signalling). The remaining
        // connections are cut; spawned agent processes die via
        // `kill_on_drop` when the runtime tears down.
        for conn in conns {
            conn.abort();
        }
        if let Some(path) = socket_path {
            let _ = std::fs::remove_file(&path);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// One line queued for the connection's writer task; `ack` fires once the
/// bytes have been flushed to the socket.
struct Outbound {
    text: String,
    ack: Option<oneshot::Sender<()>>,
}

/// Serialize `msg` and queue it. Returns `false` when the queue is closed
/// (the connection is being torn down).
async fn enqueue<M: serde::Serialize>(
    tx: &mpsc::Sender<Outbound>,
    msg: &M,
    ack: Option<oneshot::Sender<()>>,
) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => tx.send(Outbound { text, ack }).await.is_ok(),
        Err(_) => false, // our envelope types cannot fail to serialize
    }
}

/// The connection's sole writer: drains the outbound queue, one line at a
/// time. Exits when the queue closes or the socket write fails.
async fn write_loop(mut writer: OwnedWriteHalf, mut rx: mpsc::Receiver<Outbound>) {
    while let Some(Outbound { mut text, ack }) = rx.recv().await {
        text.push('\n');
        // OwnedWriteHalf::shutdown/flush are no-ops for a socket;
        // `write_all` alone decides success.
        let ok = writer.write_all(text.as_bytes()).await.is_ok();
        if let Some(ack) = ack {
            let _ = ack.send(());
        }
        if !ok {
            return;
        }
    }
}

/// Outcome of reading one inbound line.
enum LineRead {
    /// A complete line (possibly unterminated at EOF).
    Line,
    /// The peer closed the connection.
    Eof,
    /// The line exceeded [`MAX_LINE_BYTES`]; its remainder was drained.
    Overlong,
}

/// Read one `\n`-terminated line bounded to [`MAX_LINE_BYTES`]. An
/// overlong line is drained to its newline so framing stays aligned.
///
/// Byte-oriented (`read_until`, not `read_line`): a line that isn't
/// valid UTF-8 comes back as bytes and is rejected downstream as a
/// `-32700` parse error instead of surfacing here as an I/O error that
/// would silently drop the connection.
async fn read_limited_line(
    reader: &mut BufReader<OwnedReadHalf>,
    out: &mut Vec<u8>,
) -> io::Result<LineRead> {
    // `take(cap).read_until` returns after `cap` bytes or the newline —
    // whichever first — so a line is overlong iff the buffer filled
    // without reaching '\n'.
    let cap = MAX_LINE_BYTES + 1;
    let n = (&mut *reader).take(cap).read_until(b'\n', out).await?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if n as u64 == cap && out.last() != Some(&b'\n') {
        // Drain the rest of the oversized line.
        let mut scratch = Vec::new();
        loop {
            scratch.clear();
            let m = (&mut *reader)
                .take(cap)
                .read_until(b'\n', &mut scratch)
                .await?;
            if m == 0 || scratch.last() == Some(&b'\n') {
                break;
            }
        }
        return Ok(LineRead::Overlong);
    }
    Ok(LineRead::Line)
}

/// Spawn the task forwarding orchestrator bus events to this connection
/// as `session/event` notifications.
fn spawn_event_forwarder(orch: Arc<Orchestrator>, tx: mpsc::Sender<Outbound>) -> JoinHandle<()> {
    let mut bus = orch.subscribe();
    tokio::spawn(async move {
        loop {
            let event = match bus.recv().await {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // See module docs: surface the gap as an Orchestrator
                    // note under the nil session id.
                    Event {
                        session_id: SessionId(uuid::Uuid::nil()),
                        seq: 0,
                        ts: Utc::now(),
                        kind: EventKind::Orchestrator(format!(
                            "subscriber lagged; dropped {n} event(s) \
                             (the per-session event log is authoritative)"
                        )),
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let params = match serde_json::to_value(&event) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !enqueue(&tx, &RpcNotification::new(N_SESSION_EVENT, params), None).await {
                break; // connection gone
            }
        }
    })
}

/// Serve one accepted connection until EOF, a write failure, or shutdown.
async fn handle_conn(stream: UnixStream, daemon: Arc<Daemon>) {
    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::channel::<Outbound>(OUTBOUND_QUEUE);
    let writer = tokio::spawn(write_loop(write_half, rx));
    let mut forwarder: Option<JoinHandle<()>> = None;

    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_limited_line(&mut reader, &mut line).await {
            Ok(LineRead::Line) => {}
            Ok(LineRead::Eof) => break,
            Ok(LineRead::Overlong) => {
                let resp = RpcResponse::err(
                    Value::Null,
                    RpcError::parse_error("message exceeds the 8 MiB line limit"),
                );
                if !enqueue(&tx, &resp, None).await {
                    break;
                }
                continue;
            }
            Err(_) => break, // socket-level read error — drop the conn
        }

        // `Value` first: malformed JSON is a parse error even when the
        // bytes happened to look request-shaped. `from_slice` also
        // rejects invalid UTF-8 → -32700, keeping the connection open.
        let value = match serde_json::from_slice::<Value>(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = RpcResponse::err(
                    Value::Null,
                    RpcError::parse_error(format!("invalid JSON: {e}")),
                );
                if !enqueue(&tx, &resp, None).await {
                    break;
                }
                continue;
            }
        };

        let id = value.get("id").cloned();
        match serde_json::from_value::<RpcRequest>(value.clone()) {
            Ok(req) => {
                let method = req.method.clone();
                let id = req.id.clone();
                match daemon.dispatch(&req.method, req.params).await {
                    Ok(result) => {
                        if method == M_SERVER_SHUTDOWN {
                            // Flush the ack before signalling shutdown —
                            // otherwise `serve` could cut this conn
                            // mid-response.
                            let (ack_tx, ack_rx) = oneshot::channel();
                            let resp = RpcResponse::ok(id, result);
                            if !enqueue(&tx, &resp, Some(ack_tx)).await {
                                break;
                            }
                            let _ = ack_rx.await;
                            daemon.request_shutdown();
                        } else {
                            let resp = RpcResponse::ok(id, result);
                            if !enqueue(&tx, &resp, None).await {
                                break;
                            }
                            // Arm the event forwarder only *after* the
                            // subscribe response is queued — the ack must
                            // precede any notification on the wire.
                            if method == M_SESSION_SUBSCRIBE && forwarder.is_none() {
                                forwarder = Some(spawn_event_forwarder(
                                    daemon.orchestrator.clone(),
                                    tx.clone(),
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        let resp = RpcResponse::err(id, e);
                        if !enqueue(&tx, &resp, None).await {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                if id.is_none() && value.is_object() && value.get("method").is_some() {
                    // A notification: dispatch fire-and-forget, never
                    // respond. Subscribe can't arm (it needs its response
                    // ordering), so nothing connection-scoped runs here.
                    let method = value["method"].as_str().unwrap_or_default().to_string();
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    if daemon.dispatch(&method, params).await.is_ok() && method == M_SERVER_SHUTDOWN
                    {
                        daemon.request_shutdown();
                    }
                } else {
                    let resp = RpcResponse::err(
                        id.unwrap_or(Value::Null),
                        RpcError::invalid_request(format!("not a request object: {e}")),
                    );
                    if !enqueue(&tx, &resp, None).await {
                        break;
                    }
                }
            }
        }
    }

    // Conn teardown: stop the forwarder (it also exits by itself once `tx`
    // closes), close the queue, let the writer drain what is already
    // queued (writes to a dead socket just error out).
    if let Some(f) = forwarder {
        f.abort();
    }
    drop(tx);
    let _ = writer.await;
}

// ---------------------------------------------------------------------------
// Method dispatch
// ---------------------------------------------------------------------------

/// Decode `params` as `T`; decode failures are `-32602` invalid params.
fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::invalid_params(format!("invalid params: {e}")))
}

/// Serialize a result value; serialization failure is `-32603`.
fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, RpcError> {
    serde_json::to_value(v).map_err(|e| RpcError::internal(format!("serialize result: {e}")))
}

/// Orchestrator-facing method dispatcher — every method except the
/// daemon-local `server/status` and `server/shutdown` (handled by
/// [`Daemon::dispatch`]) and the `session/subscribe` side effect (armed
/// by the connection handler after the ack).
///
/// `session/subscribe` is acked here with `null`; the caller upgrades the
/// connection afterwards.
pub async fn dispatch(orch: &Orchestrator, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        M_PROJECT_REGISTER => {
            let p: rpc::ProjectRegisterParams = parse_params(params)?;
            let project = orch
                .register_project(p.root_path, p.name)
                .map_err(RpcError::from)?;
            to_value(&rpc::ProjectRegisterResult { project })
        }
        M_PROJECT_LIST => {
            parse_params::<rpc::ProjectListParams>(params)?;
            to_value(&rpc::ProjectListResult {
                projects: orch.list_projects().map_err(RpcError::from)?,
            })
        }
        M_PROJECT_REMOVE => {
            let p: rpc::ProjectRemoveParams = parse_params(params)?;
            let removed = orch.remove_project(p.project_id).map_err(RpcError::from)?;
            to_value(&rpc::ProjectRemoveResult { removed })
        }
        M_WORKSPACE_CREATE => {
            let p: rpc::WorkspaceCreateParams = parse_params(params)?;
            let id = orch
                .create_workspace(p.project_id, &p.name, p.base.as_deref().unwrap_or("HEAD"))
                .await
                .map_err(RpcError::from)?;
            let workspace = orch
                .get_workspace(id)
                .map_err(RpcError::from)?
                .ok_or_else(|| RpcError::internal("created workspace vanished"))?;
            to_value(&rpc::WorkspaceCreateResult { workspace })
        }
        M_WORKSPACE_LIST => {
            let p: rpc::WorkspaceListParams = parse_params(params)?;
            to_value(&rpc::WorkspaceListResult {
                workspaces: orch.list_workspaces(p.project_id).map_err(RpcError::from)?,
            })
        }
        M_WORKSPACE_REMOVE => {
            let p: rpc::WorkspaceRemoveParams = parse_params(params)?;
            let removed = orch
                .remove_workspace(p.workspace_id)
                .map_err(RpcError::from)?;
            to_value(&rpc::WorkspaceRemoveResult { removed })
        }
        M_SESSION_CREATE => {
            let p: rpc::SessionCreateParams = parse_params(params)?;
            let id = orch
                .create_session(p.workspace_id, &p.agent_id, p.prompt)
                .await
                .map_err(RpcError::from)?;
            let session = orch
                .get_session(id)
                .map_err(RpcError::from)?
                .ok_or_else(|| RpcError::internal("created session vanished"))?;
            to_value(&rpc::SessionCreateResult { session })
        }
        M_SESSION_PROMPT => {
            let p: rpc::SessionPromptParams = parse_params(params)?;
            // The response completes when the turn does; the turn's
            // output streams to subscribers as `session/event`
            // notifications meanwhile. Awaiting keeps `session busy` /
            // dead-session errors on the wire.
            orch.prompt(p.session_id, p.text, p.references)
                .await
                .map_err(RpcError::from)?;
            Ok(Value::Null)
        }
        M_SESSION_CANCEL => {
            let p: rpc::SessionCancelParams = parse_params(params)?;
            orch.cancel(p.session_id).await.map_err(RpcError::from)?;
            Ok(Value::Null)
        }
        M_SESSION_KILL => {
            let p: rpc::SessionKillParams = parse_params(params)?;
            orch.kill(p.session_id).await.map_err(RpcError::from)?;
            Ok(Value::Null)
        }
        M_SESSION_LIST => {
            let p: rpc::SessionListParams = parse_params(params)?;
            to_value(&rpc::SessionListResult {
                sessions: orch.list_sessions(p.workspace_id).map_err(RpcError::from)?,
            })
        }
        M_SESSION_RESUME => {
            let p: rpc::SessionResumeParams = parse_params(params)?;
            orch.resume(p.session_id).await.map_err(RpcError::from)?;
            Ok(Value::Null)
        }
        M_SESSION_SUBSCRIBE => {
            parse_params::<rpc::SessionSubscribeParams>(params)?;
            // The connection handler arms the event forwarder once this
            // `null` ack is queued.
            Ok(Value::Null)
        }
        M_AGENT_LIST => {
            parse_params::<rpc::AgentListParams>(params)?;
            to_value(&rpc::AgentListResult {
                agents: orch.list_agents(),
            })
        }
        M_AGENT_REGISTER => {
            let p: rpc::AgentRegisterParams = parse_params(params)?;
            let agent = orch.register_agent(p.profile).map_err(RpcError::from)?;
            to_value(&rpc::AgentRegisterResult { agent })
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}
