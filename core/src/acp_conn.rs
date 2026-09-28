//! ACP client connection wrapper.
//!
//! [`AcpConn`] owns a spawned agent process plus the
//! `agent_client_protocol::ClientSideConnection` that speaks ACP JSON-RPC
//! over the child's stdin/stdout.
//!
//! # Threading model
//!
//! The ACP crate's io futures and its `spawn` callback are `!Send` (tasks are
//! dispatched onto a `LocalSet`), so the whole connection lives on a dedicated
//! worker thread running a `current_thread` tokio runtime. [`AcpConn`] is a
//! `Send` handle that forwards calls to that thread over a command channel;
//! each method returns a future that resolves when the worker replies, so all
//! of them can be awaited from any tokio runtime.
//!
//! # Events
//!
//! `session/update` notifications are normalised to
//! [`EventKind::SessionUpdate`] carrying the raw notification JSON, agent exit
//! produces [`EventKind::AgentExited`], and permission requests / internal io
//! failures surface as [`EventKind::Orchestrator`]. `Event::seq` is always `0`
//! here — the orchestrator assigns real sequence numbers when it ingests
//! events into the session log.

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

use agent_client_protocol::{self as acp, Agent as _};
use anyhow::anyhow;
use chrono::Utc;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::{Event, EventKind, Result, SessionId};

/// Timeout for the ACP `initialize` handshake.
// TODO(config): allow overriding this via `Config` in a later task.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the per-connection event broadcast channel.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Emit an event onto the connection's broadcast channel. `seq` is a
/// placeholder — see module docs. A send error only means there are no
/// subscribers, which is fine.
fn emit(event_tx: &broadcast::Sender<Event>, session_id: SessionId, kind: EventKind) {
    let _ = event_tx.send(Event {
        session_id,
        seq: 0,
        ts: Utc::now(),
        kind,
    });
}

