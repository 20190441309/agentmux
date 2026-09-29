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
//! produces [`EventKind::AgentExited`], and internal io failures surface as
//! [`EventKind::Orchestrator`]. Permission requests emit
//! [`EventKind::PermissionRequest`] when parked and
//! [`EventKind::PermissionResolved`] once they conclude (a daemon answer via
//! [`AcpConn::respond_permission`], the [`PERMISSION_TIMEOUT`] fallback, or
//! conn teardown). `Event::seq` is always `0` here — the orchestrator assigns
//! real sequence numbers when it ingests events into the session log.

use std::{
    collections::{BTreeMap, HashMap},
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
use uuid::Uuid;

use crate::config::SpawnOptions;
use crate::{Event, EventKind, PermissionDecision, Result, SessionId};

/// How long a parked `session/request_permission` waits for a
/// `session/permission` answer before resolving itself `cancelled`. A UI
/// client can die mid-answer; the timeout keeps the agent's turn from
/// hanging forever.
// TODO(config): make this configurable.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(120);

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

/// A parked `session/request_permission`: the options the agent offered
/// (kept so a generic [`PermissionDecision`] can be mapped onto a concrete
/// option id at answer time) and the oneshot the handler is awaiting.
#[derive(Debug)]
struct PendingPermission {
    options: Vec<acp::PermissionOption>,
    tx: oneshot::Sender<acp::RequestPermissionOutcome>,
}

/// The per-connection pending-permission registry, shared between the
/// worker-side [`AcpClientHandler`] (which parks requests) and the
/// [`AcpConn`] handle (which resolves them). Locked for map ops only —
/// never held across `.await`.
type PendingPermissions = Arc<Mutex<HashMap<String, PendingPermission>>>;

/// Map a generic [`PermissionDecision`] onto the options the agent
/// offered. `Err` means the asked-for kind wasn't offered — the request
/// stays parked so the caller can answer differently.
///
/// `Reject` prefers `reject_once` over `reject_always` (the narrowest
/// refusal) — by *preference* order, not the agent's offer order — and
/// degrades to `cancelled`, the deny-equivalent, when the agent offered
/// no rejection option at all (mirroring ACP's own fallback guidance).
/// `Cancel` needs no option.
fn decision_outcome(
    decision: PermissionDecision,
    options: &[acp::PermissionOption],
) -> Result<acp::RequestPermissionOutcome> {
    use acp::PermissionOptionKind as K;
    let find = |kinds: &[K]| {
        options
            .iter()
            .find(|o| kinds.contains(&o.kind))
            .map(|o| o.option_id.clone())
    };
    let selected = |option_id| {
        acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
    };
    match decision {
        PermissionDecision::AllowOnce => find(&[K::AllowOnce])
            .map(selected)
            .ok_or_else(|| anyhow!("agent offered no allow-once option")),
        PermissionDecision::AllowAlways => find(&[K::AllowAlways])
            .map(selected)
            .ok_or_else(|| anyhow!("agent offered no allow-always option")),
        PermissionDecision::Reject => Ok(find(&[K::RejectOnce])
            .or_else(|| find(&[K::RejectAlways]))
            .map(selected)
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled)),
        PermissionDecision::Cancel => Ok(acp::RequestPermissionOutcome::Cancelled),
    }
}

/// Resolve one parked request: map `decision` onto the offered options,
/// then deliver the outcome. Errors leave the entry parked — the request
/// is still answerable. A consumed entry whose receiver is already gone
/// counts as resolved (nobody is waiting anymore).
fn resolve_pending(
    pending: &PendingPermissions,
    request_id: &str,
    decision: PermissionDecision,
) -> Result<()> {
    // Lookup, mapping and removal run under one lock: a racing second
    // answer or drain can't slip between the check and the removal —
    // a decision must never return Ok and then be silently dropped.
    let (entry, outcome) = {
        let mut map = pending.lock().unwrap();
        let entry = map
            .get(request_id)
            .ok_or_else(|| anyhow!("no pending permission request {request_id}"))?;
        // Compute the outcome *before* consuming the entry: an unoffered
        // kind must leave the request parked.
        let outcome = decision_outcome(decision, &entry.options)?;
        let entry = map
            .remove(request_id)
            .expect("the map was locked across get and remove");
        (entry, outcome)
    };
    // A dropped receiver means the handler went away (worker teardown) —
    // the request is concluded either way.
    let _ = entry.tx.send(outcome);
    Ok(())
}

