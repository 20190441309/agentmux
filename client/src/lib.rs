//! `agentmux-client` — the SDK for talking to a running `agentmux-server`
//! daemon, plus the auto-start path that spawns one when none is
//! listening.
//!
//! # Wire protocol
//!
//! Newline-delimited JSON-RPC 2.0 over the daemon's unix socket — the
//! mirror image of `agentmux_server::rpc_server`: one request per `\n`
//! line, one response per request, `session/event` notifications pushed
//! to subscribed connections. All method names and params/result types
//! come from `agentmux_core::rpc`, so the wire contract cannot drift.
//!
//! # Responses vs. notifications
//!
//! A single reader task owns the socket's read half. Responses are routed
//! back to the [`call`](DaemonClient::call) that sent the request via a
//! per-id oneshot; `session/event` notifications are fanned into a
//! [`tokio::sync::broadcast`] channel surfaced as [`DaemonClient::events`]
//! / [`DaemonClient::subscribe_events`]. Because [`call`] takes `&mut
//! self`, at most one request is in flight per connection — matching the
//! server's sequential per-connection dispatch. To `session/cancel` a
//! hung `prompt`, open a second connection ([`DaemonClient::connect`]).
//!
//! # Auto-start
//!
//! [`DaemonClient::connect`] (and [`connect_to`](DaemonClient::connect_to))
//! first run [`ensure_daemon`](DaemonClient::ensure_daemon): probe the
//! socket, and on failure spawn `agentmux-server --daemon` — the server's
//! own detach path, which re-checks for a live socket and polls readiness
//! itself — then keep polling until the deadline. A connectable socket at
//! the deadline means success, whoever spawned it.
//!
//! The server binary is resolved as: `$AGENTMUX_SERVER_BIN` → a sibling
//! of `current_exe` (and its parent dir, covering `target/…/deps/` test
//! binaries) → `PATH`.

use std::collections::HashMap;
use std::env;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use agentmux_core::rpc::{
    self, RpcRequest, M_AGENT_LIST, M_AGENT_REGISTER, M_PROJECT_LIST, M_PROJECT_REGISTER,
    M_PROJECT_REMOVE, M_SERVER_SHUTDOWN, M_SERVER_STATUS, M_SESSION_CANCEL, M_SESSION_CREATE,
    M_SESSION_KILL, M_SESSION_LIST, M_SESSION_PERMISSION, M_SESSION_PROMPT, M_SESSION_RESUME,
    M_SESSION_SUBSCRIBE, M_WORKSPACE_CREATE, M_WORKSPACE_LIST, M_WORKSPACE_REMOVE, N_SESSION_EVENT,
};
use agentmux_server::ServerPaths;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

pub use agentmux_core::rpc::ServerStatusResult;
pub use agentmux_core::{
    AgentId, AgentProfile, Event, EventKind, PermissionDecision, Project, ProjectId, Session,
    SessionId, SessionRef, SessionState, Workspace, WorkspaceId,
};

/// Largest inbound line the client will read; mirrors the server's own
/// 8 MiB limit so neither peer can be made to buffer arbitrarily.
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;

/// Depth of the per-connection `session/event` broadcast channel. Slow
/// receivers get a synthetic lag notice (see [`subscribe_events`]).
const EVENT_CHANNEL: usize = 256;

/// How long `ensure_daemon` waits for a spawned daemon to accept
/// connections (spec/brief: 5 s).
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval between connect probes.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Override for locating the `agentmux-server` binary — first choice in
/// [`server_binary`], handy for tests and custom installs.
const ENV_SERVER_BIN: &str = "AGENTMUX_SERVER_BIN";
/// The daemon's executable name, for sibling/`PATH` resolution.
const SERVER_BIN_NAME: &str = "agentmux-server";

/// Everything [`DaemonClient`] can fail with.
#[derive(Debug)]
pub enum ClientError {
    /// The daemon answered our request with a JSON-RPC `error` object —
    /// e.g. `-32601` unknown method or `-32603` orchestrator failure.
    Rpc {
        /// `RpcError::code` (e.g. [`agentmux_core::RpcError::METHOD_NOT_FOUND`]).
        code: i64,
        /// `RpcError::message`.
        message: String,
    },
    /// Socket connect/read/write, daemon spawn, or structurally malformed
    /// traffic from the daemon (invalid JSON, a response carrying neither
    /// `result` nor `error`).
    Transport(io::Error),
    /// An operation exceeded its deadline: the `ensure_daemon` start
    /// timeout, or a per-request timeout set with
    /// [`DaemonClient::set_request_timeout`].
    Timeout(Duration),
}