/// Commands sent from an [`AcpConn`] handle to its worker thread.
enum Cmd {
    Initialize(oneshot::Sender<Result<serde_json::Value>>),
    NewSession {
        cwd: PathBuf,
        reply: oneshot::Sender<Result<String>>,
    },
    Prompt {
        session_id: String,
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Cancel {
        session_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown(oneshot::Sender<()>),
}

/// A handle to an ACP connection driving a spawned agent process.
///
/// Dropping the handle (or [`AcpConn::shutdown`]) ends the worker thread,
/// which kills the child process via `kill_on_drop`.
#[derive(Debug)]
pub struct AcpConn {
    /// Broadcast channel for every [`Event`] this connection produces.
    event_tx: broadcast::Sender<Event>,
    /// The channel's original receiver, which has been buffering events since
    /// the channel was created (i.e. before the worker thread started).
    /// Handed to the *first* [`AcpConn::events`] caller so early events — a
    /// fast `AgentExited` is the realistic one — are not silently dropped.
    first_rx: Mutex<Option<broadcast::Receiver<Event>>>,
    /// Internal session id stamped onto emitted events; the orchestrator uses
    /// it to correlate events with its own `Session` record.
    session_id: SessionId,
    /// Channel to the worker thread; `None` after shutdown.
    cmd_tx: Option<mpsc::UnboundedSender<Cmd>>,
    /// Worker thread driving the `!Send` ACP machinery.
    thread: Option<JoinHandle<()>>,
}

impl AcpConn {
    /// Spawn `command` as a child process and open an ACP connection over its
    /// stdin/stdout.
    ///
    /// `env` is layered on top of the inherited environment; `cwd` becomes the
    /// child's working directory and the default root for `fs/*` requests.
    /// A background thread takes over io immediately; call
    /// [`AcpConn::initialize`] to perform the ACP handshake.
    pub fn spawn(
        command: &Path,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<AcpConn> {
        let session_id = SessionId::new();
        let (event_tx, first_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let spec = SpawnSpec {
            command: command.to_path_buf(),
            args: args.to_vec(),
            env: env.clone(),
            cwd: cwd.to_path_buf(),
        };
        let thread_event_tx = event_tx.clone();

        let thread = std::thread::Builder::new()
            .name(format!("acp-conn-{session_id}"))
            .spawn(move || {
                actor_main(spec, session_id, thread_event_tx, cmd_rx, ready_tx);
            })
            .map_err(|e| anyhow!("failed to spawn acp io thread: {e}"))?;

        // Block until the worker reports whether the child spawned. If the
        // worker dies first, `ready_tx` is dropped and `recv` errors out.
        ready_rx
            .recv()
            .map_err(|_| anyhow!("acp io thread died before reporting spawn status"))??;

        Ok(AcpConn {
            event_tx,
            first_rx: Mutex::new(Some(first_rx)),
            session_id,
            cmd_tx: Some(cmd_tx),
            thread: Some(thread),
        })
    }

    /// The internal [`SessionId`] stamped on events from this connection.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Perform the ACP `initialize` handshake, advertising fs read/write and
    /// terminal client capabilities. Times out after [`INIT_TIMEOUT`].
    ///
    /// Resolves to the serialized `InitializeResponse` so callers can inspect
    /// agent capabilities without depending on the ACP crate's types.
    pub async fn initialize(&mut self) -> Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Initialize(tx))?;
        tokio::time::timeout(INIT_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("acp initialize timed out after {INIT_TIMEOUT:?}"))?
            .map_err(|_| anyhow!("acp connection closed before initialize completed"))?
    }

    /// Create a new ACP session rooted at `cwd`; returns the acp session id.
    pub async fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::NewSession {
            cwd: cwd.to_path_buf(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("acp connection closed before session/new completed"))?
    }

    /// Send a user prompt; resolves when the agent finishes its turn.
    pub async fn prompt(&mut self, session_id: &str, text: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Prompt {
            session_id: session_id.to_string(),
            text,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("acp connection closed before session/prompt completed"))?
    }

    /// Send `session/cancel` for an in-flight prompt turn.
    pub async fn cancel(&mut self, session_id: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Cancel {
            session_id: session_id.to_string(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("acp connection closed before session/cancel completed"))?
    }

    /// Subscribe to this connection's event stream.
    ///
    /// The *first* call returns the receiver that has been buffering since
    /// `spawn`, so events emitted before anyone subscribed (e.g. an early
    /// `AgentExited`) are replayed to that receiver instead of being lost.
    /// Later calls get a receiver that sees only events from subscribe-time
    /// onwards — standard broadcast semantics.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        if let Some(rx) = self.first_rx.lock().unwrap().take() {
            return rx;
        }
        self.event_tx.subscribe()
    }

    /// Shut the connection down: the worker thread exits and the child
    /// process is killed (`kill_on_drop`). Idempotent.
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(cmd_tx) = self.cmd_tx.take() {
            let (tx, rx) = oneshot::channel();
            if cmd_tx.send(Cmd::Shutdown(tx)).is_ok() {
                // Best-effort wait for the worker to acknowledge; a dead
                // worker just drops our receiver.
                let _ = rx.await;
            }
            // `cmd_tx` drops here; even without the Shutdown command the
            // closed channel ends the worker loop.
        }
        if let Some(thread) = self.thread.take() {
            // The worker already acknowledged shutdown (or died), so the join
            // normally returns promptly — but joining a thread must never
            // block the caller's executor, so it runs on the blocking pool.
            // Without a runtime there is nothing to stall; detach instead.
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            }
        }
        Ok(())
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.cmd_tx
            .as_ref()
            .ok_or_else(|| anyhow!("acp connection is shut down"))?
            .send(cmd)
            .map_err(|_| anyhow!("acp connection closed"))
    }
}

impl Drop for AcpConn {
    fn drop(&mut self) {
        // Closing the command channel ends the worker loop; runtime teardown
        // on that thread then kills the child (kill_on_drop). The join handle
        // is intentionally detached — `drop` must not block an async
        // executor, and the worker exits promptly once the channel closes.
        self.cmd_tx.take();
    }
}

/// Everything the worker thread needs to spawn the child, captured by value.
struct SpawnSpec {
    command: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
}