/// Cancel every parked request — the conn-teardown and `session/cancel`
/// path. ACP's cancellation semantics: a cancelled turn's pending
/// `session/request_permission`s must be answered `cancelled` so the
/// agent can unwind them.
fn drain_pending(pending: &PendingPermissions) {
    let entries: Vec<PendingPermission> = pending.lock().unwrap().drain().map(|(_, e)| e).collect();
    for entry in entries {
        let _ = entry.tx.send(acp::RequestPermissionOutcome::Cancelled);
    }
}

/// Render an ACP outcome for [`EventKind::PermissionResolved`]:
/// `"cancelled"` or `"selected:<option_id>"`.
fn outcome_label(outcome: &acp::RequestPermissionOutcome) -> String {
    match outcome {
        acp::RequestPermissionOutcome::Cancelled => "cancelled".to_string(),
        acp::RequestPermissionOutcome::Selected(sel) => {
            format!("selected:{}", sel.option_id)
        }
        // The enum is non_exhaustive; forward-compat label.
        _ => "unknown".to_string(),
    }
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
    ///
    /// Interior-mutable so [`AcpConn::close`]/[`AcpConn::shutdown`] work on
    /// `&self`: an `Arc`'d connection shared with an in-flight prompt must
    /// still be stoppable (the orchestrator's kill path).
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<Cmd>>>,
    /// `session/request_permission`s parked awaiting an answer, keyed by
    /// the daemon-minted `request_id` carried on the
    /// [`EventKind::PermissionRequest`] event. Shared with the
    /// worker-side handler; resolved by [`AcpConn::respond_permission`]
    /// and drained `cancelled` on `cancel`/`close`/drop.
    pending: PendingPermissions,
    /// Worker thread driving the `!Send` ACP machinery.
    thread: Option<JoinHandle<()>>,
    /// Connection timeouts from config (`ConnTimeouts`, resolved through
    /// the registry into [`SpawnOptions`] at spawn).
    timeouts: crate::config::ConnTimeouts,
}

impl AcpConn {
    /// Spawn `command` as a child process and open an ACP connection over its
    /// stdin/stdout.
    ///
    /// `env` is layered on top of the inherited environment; `cwd` becomes the
    /// child's working directory and the default root for `fs/*` requests.
    /// `options` carries the config-derived timeouts (see
    /// [`crate::config::ConnTimeouts`]). A background thread takes over io
    /// immediately; call [`AcpConn::initialize`] to perform the ACP handshake.
    pub fn spawn(
        command: &Path,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
        options: &SpawnOptions,
    ) -> Result<AcpConn> {
        let session_id = SessionId::new();
        let (event_tx, first_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let pending: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));

        let spec = SpawnSpec {
            command: command.to_path_buf(),
            args: args.to_vec(),
            env: env.clone(),
            cwd: cwd.to_path_buf(),
        };
        let thread_event_tx = event_tx.clone();
        let thread_pending = pending.clone();