impl ClientError {
    /// A transport error carrying only a message (spawn failures,
    /// protocol violations) — `io::ErrorKind::Other`.
    fn transport(message: impl fmt::Display) -> ClientError {
        ClientError::Transport(io::Error::other(message.to_string()))
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            ClientError::Transport(e) => write!(f, "transport error: {e}"),
            ClientError::Timeout(d) => write!(f, "operation timed out after {d:?}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> ClientError {
        ClientError::Transport(e)
    }
}

impl From<rpc::RpcError> for ClientError {
    fn from(e: rpc::RpcError) -> ClientError {
        ClientError::Rpc {
            code: e.code,
            message: e.message,
        }
    }
}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, ClientError>;

/// Requests awaiting their response, keyed by numeric request id.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// DaemonClient
// ---------------------------------------------------------------------------

/// One client connection to the agentmux daemon.
///
/// Created by [`connect`](DaemonClient::connect) — which auto-spawns the
/// daemon when the socket is dead — or
/// [`connect_existing`](DaemonClient::connect_existing) for a strict
/// no-spawn connection. Dropping the client closes the socket and aborts
/// its reader task.
pub struct DaemonClient {
    writer: OwnedWriteHalf,
    reader: JoinHandle<()>,
    pending: PendingMap,
    /// `session/event` notifications fan into this broadcast channel.
    events: broadcast::Sender<Event>,
    /// Response-routing ids: allocated monotonically (`AtomicU64` keeps
    /// `call` sound even if a future API relaxes `&mut self`).
    next_id: AtomicU64,
    /// Set by the reader task (under the `pending` lock) when the socket
    /// hits EOF, a read error, or unrecoverable framing — so a `call`
    /// racing teardown fails fast instead of hanging on a dead socket.
    closed: Arc<AtomicBool>,
    socket_path: PathBuf,
    request_timeout: Option<Duration>,
}

impl DaemonClient {
    /// The socket the daemon would listen on given the current env:
    /// `$AGENTMUX_SOCK` → `<data-dir>/agentmux.sock` (same resolution the
    /// server uses via [`ServerPaths`]).
    pub fn default_socket_path() -> PathBuf {
        ServerPaths::resolve(None, None, None).socket_path
    }

    /// Connect to the default socket, auto-spawning the daemon first if
    /// it isn't reachable ([`ensure_daemon`](Self::ensure_daemon)).
    pub async fn connect() -> Result<DaemonClient> {
        Self::connect_to(Self::default_socket_path()).await
    }

    /// Restart the default daemon, then return a connection to it.
    /// Running agent connections end; persisted tasks can be resumed.
    pub async fn restart() -> Result<DaemonClient> {
        Self::restart_to(Self::default_socket_path()).await
    }

    /// Gracefully stop a daemon and wait for its socket to be removed before
    /// starting its replacement. If absent, simply start it. Newer daemons
    /// report their effective paths; older ones fall back to env/defaults.
    pub async fn restart_to(socket_path: impl AsRef<Path>) -> Result<DaemonClient> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let mut paths = ServerPaths::resolve(Some(socket_path.clone()), None, None);
        match Self::connect_existing(&socket_path).await {
            Ok(mut old) => {
                old.set_request_timeout(Some(DAEMON_START_TIMEOUT));
                let status = old.server_status().await?;
                if let Some(data_dir) = status.data_dir {
                    paths.data_dir = data_dir;
                }
                if let Some(config_path) = status.config_path {
                    paths.config_path = config_path;
                }
                old.shutdown().await?;
                let deadline = Instant::now() + DAEMON_START_TIMEOUT;
                // Waiting just for the ack can reconnect to the old listener.
                while socket_path.try_exists()? {
                    if Instant::now() >= deadline {
                        return Err(ClientError::Timeout(DAEMON_START_TIMEOUT));
                    }
                    tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
                }
            }
            Err(ClientError::Transport(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(e) => return Err(e),
        }
        tokio::task::spawn_blocking(move || Self::ensure_daemon_with_paths(&paths))
            .await
            .map_err(|e| ClientError::transport(format!("restart daemon task failed: {e}")))??;
        Self::connect_existing(socket_path).await
    }

    /// Connect to `socket_path`, auto-spawning the daemon if needed.
    ///
    /// When spawned, the data dir / config are resolved from the current
    /// environment and defaults, then passed explicitly with the socket.
    pub async fn connect_to(socket_path: impl AsRef<Path>) -> Result<DaemonClient> {
        let socket_path = socket_path.as_ref().to_path_buf();
        {
            let path = socket_path.clone();
            // Blocking probe+spawn+poll off the async reactor.
            tokio::task::spawn_blocking(move || Self::ensure_daemon_at(&path))
                .await
                .map_err(|e| ClientError::transport(format!("ensure_daemon task failed: {e}")))??;
        }
        Self::connect_existing(socket_path).await
    }

    /// Connect to `socket_path` *without* auto-starting — a dead socket
    /// is a plain `Transport` error. Used by [`connect_to`](Self::connect_to)
    /// after [`ensure_daemon`](Self::ensure_daemon) has run.
    pub async fn connect_existing(socket_path: impl AsRef<Path>) -> Result<DaemonClient> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let stream = UnixStream::connect(&socket_path).await.map_err(|e| {
            ClientError::Transport(io::Error::new(
                e.kind(),
                format!("connect {}: {e}", socket_path.display()),
            ))
        })?;
        let (read_half, writer) = stream.into_split();
        let pending = PendingMap::default();
        let (events, _) = broadcast::channel(EVENT_CHANNEL);
        let closed = Arc::new(AtomicBool::new(false));
        let reader = tokio::spawn(reader_loop(
            read_half,
            pending.clone(),
            events.clone(),
            closed.clone(),
        ));
        Ok(DaemonClient {
            writer,
            reader,
            pending,
            events,
            next_id: AtomicU64::new(1),
            closed,
            socket_path,
            request_timeout: None,
        })
    }