/// The worker thread: builds its own `current_thread` runtime, spawns the
/// child, wires up `ClientSideConnection` on a `LocalSet`, then serves
/// commands until the channel closes.
fn actor_main(
    spec: SpawnSpec,
    session_id: SessionId,
    event_tx: broadcast::Sender<Event>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    ready_tx: std::sync::mpsc::Sender<Result<()>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow!("failed to build acp io runtime: {e}")));
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let mut child = match tokio::process::Command::new(&spec.command)
            .args(&spec.args)
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // TODO(observability): route agent stderr into the event stream
            // instead of discarding it.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let _ = ready_tx.send(Err(anyhow!(
                    "failed to spawn agent process {}: {e}",
                    spec.command.display()
                )));
                return;
            }
        };

        let child_stdin = child.stdin.take().expect("stdin was piped");
        let child_stdout = child.stdout.take().expect("stdout was piped");

        // Working directory the `fs/*` handlers are confined to; replaced by
        // the `session/new` cwd once a session exists.
        let session_cwd = Arc::new(Mutex::new(spec.cwd.clone()));

        let handler = AcpClientHandler {
            session_id,
            cwd: session_cwd.clone(),
            event_tx: event_tx.clone(),
        };

        // The returned io future and the `spawn` callback are `!Send`; both
        // stay on this LocalSet.
        let (conn, io_task) = acp::ClientSideConnection::new(
            handler,
            child_stdin.compat_write(),
            child_stdout.compat(),
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );
        let conn = Rc::new(conn);

        // Drive the JSON-RPC io loop in the background; surface failures.
        tokio::task::spawn_local({
            let event_tx = event_tx.clone();
            async move {
                if let Err(e) = io_task.await {
                    emit(
                        &event_tx,
                        session_id,
                        EventKind::Orchestrator(format!("acp io loop failed: {e}")),
                    );
                }
            }
        });

        // Report child exit as `AgentExited`.
        tokio::task::spawn_local({
            let event_tx = event_tx.clone();
            async move {
                let code = match child.wait().await {
                    Ok(status) => status.code(),
                    Err(e) => {
                        log_noop_wait_error(&e);
                        None
                    }
                };
                emit(&event_tx, session_id, EventKind::AgentExited { code });
            }
        });

        if ready_tx.send(Ok(())).is_err() {
            // The caller went away while we were starting up.
            return;
        }

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Cmd::Shutdown(reply) => {
                    let _ = reply.send(());
                    break;
                }
                // Each request runs as its own local task so a long-running
                // `session/prompt` cannot starve `session/cancel` or
                // `Shutdown`, and so a dropped `AcpConn` cannot strand the
                // loop inside one RPC.
                cmd => {
                    tokio::task::spawn_local(handle_cmd(conn.clone(), session_cwd.clone(), cmd));
                }
            }
        }
        // Loop done: `block_on` returns, the runtime tears down every local
        // task, and `kill_on_drop` reaps the child.
    });
}

/// `Child::wait` errors carry no actionable detail today; keep a hook so we
/// can wire logging later without changing the call site.
fn log_noop_wait_error(_e: &std::io::Error) {
    // TODO(observability): surface wait() failures.
}

async fn handle_cmd(
    conn: Rc<acp::ClientSideConnection>,
    session_cwd: Arc<Mutex<PathBuf>>,
    cmd: Cmd,
) {
    match cmd {
        Cmd::Initialize(reply) => {
            let request = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                .client_info(acp::Implementation::new(
                    "agentmux",
                    env!("CARGO_PKG_VERSION"),
                ))
                .client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new()
                            .read_text_file(true)
                            .write_text_file(true))
                        .terminal(true),
                );
            let result = conn
                .initialize(request)
                .await
                .map_err(|e| anyhow!("acp initialize failed: {e}"))
                .and_then(|resp| {
                    serde_json::to_value(&resp)
                        .map_err(|e| anyhow!("failed to serialize initialize response: {e}"))
                });
            let _ = reply.send(result);
        }
        Cmd::NewSession { cwd, reply } => {
            let result = conn
                .new_session(acp::NewSessionRequest::new(cwd.clone()))
                .await
                .map_err(|e| anyhow!("acp session/new failed: {e}"))
                .map(|resp| resp.session_id.to_string());
            if result.is_ok() {
                *session_cwd.lock().unwrap() = cwd;
            }
            let _ = reply.send(result);
        }
        Cmd::Prompt {
            session_id,
            text,
            reply,
        } => {
            let request = acp::PromptRequest::new(session_id, vec![acp::ContentBlock::from(text)]);
            let result = conn
                .prompt(request)
                .await
                .map(|_| ())
                .map_err(|e| anyhow!("acp session/prompt failed: {e}"));
            let _ = reply.send(result);
        }
        Cmd::Cancel { session_id, reply } => {
            let result = conn
                .cancel(acp::CancelNotification::new(session_id))
                .await
                .map_err(|e| anyhow!("acp session/cancel failed: {e}"));
            let _ = reply.send(result);
        }
        // Handled in the command loop itself.
        Cmd::Shutdown(_) => {}
    }
}