        let thread = std::thread::Builder::new()
            .name(format!("acp-conn-{session_id}"))
            .spawn(move || {
                actor_main(
                    spec,
                    session_id,
                    thread_event_tx,
                    thread_pending,
                    cmd_rx,
                    ready_tx,
                );
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
            cmd_tx: Mutex::new(Some(cmd_tx)),
            pending,
            thread: Some(thread),
            timeouts: options.timeouts,
        })
    }

    /// The internal [`SessionId`] stamped on events from this connection.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Perform the ACP `initialize` handshake, advertising fs read/write and
    /// terminal client capabilities. Times out after the configured
    /// `init` bound ([`crate::config::ConnTimeouts::init`], default 10 s).
    ///
    /// Resolves to the serialized `InitializeResponse` so callers can inspect
    /// agent capabilities without depending on the ACP crate's types.
    ///
    /// All request methods take `&self`: they only forward a command over
    /// the worker channel, so a shared (`Arc`) connection supports e.g.
    /// `cancel` while a `prompt` is in flight.
    pub async fn initialize(&self) -> Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Initialize(tx))?;
        self.await_reply(rx, self.timeouts.init, "initialize").await
    }

    /// Create a new ACP session rooted at `cwd`; returns the acp session id.
    /// Unbounded: session/new cost is agent-defined (v1 keeps the
    /// pre-existing behavior rather than inventing a bound).
    pub async fn new_session(&self, cwd: &Path) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::NewSession {
            cwd: cwd.to_path_buf(),
            reply: tx,
        })?;
        self.await_reply(rx, Duration::ZERO, "session/new").await
    }

    /// Send a user prompt; resolves when the agent finishes its turn.
    /// Times out after the configured `prompt` bound
    /// ([`crate::config::ConnTimeouts::prompt`], default 600 s; `0`
    /// disables) — the timeout fires on the caller's side; the wedged
    /// worker-side request is abandoned in place and dies with the conn.
    /// An [`EventKind::Orchestrator`] "timed out" note is emitted so the
    /// orchestrator's fan-out persists it into the session log.
    pub async fn prompt(&self, session_id: &str, text: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Prompt {
            session_id: session_id.to_string(),
            text,
            reply: tx,
        })?;
        self.await_reply(rx, self.timeouts.prompt, "session/prompt")
            .await
    }

    /// Await a command reply, bounding the wait to `bound`
    /// ([`Duration::ZERO`] = wait forever). A timeout emits an
    /// `Orchestrator` note and resolves to a `timed out` error; the
    /// worker-side request keeps running (the reply oneshot is simply
    /// dropped) so machinery that *un*wedges it — `cancel`, `close` —
    /// is never blocked by the timeout.
    async fn await_reply<T>(
        &self,
        rx: oneshot::Receiver<Result<T>>,
        bound: Duration,
        what: &'static str,
    ) -> Result<T> {
        let outcome = if bound.is_zero() {
            rx.await
        } else {
            match tokio::time::timeout(bound, rx).await {
                Err(_) => {
                    emit(
                        &self.event_tx,
                        self.session_id,
                        EventKind::Orchestrator(format!("acp {what} timed out after {bound:?}")),
                    );
                    return Err(anyhow!("acp {what} timed out after {bound:?}"));
                }
                Ok(outcome) => outcome,
            }
        };
        outcome.map_err(|_| anyhow!("acp connection closed before {what} completed"))?
    }

    /// Send `session/cancel` for an in-flight prompt turn.
    ///
    /// Per ACP cancellation semantics the client MUST answer every
    /// pending `session/request_permission` with `cancelled` — parked
    /// requests are drained first so the agent's outstanding permission
    /// calls resolve instead of outliving the cancelled turn.
    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        drain_pending(&self.pending);
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Cancel {
            session_id: session_id.to_string(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("acp connection closed before session/cancel completed"))?
    }

    /// Answer a parked `session/request_permission` — the conn-level half
    /// of the `session/permission` RPC.
    ///
    /// `decision` is mapped onto the options the agent offered (see
    /// [`decision_outcome`]); answering with a kind the agent didn't
    /// offer errors and leaves the request parked. An unknown or
    /// already-resolved `request_id` errors too.
    ///
    /// Synchronous and non-blocking: the registry is shared with the
    /// worker-side handler, so no worker round-trip is needed.
    pub fn respond_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        resolve_pending(&self.pending, request_id, decision)
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

    /// Force-close the command channel: the worker loop ends and runtime
    /// teardown kills the child (`kill_on_drop`). Synchronous, instant and
    /// idempotent — the orchestrator's kill path, usable on a shared
    /// (`Arc`) connection even while a prompt is in flight. Pending
    /// requests resolve with an error once the worker exits.
    ///
    /// Unlike [`AcpConn::shutdown`] this does not wait for the worker to
    /// acknowledge or join its thread; use `shutdown` when a clean,
    /// awaited teardown is wanted and `&mut` access is available.
    pub fn close(&self) {
        // Answer parked permission requests first: while the worker is
        // still up the agent sees a clean `cancelled` rather than a dead
        // channel mid-RPC.
        drain_pending(&self.pending);
        self.cmd_tx.lock().unwrap().take();
    }

    /// Shut the connection down: the worker thread exits and the child
    /// process is killed (`kill_on_drop`). Idempotent.
    pub async fn shutdown(&mut self) -> Result<()> {
        drain_pending(&self.pending);
        // Take the sender in a statement of its own — the guard must drop
        // before awaiting, otherwise the lock is held across `.await`.
        let cmd_tx = self.cmd_tx.lock().unwrap().take();
        if let Some(cmd_tx) = cmd_tx {
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
            .lock()
            .unwrap()
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
        // (Even if the lock were poisoned, field destruction would drop the
        // sender anyway, closing the channel — this just does it early.)
        drain_pending(&self.pending);
        if let Ok(mut cmd_tx) = self.cmd_tx.lock() {
            cmd_tx.take();
        }
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
    pending: PendingPermissions,
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
            pending,
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
    /// Requests this handler has parked, shared with the [`AcpConn`]
    /// handle — `respond_permission` resolves them, conn teardown drains
    /// them, and the handler removes its own entry when its await ends
    /// (so a timed-out request can never be answered into a void).
    pending: PendingPermissions,
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
    /// Park the agent's permission request and await an answer.
    ///
    /// The request gets a daemon-minted `request_id`, is registered in
    /// the shared `pending` map, and surfaced as
    /// [`EventKind::PermissionRequest`] so clients can answer it via
    /// `session/permission` → [`AcpConn::respond_permission`]. The trait
    /// method then awaits the oneshot — the agent's turn is in flight for
    /// the whole wait, which is inherent to a permission gate.
    ///
    /// Resolution order, whichever comes first:
    /// - a `session/permission` answer (`resolve_pending`);
    /// - [`PERMISSION_TIMEOUT`] → `cancelled` (a UI can die mid-answer —
    ///   the turn must not hang forever);
    /// - the oneshot's sender being dropped (`close`/kill teardown or
    ///   `session/cancel` draining the map) → `cancelled`, per ACP's
    ///   "pending permission requests MUST be answered `cancelled` on
    ///   cancel" rule.
    ///
    /// Every conclusion emits [`EventKind::PermissionResolved`] — the
    /// single emission site that lets the orchestrator's fan-out pair
    /// park/resolve for its `WaitingPermission` transitions.
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        // Register *before* emitting: a `session/permission` answer that
        // lands between emit and insert would otherwise see no pending
        // entry and fail.
        self.pending.lock().unwrap().insert(
            request_id.clone(),
            PendingPermission {
                options: args.options.clone(),
                tx,
            },
        );
        let payload = serde_json::to_value(&args)
            .unwrap_or_else(|_| serde_json::json!({"unserializable": true}));
        emit(
            &self.event_tx,
            self.session_id,
            EventKind::PermissionRequest {
                request_id: request_id.clone(),
                request: payload,
            },
        );

        let outcome = match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
            Ok(Ok(outcome)) => outcome,
            // Sender dropped (teardown/cancel drain) or timed out —
            // either way the request resolves `cancelled`.
            Ok(Err(_)) | Err(_) => acp::RequestPermissionOutcome::Cancelled,
        };
        // Self-cleanup so the map never leaks (the timeout path leaves
        // its entry; the resolve path already consumed it — remove is
        // idempotent).
        self.pending.lock().unwrap().remove(&request_id);
        emit(
            &self.event_tx,
            self.session_id,
            EventKind::PermissionResolved {
                request_id,
                outcome: outcome_label(&outcome),
            },
        );
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
    use std::future::Future;
    use std::pin::Pin;

    fn handler(cwd: &Path) -> AcpClientHandler {
        handler_with(cwd).0
    }

    /// A handler wired to a real event channel + pending map — the
    /// permission-parking tests need both.
    fn handler_with(
        cwd: &Path,
    ) -> (
        AcpClientHandler,
        broadcast::Receiver<Event>,
        PendingPermissions,
    ) {
        let (event_tx, rx) = broadcast::channel(8);
        let pending: PendingPermissions = Arc::new(Mutex::new(HashMap::new()));
        (
            AcpClientHandler {
                session_id: SessionId::new(),
                cwd: Arc::new(Mutex::new(cwd.to_path_buf())),
                event_tx,
                pending: pending.clone(),
            },
            rx,
            pending,
        )
    }

    /// A `RequestPermissionRequest` offering allow-once, allow-always,
    /// and reject-once options.
    fn perm_request() -> acp::RequestPermissionRequest {
        acp::RequestPermissionRequest::new(
            "mock-session-1",
            acp::ToolCallUpdate::new(
                "tc-1",
                acp::ToolCallUpdateFields::new().title("Write src/x.rs".to_string()),
            ),
            vec![
                acp::PermissionOption::new("allow", "Allow", acp::PermissionOptionKind::AllowOnce),
                acp::PermissionOption::new(
                    "always",
                    "Always allow",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                acp::PermissionOption::new("deny", "Reject", acp::PermissionOptionKind::RejectOnce),
            ],
        )
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

    // --- interactive permission requests -------------------------------------

    /// The parked request future — `Client` is `#[async_trait(?Send)]`,
    /// so `tokio::spawn` can't drive it; tests poll it on their own task.
    type ParkedRequest =
        Pin<Box<dyn Future<Output = Result<acp::RequestPermissionResponse, acp::Error>>>>;

    /// Start `handler.request_permission` and poll it until its
    /// `PermissionRequest` event lands; returns the (still-parked)
    /// future plus the event's request_id.
    async fn park_request(
        h: &'static AcpClientHandler,
        rx: &mut broadcast::Receiver<Event>,
        req: acp::RequestPermissionRequest,
    ) -> (ParkedRequest, String) {
        let mut fut: ParkedRequest = Box::pin(h.request_permission(req));
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let ev = ev.expect("event channel closed");
                    if let EventKind::PermissionRequest { request_id, request } = &ev.kind {
                        assert!(
                            request.get("toolCall").is_some(),
                            "event payload should carry the tool call: {request}"
                        );
                        return (fut, request_id.clone());
                    }
                }
                r = &mut fut => panic!(
                    "request_permission completed before its event was seen: {r:?}"
                ),
            }
        }
    }

    /// `allow_once` answers with the matching option's id; the parked
    /// future resolves `Selected("allow")` and a `PermissionResolved`
    /// event follows the request in the log.
    #[tokio::test]
    async fn parked_permission_resolves_allow_once() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));

        let (fut, request_id) = park_request(h, &mut rx, perm_request()).await;
        assert!(pending.lock().unwrap().contains_key(&request_id));

        resolve_pending(&pending, &request_id, PermissionDecision::AllowOnce).unwrap();
        let resp = fut.await.unwrap();
        assert!(
            matches!(
                &resp.outcome,
                acp::RequestPermissionOutcome::Selected(sel) if sel.option_id.to_string() == "allow"
            ),
            "allow_once must select the AllowOnce option: {:?}",
            resp.outcome
        );
        assert!(pending.lock().unwrap().is_empty(), "entry consumed");

        // The resolved event pairs with the request in the event stream.
        let resolved = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                &resolved.kind,
                EventKind::PermissionResolved { request_id: rid, outcome }
                    if *rid == request_id && outcome == "selected:allow"
            ),
            "expected paired PermissionResolved, got {resolved:?}"
        );
    }

    /// `reject` picks the `reject_once` option — the agent learns it was
    /// refused rather than cancelled.
    #[tokio::test]
    async fn reject_selects_the_reject_option() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));

        let (fut, request_id) = park_request(h, &mut rx, perm_request()).await;
        resolve_pending(&pending, &request_id, PermissionDecision::Reject).unwrap();
        let resp = fut.await.unwrap();
        assert!(
            matches!(
                &resp.outcome,
                acp::RequestPermissionOutcome::Selected(sel) if sel.option_id.to_string() == "deny"
            ),
            "reject must select the RejectOnce option: {:?}",
            resp.outcome
        );
    }

    /// `reject` honours preference order, not offer order: with
    /// `reject_always` listed first it still picks the `reject_once`
    /// option — the narrowest refusal.
    #[tokio::test]
    async fn reject_prefers_once_even_when_always_is_offered_first() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));
        let req = acp::RequestPermissionRequest::new(
            "mock-session-1",
            acp::ToolCallUpdate::new("tc-1", acp::ToolCallUpdateFields::new()),
            vec![
                acp::PermissionOption::new(
                    "always-deny",
                    "Always reject",
                    acp::PermissionOptionKind::RejectAlways,
                ),
                acp::PermissionOption::new("deny", "Reject", acp::PermissionOptionKind::RejectOnce),
            ],
        );

        let (fut, request_id) = park_request(h, &mut rx, req).await;
        resolve_pending(&pending, &request_id, PermissionDecision::Reject).unwrap();
        let resp = fut.await.unwrap();
        assert!(
            matches!(
                &resp.outcome,
                acp::RequestPermissionOutcome::Selected(sel) if sel.option_id.to_string() == "deny"
            ),
            "reject must prefer the RejectOnce option over RejectAlways: {:?}",
            resp.outcome
        );
    }

    /// A decision the agent didn't offer errors and leaves the request
    /// parked — a later valid answer still resolves it. Unknown and
    /// already-resolved ids fail the same way.
    #[tokio::test]
    async fn unoffered_or_unknown_decisions_fail_and_stay_parked() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));
        // Offer allow_once only so `allow_always` is unoffered.
        let req = acp::RequestPermissionRequest::new(
            "mock-session-1",
            acp::ToolCallUpdate::new("tc-1", acp::ToolCallUpdateFields::new()),
            vec![acp::PermissionOption::new(
                "allow",
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            )],
        );

        let (fut, request_id) = park_request(h, &mut rx, req).await;

        assert!(resolve_pending(&pending, "nope", PermissionDecision::AllowOnce).is_err());
        assert!(
            resolve_pending(&pending, &request_id, PermissionDecision::AllowAlways).is_err(),
            "allow_always was not offered — the request must stay parked"
        );
        assert!(pending.lock().unwrap().contains_key(&request_id));

        resolve_pending(&pending, &request_id, PermissionDecision::AllowOnce).unwrap();
        fut.await.unwrap();
        assert!(
            resolve_pending(&pending, &request_id, PermissionDecision::AllowOnce).is_err(),
            "an already-resolved request_id must fail"
        );
    }

    /// When the agent offers only allows, `reject`/`cancel` resolve to
    /// `cancelled` — the deny-equivalent — instead of erroring.
    #[tokio::test]
    async fn reject_without_reject_option_falls_back_to_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));
        let req = acp::RequestPermissionRequest::new(
            "mock-session-1",
            acp::ToolCallUpdate::new("tc-1", acp::ToolCallUpdateFields::new()),
            vec![acp::PermissionOption::new(
                "allow",
                "Allow",
                acp::PermissionOptionKind::AllowOnce,
            )],
        );

        let (fut, request_id) = park_request(h, &mut rx, req).await;
        resolve_pending(&pending, &request_id, PermissionDecision::Reject).unwrap();
        let resp = fut.await.unwrap();
        assert!(matches!(
            &resp.outcome,
            acp::RequestPermissionOutcome::Cancelled
        ));
    }

    /// The `PERMISSION_TIMEOUT` fallback: an unanswered request resolves
    /// `cancelled`, emits `PermissionResolved`, and drops its map entry.
    /// `start_paused` makes the 120 s wait instant.
    #[tokio::test(start_paused = true)]
    async fn unanswered_permission_times_out_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));

        let (fut, request_id) = park_request(h, &mut rx, perm_request()).await;
        let resp = fut.await.unwrap();
        assert!(matches!(
            &resp.outcome,
            acp::RequestPermissionOutcome::Cancelled
        ));
        assert!(
            pending.lock().unwrap().is_empty(),
            "a timed-out request must not linger in the map"
        );
        let resolved = rx.recv().await.unwrap();
        assert!(
            matches!(
                &resolved.kind,
                EventKind::PermissionResolved { request_id: rid, outcome }
                    if *rid == request_id && outcome == "cancelled"
            ),
            "expected cancelled PermissionResolved, got {resolved:?}"
        );
    }

    /// Conn teardown (`drain_pending` — what `close`/`kill`/`cancel`
    /// call) resolves every parked request `cancelled`.
    #[tokio::test]
    async fn teardown_drains_pending_as_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let (h, mut rx, pending) = handler_with(dir.path());
        let h: &'static _ = Box::leak(Box::new(h));

        let (fut, _request_id) = park_request(h, &mut rx, perm_request()).await;
        drain_pending(&pending);
        let resp = fut.await.unwrap();
        assert!(matches!(
            &resp.outcome,
            acp::RequestPermissionOutcome::Cancelled
        ));
        assert!(pending.lock().unwrap().is_empty());
    }
}