    /// The socket this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Whether the connection's reader task has torn down (EOF, socket
    /// error, or unrecoverable framing). Liveness signal for UIs: the
    /// [`subscribe_events`](Self::subscribe_events) stream pends forever
    /// once the daemon is gone — the broadcast sender lives on `self` —
    /// so consumers must poll this to detect disconnects.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Per-request deadline for subsequent [`call`]s. `None` (the
    /// default) waits forever — required for `session/prompt`, whose
    /// response only arrives once the whole turn has run.
    pub fn set_request_timeout(&mut self, timeout: Option<Duration>) {
        self.request_timeout = timeout;
    }

    /// Ensure a daemon is reachable on the **default** socket,
    /// auto-spawning `agentmux-server --daemon` if not.
    ///
    /// Blocking (spawn + readiness poll); [`connect`](Self::connect)
    /// invokes it via `spawn_blocking`. See module docs for the binary
    /// resolution order.
    pub fn ensure_daemon() -> Result<()> {
        Self::ensure_daemon_at(&Self::default_socket_path())
    }

    /// [`ensure_daemon`](Self::ensure_daemon) against an explicit socket.
    ///
    /// A live socket short-circuits. Otherwise `agentmux-server --daemon
    /// --socket <path>` is spawned and awaited — it prints "already
    /// running" when a concurrent spawn won the race — then the socket is
    /// polled until [`DAEMON_START_TIMEOUT`]; connectable at the deadline
    /// is success regardless of which process owns it.
    fn ensure_daemon_at(socket_path: &Path) -> Result<()> {
        Self::ensure_daemon_with_paths(&ServerPaths::resolve(
            Some(socket_path.to_path_buf()),
            None,
            None,
        ))
    }

    fn ensure_daemon_with_paths(paths: &ServerPaths) -> Result<()> {
        let socket_path = &paths.socket_path;
        let deadline = Instant::now() + DAEMON_START_TIMEOUT;
        if socket_accepts(socket_path) {
            return Ok(());
        }
        let server = server_binary();
        let output = Command::new(&server)
            .arg("--daemon")
            .arg("--socket")
            .arg(socket_path)
            .arg("--data-dir")
            .arg(&paths.data_dir)
            .arg("--config")
            .arg(&paths.config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                ClientError::transport(format!(
                    "failed to spawn {} --daemon: {e}",
                    server.display()
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ClientError::transport(format!(
                "{} --daemon failed ({}): {}",
                server.display(),
                output.status,
                stderr.trim()
            )));
        }
        // `--daemon` only exits 0 once the socket accepts (or a live one
        // was found), so this loop normally succeeds on the first probe;
        // the poll is belt-and-suspenders per the connectable-at-deadline
        // semantics.
        loop {
            if socket_accepts(socket_path) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClientError::Timeout(DAEMON_START_TIMEOUT));
            }
            std::thread::sleep(remaining.min(DAEMON_POLL_INTERVAL));
        }
    }