/// The ACP `Client` implementation bound to this connection.
///
/// Lives entirely on the worker thread's `LocalSet`; communicates outward
/// through `event_tx` and the shared `cwd`.
struct AcpClientHandler {
    /// Internal session id stamped onto emitted [`Event`]s.
    session_id: SessionId,
    /// Directory `fs/*` requests are confined to.
    cwd: Arc<Mutex<PathBuf>>,
    event_tx: broadcast::Sender<Event>,
}

impl AcpClientHandler {
    /// Resolve `path` against the session cwd, rejecting paths outside it.
    ///
    /// `fs/*` requests carry absolute paths but agents do send relative ones;
    /// both are normalised lexically, then checked again after resolving the
    /// deepest existing ancestor so symlinks cannot escape the cwd.
    fn resolve_in_cwd(&self, path: &Path) -> std::result::Result<PathBuf, acp::Error> {
        let cwd = normalize_lexical(&self.cwd.lock().unwrap());
        let candidate = if path.is_absolute() {
            normalize_lexical(path)
        } else {
            normalize_lexical(&cwd.join(path))
        };
        if !candidate.starts_with(&cwd) {
            return Err(acp::Error::invalid_params().data(serde_json::json!(format!(
                "path {} is outside session cwd {}",
                path.display(),
                cwd.display()
            ))));
        }
        // Canonicalize the deepest *existing* ancestor and re-attach the
        // (already normalized, so `..`-free) dangling tail.
        //
        // `symlink_metadata` is used (not `exists`, which follows links) so a
        // symlink — including a *dangling* one, the classic escape vector for
        // `fs/write` — counts as existing and gets checked by canonicalize
        // below instead of being re-attached unchecked.
        let mut ancestor = candidate.clone();
        let mut tail = Vec::new();
        while ancestor.symlink_metadata().is_err() {
            match ancestor.file_name() {
                Some(name) => tail.push(name.to_os_string()),
                None => break,
            }
            ancestor.pop();
        }
        // Fail closed: an unresolvable ancestor (dangling symlink, missing
        // cwd, permission error) rejects the request rather than skipping the
        // boundary check.
        let mut resolved = ancestor.canonicalize().map_err(|e| {
            acp::Error::invalid_params().data(serde_json::json!(format!(
                "cannot safely resolve {}: {e}",
                path.display()
            )))
        })?;
        for comp in tail.iter().rev() {
            resolved.push(comp);
        }
        let base = cwd.canonicalize().map_err(|e| {
            acp::Error::invalid_params().data(serde_json::json!(format!(
                "session cwd is not resolvable: {e}"
            )))
        })?;
        if !resolved.starts_with(&base) {
            return Err(acp::Error::invalid_params()
                .data(serde_json::json!("resolved path escapes session cwd")));
        }
        // Defense in depth: a final path that is itself a symlink is refused
        // outright (canonicalize already resolves links, so this only fires
        // if the tail somehow re-attached one).
        if resolved
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(acp::Error::invalid_params()
                .data(serde_json::json!("refusing to operate through a symlink")));
        }
        Ok(resolved)
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for AcpClientHandler {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        // Forward the request onto the event bus so the daemon/UI can see it.
        let payload = serde_json::to_value(&args)
            .unwrap_or_else(|_| serde_json::json!({"unserializable": true}));
        emit(
            &self.event_tx,
            self.session_id,
            EventKind::Orchestrator(format!("permission-request: {payload}")),
        );

        // SECURITY: deny-by-default. Prefer an explicit `reject_*` option so
        // the agent learns the call was refused; fall back to `cancelled`
        // when the agent offered no rejection option.
        // TODO(T14): park the request on a oneshot and let the daemon drive
        // an interactive approval flow instead of auto-denying.
        let reject = args.options.iter().find(|o| {
            matches!(
                o.kind,
                acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways
            )
        });
        let outcome = match reject {
            Some(option) => acp::RequestPermissionOutcome::Selected(
                acp::SelectedPermissionOutcome::new(option.option_id.clone()),
            ),
            None => acp::RequestPermissionOutcome::Cancelled,
        };
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        let payload = serde_json::to_value(&args).unwrap_or(serde_json::Value::Null);
        emit(
            &self.event_tx,
            self.session_id,
            EventKind::SessionUpdate(payload),
        );
        Ok(())
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> acp::Result<acp::ReadTextFileResponse> {
        let path = self.resolve_in_cwd(&args.path)?;
        // Blocking fs on the worker thread is acceptable for v1 — reads are
        // small and the io loop tolerates brief stalls.
        let content = std::fs::read_to_string(&path).map_err(acp::Error::into_internal_error)?;
        let content = match (args.line, args.limit) {
            (None, None) => content,
            (line, limit) => {
                let skip = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
                let take = limit.map(|l| l as usize).unwrap_or(usize::MAX);
                let mut out = content
                    .lines()
                    .skip(skip)
                    .take(take)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !out.is_empty() {
                    out.push('\n');
                }
                out
            }
        };
        Ok(acp::ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> acp::Result<acp::WriteTextFileResponse> {
        let path = self.resolve_in_cwd(&args.path)?;
        std::fs::write(&path, &args.content).map_err(acp::Error::into_internal_error)?;
        // Surface the edit so the TUI/orchestrator can display it.
        let rel = path
            .strip_prefix(&*self.cwd.lock().unwrap())
            .unwrap_or(&path)
            .to_path_buf();
        emit(
            &self.event_tx,
            self.session_id,
            EventKind::FileEdited { path: rel },
        );
        Ok(acp::WriteTextFileResponse::new())
    }

    // terminal/* is intentionally unimplemented for v1: the trait's default
    // methods return `method_not_found`, which agents treat as "unsupported".
    // TODO: implement a terminal registry (bounded output, wait_for_exit).
}

/// Remove `.`/`..` components lexically (no filesystem access).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Client as _;

    fn handler(cwd: &Path) -> AcpClientHandler {
        AcpClientHandler {
            session_id: SessionId::new(),
            cwd: Arc::new(Mutex::new(cwd.to_path_buf())),
            event_tx: broadcast::channel(1).0,
        }
    }

    #[test]
    fn resolve_accepts_in_cwd_paths_and_rejects_lexical_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/real.txt"), "x").unwrap();

        let h = handler(dir.path());
        // Existing file, relative path, and not-yet-created write target.
        assert!(h.resolve_in_cwd(&dir.path().join("sub/real.txt")).is_ok());
        assert!(h.resolve_in_cwd(Path::new("new.txt")).is_ok());
        // Lexical `..` escape is refused.
        assert!(h.resolve_in_cwd(Path::new("../outside.txt")).is_err());
    }

    /// The core escape vector: a *dangling* symlink inside the cwd whose
    /// target is outside it. `Path::exists` follows links and reports the
    /// link as missing, so a naive check would re-attach the name unchecked
    /// and `fs::write` would follow it out of the sandbox.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_dangling_symlink_is_rejected() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("pwned.txt");
        std::os::unix::fs::symlink(&victim, inside.path().join("evil.txt")).unwrap();

        let h = handler(inside.path());
        // Both the raw resolver and the write handler must refuse…
        assert!(h.resolve_in_cwd(&inside.path().join("evil.txt")).is_err());
        let req = acp::WriteTextFileRequest::new("s", inside.path().join("evil.txt"), "owned");
        assert!(h.write_text_file(req).await.is_err());
        // …and the outside file must not have been created.
        assert!(!victim.exists());
    }

    /// A live symlink pointing outside the cwd must not be readable either.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_through_symlink_escaping_cwd_is_rejected() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            inside.path().join("peek.txt"),
        )
        .unwrap();

        let h = handler(inside.path());
        let req = acp::ReadTextFileRequest::new("s", inside.path().join("peek.txt"));
        assert!(h.read_text_file(req).await.is_err());
    }

    /// A symlink chain inside the cwd pointing back inside is still served —
    /// the sandbox boundary is about escape, not about links per se.
    #[cfg(unix)]
    #[test]
    fn symlink_within_cwd_is_allowed() {
        let inside = tempfile::tempdir().unwrap();
        std::fs::write(inside.path().join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(
            inside.path().join("real.txt"),
            inside.path().join("link.txt"),
        )
        .unwrap();

        let h = handler(inside.path());
        assert!(h.resolve_in_cwd(&inside.path().join("link.txt")).is_ok());
    }
}