    /// Send one request and await its response, routed by id.
    ///
    /// `session/event` notifications interleaved on the wire do not
    /// disturb the response routing — the reader task fans them into
    /// [`events`](Self::events) while the matching response is still
    /// delivered here.
    ///
    /// Errors: `Transport` on I/O or protocol trouble, `Timeout` if a
    /// request timeout is configured and elapses, `Rpc` for a daemon-side
    /// `error` object.
    pub async fn call(&mut self, method: &str, params: impl Serialize) -> Result<Value> {
        let params = serde_json::to_value(params).map_err(|e| {
            ClientError::transport(format!("failed to serialize params for {method}: {e}"))
        })?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            // Register under the same lock the reader uses to mark
            // teardown: either we see `closed` and bail, or the reader's
            // exit drain will resolve this entry — no lost wakeups.
            let mut pending = lock(&self.pending);
            if self.closed.load(Ordering::SeqCst) {
                return Err(ClientError::transport(format!(
                    "connection to {} is closed",
                    self.socket_path.display()
                )));
            }
            pending.insert(id, tx);
        }
        let request = RpcRequest::new(method, params, Value::from(id));
        let mut line = match serde_json::to_vec(&request) {
            Ok(line) => line,
            Err(e) => {
                lock(&self.pending).remove(&id);
                return Err(ClientError::transport(format!(
                    "failed to serialize {method} request: {e}"
                )));
            }
        };
        line.push(b'\n');
        if let Err(e) = self.writer.write_all(&line).await {
            lock(&self.pending).remove(&id);
            return Err(ClientError::Transport(io::Error::new(
                e.kind(),
                format!("write to {}: {e}", self.socket_path.display()),
            )));
        }
        let response = match self.request_timeout {
            Some(d) => match tokio::time::timeout(d, rx).await {
                Ok(r) => r,
                Err(_) => {
                    lock(&self.pending).remove(&id);
                    return Err(ClientError::Timeout(d));
                }
            },
            None => rx.await,
        };
        match response {
            Ok(result) => result,
            Err(_) => Err(ClientError::transport(format!(
                "connection to {} closed while awaiting the {method} response",
                self.socket_path.display()
            ))),
        }
    }

    /// The connection's `session/event` broadcast channel: each receiver
    /// sees every event pushed *after* it subscribes. Events only flow
    /// once [`subscribe_events`](Self::subscribe_events) (or a raw
    /// `session/subscribe` [`call`]) has armed the daemon's forwarder.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Send `session/subscribe` and return this connection's events as a
    /// `Stream`.
    ///
    /// The server acks the subscribe request before arming its forwarder,
    /// so when this resolves the stream is live. The broadcast receiver is
    /// created *before* the request is sent — the reader task fans
    /// notifications into a channel with zero receivers only between the
    /// ack and receiver creation, and this ordering removes that gap.
    /// A receiver that falls more than [`EVENT_CHANNEL`] events behind
    /// gets a synthetic [`EventKind::Orchestrator`] note under the nil
    /// session id — the same lag convention the server uses.
    pub async fn subscribe_events(&mut self) -> Result<impl Stream<Item = Event> + Send + 'static> {
        let rx = self.events();
        self.call(M_SESSION_SUBSCRIBE, ()).await?;
        Ok(event_stream(rx))
    }

    /// `project/register` → the created [`Project`].
    pub async fn register_project(
        &mut self,
        root_path: impl Into<PathBuf>,
        name: Option<String>,
    ) -> Result<Project> {
        let params = rpc::ProjectRegisterParams {
            root_path: root_path.into(),
            name,
        };
        Ok(self
            .call_result::<rpc::ProjectRegisterResult>(M_PROJECT_REGISTER, params)
            .await?
            .project)
    }

    /// `project/list` → all registered projects.
    pub async fn list_projects(&mut self) -> Result<Vec<Project>> {
        Ok(self
            .call_result::<rpc::ProjectListResult>(M_PROJECT_LIST, ())
            .await?
            .projects)
    }

    /// `project/remove` → `false` when no such project existed.
    pub async fn remove_project(&mut self, project_id: ProjectId) -> Result<bool> {
        Ok(self
            .call_result::<rpc::ProjectRemoveResult>(
                M_PROJECT_REMOVE,
                rpc::ProjectRemoveParams { project_id },
            )
            .await?
            .removed)
    }

    /// `workspace/create` → the created [`Workspace`]. `base` is the git
    /// ref the worktree branches off (`None` = the repo's default).
    pub async fn create_workspace(
        &mut self,
        project_id: ProjectId,
        name: impl Into<String>,
        base: Option<&str>,
    ) -> Result<Workspace> {
        let params = rpc::WorkspaceCreateParams {
            project_id,
            name: name.into(),
            base: base.map(str::to_string),
        };
        Ok(self
            .call_result::<rpc::WorkspaceCreateResult>(M_WORKSPACE_CREATE, params)
            .await?
            .workspace)
    }

    pub async fn open_workspace(&mut self, project_id: ProjectId) -> Result<Workspace> {
        Ok(self
            .call_result::<rpc::WorkspaceCreateResult>(
                rpc::M_WORKSPACE_OPEN,
                rpc::WorkspaceOpenParams { project_id },
            )
            .await?
            .workspace)
    }
    pub async fn native_list(&mut self, session_id: SessionId) -> Result<rpc::NativeListResult> {
        self.call_result(rpc::M_NATIVE_LIST, rpc::NativeListParams { session_id })
            .await
    }
    pub async fn native_open(&mut self, params: rpc::NativeOpenParams) -> Result<Session> {
        Ok(self
            .call_result::<rpc::SessionCreateResult>(rpc::M_NATIVE_OPEN, params)
            .await?
            .session)
    }
    pub async fn terminal_attach(
        &mut self,
        session_id: SessionId,
        rows: u16,
        cols: u16,
    ) -> Result<agentmux_core::terminal::TerminalAttached> {
        self.call_result(
            rpc::M_TERMINAL_ATTACH,
            rpc::TerminalAttachParams {
                session_id,
                rows,
                cols,
            },
        )
        .await
    }
    pub async fn terminal_read(
        &mut self,
        session_id: SessionId,
        token: &str,
        after: u64,
    ) -> Result<agentmux_core::terminal::TerminalFrame> {
        self.call_result(
            rpc::M_TERMINAL_READ,
            rpc::TerminalReadParams {
                session_id,
                token: token.into(),
                after,
            },
        )
        .await
    }
    pub async fn terminal_input(
        &mut self,
        session_id: SessionId,
        token: &str,
        data: Vec<u8>,
    ) -> Result<()> {
        self.call_result(
            rpc::M_TERMINAL_INPUT,
            rpc::TerminalInputParams {
                session_id,
                token: token.into(),
                data,
            },
        )
        .await
    }
    pub async fn terminal_resize(
        &mut self,
        session_id: SessionId,
        token: &str,
        rows: u16,
        cols: u16,
    ) -> Result<()> {
        self.call_result(
            rpc::M_TERMINAL_RESIZE,
            rpc::TerminalResizeParams {
                session_id,
                token: token.into(),
                rows,
                cols,
            },
        )
        .await
    }
    pub async fn terminal_detach(&mut self, session_id: SessionId, token: &str) -> Result<()> {
        self.call_result(
            rpc::M_TERMINAL_DETACH,
            rpc::TerminalDetachParams {
                session_id,
                token: token.into(),
            },
        )
        .await
    }

    /// `workspace/list` → a project's workspaces.
    pub async fn list_workspaces(&mut self, project_id: ProjectId) -> Result<Vec<Workspace>> {
        Ok(self
            .call_result::<rpc::WorkspaceListResult>(
                M_WORKSPACE_LIST,
                rpc::WorkspaceListParams { project_id },
            )
            .await?
            .workspaces)
    }

    /// `workspace/remove` → `false` when no such workspace existed.
    /// Refused while any of its sessions is live — kill them first.
    pub async fn remove_workspace(&mut self, workspace_id: WorkspaceId) -> Result<bool> {
        Ok(self
            .call_result::<rpc::WorkspaceRemoveResult>(
                M_WORKSPACE_REMOVE,
                rpc::WorkspaceRemoveParams { workspace_id },
            )
            .await?
            .removed)
    }

    /// `session/create` → the created [`Session`]. `prompt`, when given,
    /// is sent as the first turn once the session is `Ready`.
    /// Adapter setup failures return the persisted session in `Error`, so
    /// callers can resume it instead of silently creating a duplicate.
    pub async fn create_session(
        &mut self,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        prompt: Option<String>,
    ) -> Result<Session> {
        let params = rpc::SessionCreateParams {
            workspace_id,
            agent_id,
            prompt,
        };
        Ok(self
            .call_result::<rpc::SessionCreateResult>(M_SESSION_CREATE, params)
            .await?
            .session)
    }

    pub async fn history(
        &mut self,
        session_id: SessionId,
        before_seq: Option<u64>,
    ) -> Result<rpc::SessionHistoryResult> {
        let mut page: rpc::SessionHistoryResult = self
            .call_result(
                rpc::M_SESSION_HISTORY,
                rpc::SessionHistoryParams {
                    session_id,
                    before_seq,
                },
            )
            .await?;
        for seq in std::mem::take(&mut page.event_refs) {
            page.events
                .push(self.read_history_event(session_id, seq).await?);
        }
        for seq in std::mem::take(&mut page.pending_permission_refs) {
            let event = if let Some(event) = page.events.iter().find(|e| e.seq == seq) {
                event.clone()
            } else {
                self.read_history_event(session_id, seq).await?
            };
            page.pending_permissions.push(event);
        }
        if let Some(seq) = page.available_commands_ref.take() {
            page.available_commands = Some(
                if let Some(event) = page.events.iter().find(|e| e.seq == seq) {
                    event.clone()
                } else {
                    self.read_history_event(session_id, seq).await?
                },
            );
        }
        page.events.sort_by_key(|e| e.seq);
        page.pending_permissions.sort_by_key(|e| e.seq);
        Ok(page)
    }

    pub async fn set_session_title(
        &mut self,
        session_id: SessionId,
        title: String,
    ) -> Result<Event> {
        self.call_result(
            rpc::M_SESSION_TITLE,
            rpc::SessionTitleParams { session_id, title },
        )
        .await
    }

    pub async fn read_history_event(&mut self, session_id: SessionId, seq: u64) -> Result<Event> {
        let mut json = String::new();
        loop {
            let offset = json.len();
            let chunk: rpc::SessionEventReadResult = self
                .call_result(
                    rpc::M_SESSION_EVENT_READ,
                    rpc::SessionEventReadParams {
                        session_id,
                        seq,
                        offset,
                    },
                )
                .await?;
            if chunk.data.is_empty() {
                return Err(ClientError::transport("empty history chunk"));
            }
            json.push_str(&chunk.data);
            match chunk.next_offset {
                Some(next) if next == json.len() && next > offset => {}
                Some(_) => return Err(ClientError::transport("invalid history chunk offset")),
                None => break,
            }
        }
        let event: Event = serde_json::from_str(&json).map_err(ClientError::transport)?;
        if event.session_id != session_id || event.seq != seq {
            return Err(ClientError::transport(
                "history chunk event identity mismatch",
            ));
        }
        Ok(event)
    }

    pub async fn file_diff(&mut self, workspace_id: WorkspaceId, path: String) -> Result<String> {
        Ok(self
            .file_diff_info(rpc::WorkspaceDiffParams {
                workspace_id,
                path,
                path_bytes: None,
                old_path: None,
                old_path_bytes: None,
                scope: rpc::DiffScope::Head,
            })
            .await?
            .text)
    }

    pub async fn file_diff_info(
        &mut self,
        params: rpc::WorkspaceDiffParams,
    ) -> Result<rpc::WorkspaceDiffResult> {
        self.call_result(rpc::M_WORKSPACE_DIFF, params).await
    }

    pub async fn workspace_changes(
        &mut self,
        workspace_id: WorkspaceId,
    ) -> Result<rpc::WorkspaceChangesResult> {
        self.call_result(
            rpc::M_WORKSPACE_CHANGES,
            rpc::WorkspaceChangesParams { workspace_id },
        )
        .await
    }

    pub async fn workspace_context(
        &mut self,
        workspace_id: WorkspaceId,
    ) -> Result<rpc::WorkspaceContextResult> {
        self.call_result(
            rpc::M_WORKSPACE_CONTEXT,
            rpc::WorkspaceChangesParams { workspace_id },
        )
        .await
    }

    pub async fn save_workspace_context(
        &mut self,
        params: rpc::WorkspaceContextSaveParams,
    ) -> Result<rpc::WorkspaceContextResult> {
        self.call_result(rpc::M_WORKSPACE_CONTEXT_SAVE, params)
            .await
    }

    /// `session/prompt` → resolves when the turn completes; the turn's
    /// output streams via [`events`](Self::events) meanwhile.
    /// `references` hands cross-session context to the turn
    /// (`vec![]` for none).
    pub async fn prompt(
        &mut self,
        session_id: SessionId,
        text: impl Into<String>,
        references: Vec<SessionRef>,
    ) -> Result<()> {
        self.call_unit(
            M_SESSION_PROMPT,
            rpc::SessionPromptParams {
                session_id,
                text: text.into(),
                references,
            },
        )
        .await
    }

    /// `session/cancel` → cancel the in-flight prompt turn. Needs a
    /// *second* connection while a `prompt` call on this one is still
    /// awaiting (the daemon dispatches per connection sequentially).
    pub async fn cancel(&mut self, session_id: SessionId) -> Result<()> {
        self.call_unit(M_SESSION_CANCEL, rpc::SessionCancelParams { session_id })
            .await
    }

    /// `session/permission` → answer a parked agent permission request.
    /// `request_id` is the id carried by the `PermissionRequest` event;
    /// `outcome` maps onto the option kinds the agent offered. Like
    /// `cancel`, this needs a second connection while a `prompt` call
    /// is still awaiting on this one.
    pub async fn respond_permission(
        &mut self,
        session_id: SessionId,
        request_id: &str,
        outcome: PermissionDecision,
    ) -> Result<()> {
        self.call_unit(
            M_SESSION_PERMISSION,
            rpc::SessionPermissionParams {
                session_id,
                request_id: request_id.to_string(),
                outcome,
            },
        )
        .await
    }

    /// `session/kill` → kill the session's agent process.
    pub async fn kill(&mut self, session_id: SessionId) -> Result<()> {
        self.call_unit(M_SESSION_KILL, rpc::SessionKillParams { session_id })
            .await
    }

    /// `session/list` → a workspace's sessions.
    pub async fn list_sessions(&mut self, workspace_id: WorkspaceId) -> Result<Vec<Session>> {
        Ok(self
            .call_result::<rpc::SessionListResult>(
                M_SESSION_LIST,
                rpc::SessionListParams { workspace_id },
            )
            .await?
            .sessions)
    }

    /// Pi command discovery and controls on an existing adapter connection.
    pub async fn pi_command(
        &mut self,
        session_id: SessionId,
        command: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.call_result(
            rpc::M_SESSION_PI,
            rpc::SessionPiParams {
                session_id,
                command,
            },
        )
        .await
    }

    /// `session/resume` → resume a `Done`/`Error` session.
    pub async fn resume(&mut self, session_id: SessionId) -> Result<()> {
        self.call_unit(M_SESSION_RESUME, rpc::SessionResumeParams { session_id })
            .await
    }

    /// `agent/list` → configured agents, availability-probed server-side.
    pub async fn list_agents(&mut self) -> Result<Vec<AgentProfile>> {
        Ok(self
            .call_result::<rpc::AgentListResult>(M_AGENT_LIST, ())
            .await?
            .agents)
    }

    /// `agent/register` → register/update a profile; the daemon fills in
    /// `available` by probing.
    pub async fn register_agent(&mut self, profile: AgentProfile) -> Result<AgentProfile> {
        Ok(self
            .call_result::<rpc::AgentRegisterResult>(
                M_AGENT_REGISTER,
                rpc::AgentRegisterParams { profile },
            )
            .await?
            .agent)
    }

    /// `server/status` → daemon version/uptime/session count.
    pub async fn server_status(&mut self) -> Result<ServerStatusResult> {
        self.call_result::<ServerStatusResult>(M_SERVER_STATUS, ())
            .await
    }

    /// `server/shutdown` → ask the daemon to exit (acks first, then the
    /// socket closes).
    pub async fn shutdown(&mut self) -> Result<()> {
        self.call_unit(M_SERVER_SHUTDOWN, ()).await
    }

    /// [`call`] + decode the result `Value` into `R`; a mismatched shape
    /// is a `Transport` error (protocol violation).
    async fn call_result<R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R> {
        let value = self.call(method, params).await?;
        serde_json::from_value(value)
            .map_err(|e| ClientError::transport(format!("malformed {method} result: {e}")))
    }

    /// [`call`] discarding the (null) result — for ack-only methods.
    async fn call_unit(&mut self, method: &str, params: impl Serialize) -> Result<()> {
        self.call(method, params).await.map(|_| ())
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

// ---------------------------------------------------------------------------
// Reader task — the connection's sole inbound pump
// ---------------------------------------------------------------------------

/// Outcome of reading one inbound line (mirrors the server's framing).
enum LineRead {
    /// A complete `\n`-terminated line (or unterminated tail at EOF).
    Line,
    /// The daemon closed the connection.
    Eof,
    /// The line exceeded [`MAX_LINE_BYTES`]; its remainder was drained.
    Overlong,
}

/// Read one `\n`-terminated line bounded to [`MAX_LINE_BYTES`]; an
/// overlong line is drained to its newline so framing stays aligned.
async fn read_bounded_line(
    reader: &mut BufReader<OwnedReadHalf>,
    out: &mut Vec<u8>,
) -> io::Result<LineRead> {
    let cap = MAX_LINE_BYTES + 1;
    let n = (&mut *reader).take(cap).read_until(b'\n', out).await?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if n as u64 == cap && out.last() != Some(&b'\n') {
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

/// Route one parsed inbound message: a response goes to its pending
/// request, a `session/event` notification to the broadcast channel.
/// Anything else (response with a foreign id, unknown notification) is
/// ignored.
fn route_message(msg: Value, pending: &PendingMap, events: &broadcast::Sender<Event>) {
    match msg.get("id") {
        // A notification (no `id`): only `session/event` exists today.
        None => {
            if msg.get("method").and_then(Value::as_str) == Some(N_SESSION_EVENT) {
                if let Ok(event) = serde_json::from_value::<Event>(msg["params"].clone()) {
                    // No receivers yet is fine — receivers see events sent
                    // after they subscribe.
                    let _ = events.send(event);
                }
            }
        }
        // A response. Numeric ids are the ones we allocate; an
        // unrouteable response (e.g. `id: null` for a parse error) is
        // delivered to the sole in-flight request — with `&mut self`
        // calls at most one exists — so its `call` resolves instead of
        // hanging.
        Some(id) => {
            let tx = match id.as_u64() {
                Some(n) => lock(pending).remove(&n),
                None => {
                    let mut map = lock(pending);
                    if map.len() == 1 {
                        map.drain().next().map(|(_, tx)| tx)
                    } else {
                        None
                    }
                }
            };
            if let Some(tx) = tx {
                let _ = tx.send(response_result(msg));
            }
        }
    }
}

/// Turn a response message into the `Result<Value>` delivered to its
/// `call`: `error` → [`ClientError::Rpc`], `result` → the value,
/// neither → a `Transport` protocol error.
fn response_result(msg: Value) -> Result<Value> {
    if let Some(error) = msg.get("error").filter(|e| !e.is_null()) {
        return match serde_json::from_value::<rpc::RpcError>(error.clone()) {
            Ok(e) => Err(e.into()),
            Err(_) => Err(ClientError::transport(
                "daemon sent a malformed error object",
            )),
        };
    }
    match msg.get("result") {
        Some(result) => Ok(result.clone()),
        None => Err(ClientError::transport(
            "daemon response carries neither result nor error",
        )),
    }
}

/// Mark the connection dead and fail every pending request — under one
/// lock hold so a `call` that already inserted is always resolved.
fn fail_pending(pending: &PendingMap, closed: &AtomicBool, message: &str) {
    let mut map = lock(pending);
    closed.store(true, Ordering::SeqCst);
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(ClientError::transport(message)));
    }
}

/// The connection's read loop: route responses and notifications until
/// EOF, a socket error, or unrecoverable framing — which the daemon never
/// produces, so those are treated as fatal: every pending `call` resolves
/// with `Transport` instead of hanging on a lost response.
async fn reader_loop(
    read_half: OwnedReadHalf,
    pending: PendingMap,
    events: broadcast::Sender<Event>,
    closed: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(read_half);
    let mut line = Vec::new();
    let fatal: String = loop {
        line.clear();
        match read_bounded_line(&mut reader, &mut line).await {
            Ok(LineRead::Line) => {}
            Ok(LineRead::Eof) => break "daemon closed the connection".to_string(),
            Ok(LineRead::Overlong) => {
                break format!("daemon sent a line over {MAX_LINE_BYTES} bytes")
            }
            Err(e) => break format!("read from daemon: {e}"),
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(msg) => route_message(msg, &pending, &events),
            Err(e) => break format!("daemon sent invalid JSON: {e}"),
        }
    };
    fail_pending(&pending, &closed, &fatal);
}

// ---------------------------------------------------------------------------
// Event stream adaptation
// ---------------------------------------------------------------------------

/// A `Stream<Item = Event>` over an `events()` receiver: broadcast lag
/// surfaces as a synthetic [`EventKind::Orchestrator`] note under the nil
/// session id — the same convention the server uses for its own lag.
fn event_stream(rx: broadcast::Receiver<Event>) -> impl Stream<Item = Event> + Send + 'static {
    BroadcastStream::new(rx).map(|item| match item {
        Ok(event) => event,
        Err(BroadcastStreamRecvError::Lagged(n)) => Event {
            session_id: SessionId(uuid::Uuid::nil()),
            seq: 0,
            ts: chrono::Utc::now(),
            kind: EventKind::Orchestrator(format!(
                "client event stream lagged; dropped {n} event(s) \
                 (the per-session event log is authoritative)"
            )),
        },
    })
}

// ---------------------------------------------------------------------------
// Daemon auto-start helpers
// ---------------------------------------------------------------------------

/// Whether something accepts connections on `path`.
#[cfg(unix)]
fn socket_accepts(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Non-unix has no unix sockets; the daemon crate is unix-only anyway.
#[cfg(not(unix))]
fn socket_accepts(_path: &Path) -> bool {
    false
}

/// Locate the `agentmux-server` binary: `$AGENTMUX_SERVER_BIN` → a sibling
/// of the running executable (also `..` — `target/<profile>/deps/` test
/// binaries sit one level below `target/<profile>/`) → `PATH`.
fn server_binary() -> PathBuf {
    if let Some(path) = env::var_os(ENV_SERVER_BIN) {
        return PathBuf::from(path);
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [
                Some(dir.join(SERVER_BIN_NAME)),
                dir.parent().map(|p| p.join(SERVER_BIN_NAME)),
            ]
            .into_iter()
            .flatten()
            {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    // Bare name — Command::new resolves it along PATH.
    PathBuf::from(SERVER_BIN_NAME)
}
