//! `pi --mode rpc` connection wrapper.
//!
//! [`PiConn`] owns a spawned `pi` process running in RPC mode and speaks
//! pi's JSONL command/response/event protocol over the child's
//! stdin/stdout — *not* JSON-RPC. Its method surface is identical to
//! [`AcpConn`](crate::AcpConn) (`spawn`/`initialize`/`new_session`/
//! `prompt`/`cancel`/`events`/`shutdown`) so the orchestrator's
//! `SpawnedConn` can delegate uniformly.
//!
//! # Protocol summary (pi docs: `docs/rpc.md`, `docs/rpc-commands.md`)
//!
//! - **Commands** (stdin): JSON objects, one per line —
//!   `{"id": "...", "type": "prompt", "message": "..."}`.
//! - **Responses** (stdout): `{"id": "...", "type": "response",
//!   "command": "...", "success": bool, "data"?/ "error"?}` — routed to the
//!   pending command by `id`.
//! - **Events** (stdout): `{"type": "<event kind>", ...}` with no `id`
//!   (the exception: `bash_execution_update` carries its originating
//!   `bash` command's id — still an event). Extension-UI requests
//!   (`type: "extension_ui_request"`) also carry an `id`; they are
//!   surfaced as events — v1 does not answer them.
//!
//! # Framing — LF only
//!
//! Records are delimited **only** by `\n` (LF), with an optional preceding
//! `\r` stripped. Raw U+2028/U+2029 bytes are legal inside JSON string
//! payloads and must not split records — generic line readers (and
//! `str::lines`) treat them as boundaries and corrupt the stream. The
//! `take_record` loop splits bytes on `0x0A` and nothing else.
//!
//! # Semantic mapping to the ACP-shaped surface
//!
//! - `initialize()` — pi has no handshake phase; we issue `get_state`,
//!   which both proves the subprocess speaks the protocol and returns the
//!   initial session state. Documented no-op-equivalent handshake.
//! - `new_session(cwd)` — sends `new_session`, then `get_state`, returning
//!   the pi session id (`data.sessionId`, falling back to `sessionName`/
//!   `sessionFile`). pi sessions are process-global, so `cwd` is accepted
//!   for signature parity but unused (the workdir is fixed at spawn).
//! - `prompt(session_id, text)` — sends `prompt`; the response's
//!   `data.disposition` only reports acceptance. To match ACP's
//!   "resolves when the turn ends" shape we then wait for the
//!   `agent_settled` event (pi's "no remaining automatic work" signal).
//!   `disposition == "handled"` means an extension consumed the prompt
//!   and no run starts, so it resolves immediately. `session_id` is
//!   unused — pi tracks one session per process.
//! - `cancel(session_id)` — sends `abort`, which waits for the session to
//!   go idle before responding.
//!
//! Events stream onto the broadcast bus as [`EventKind::SessionUpdate`]
//! carrying the raw record JSON; process exit produces
//! [`EventKind::AgentExited`]; protocol anomalies (unparseable lines,
//! unroutable responses) surface as [`EventKind::Orchestrator`].
//! `Event::seq` is `0` — the orchestrator assigns real sequence numbers.
//!
//! # Threading model
//!
//! Same shape as [`AcpConn`](crate::AcpConn): the connection lives on a
//! dedicated worker thread running a `current_thread` runtime, driven
//! through a command
//! channel, so [`PiConn::spawn`] works without a tokio runtime and every
//! async method can be awaited from any runtime. Unlike ACP, none of the
//! machinery here is `!Send`.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::anyhow;
use chrono::Utc;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::ChildStdout,
    sync::{broadcast, mpsc, oneshot},
};
use uuid::Uuid;

use crate::{Event, EventKind, Result, SessionId};

/// Timeout for the `get_state` handshake performed by [`PiConn::initialize`].
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

/// Pop the next LF-terminated record from `buf` (terminator excluded), or
/// `None` if `buf` holds no complete record yet.
///
/// Only `0x0A` is a delimiter — this is the documented pi framing rule.
/// Raw U+2028/U+2029 bytes stay inside the record where they belong. An
/// optional `\r` immediately before the `\n` is stripped.
fn take_record(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    let pos = buf.iter().position(|b| *b == b'\n')?;
    let mut rec: Vec<u8> = buf.drain(..=pos).collect();
    rec.pop(); // drop the \n itself
    if rec.last() == Some(&b'\r') {
        rec.pop();
    }
    Some(rec)
}

/// What one decoded stdout record means for the connection.
enum Classified {
    /// `type:"response"` — route to the pending-command map by `id`.
    Response {
        id: Option<String>,
        command: Option<String>,
        success: bool,
        data: Option<Value>,
        error: Option<String>,
    },
    /// Any other JSON object — a session event for the broadcast bus.
    Event(Value),
    /// Blank, unparseable, or non-object JSON. Not a bus event.
    Junk,
}

/// Classify one already-split stdout line. Pure and I/O-free.
fn classify_line(line: &str) -> Classified {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Classified::Junk;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Classified::Junk,
    };
    if !value.is_object() {
        return Classified::Junk;
    }
    if value.get("type").and_then(|t| t.as_str()) == Some("response") {
        return Classified::Response {
            id: value
                .get("id")
                .and_then(|i| i.as_str())
                .map(str::to_owned),
            command: value
                .get("command")
                .and_then(|c| c.as_str())
                .map(str::to_owned),
            success: value
                .get("success")
                .and_then(|s| s.as_bool())
                .unwrap_or(false),
            data: value.get("data").cloned(),
            error: value.get("error").map(|e| match e.as_str() {
                Some(s) => s.to_owned(),
                None => e.to_string(),
            }),
        };
    }
    Classified::Event(value)
}

/// Translate one raw stdout line into a bus [`Event`], if it carries one.
///
/// - a pi event record (any JSON object that is not a command response) →
///   `Some(Event)` with [`EventKind::SessionUpdate`] holding the raw JSON
/// - a command response (`type:"response"`) → `None`: responses are routed
///   to the pending-command map, never onto the bus
/// - blank/garbage/non-object lines → `None`
///
/// The returned event carries a nil `session_id` placeholder — callers
/// broadcasting it must stamp their connection's real session id first —
/// and `seq` 0 (the orchestrator assigns real sequence numbers).
///
/// Pure and I/O-free: the framing guarantee is that `line` is already one
/// complete LF-delimited record — a line containing raw U+2028/U+2029
/// inside a string payload translates as a single record, unsplit.
pub fn translate_line(line: &str) -> Option<Event> {
    match classify_line(line) {
        Classified::Event(value) => Some(Event {
            session_id: SessionId(Uuid::nil()),
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::SessionUpdate(value),
        }),
        _ => None,
    }
}

/// Extract the pi session identifier from a `get_state` `data` object:
/// `sessionId`, falling back to `sessionName` then `sessionFile`.
fn extract_session_id(state: &Value) -> Option<String> {
    for key in ["sessionId", "sessionName", "sessionFile"] {
        if let Some(s) = state.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_owned());
            }
        }
    }
    None
}

/// Commands sent from a [`PiConn`] handle to its worker thread.
enum Cmd {
    Initialize(oneshot::Sender<Result<Value>>),
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

/// A handle to a connection driving a spawned `pi --mode rpc` process.
///
/// Dropping the handle (or [`PiConn::shutdown`]) ends the worker thread,
/// which kills the child process via `kill_on_drop`.
#[derive(Debug)]
pub struct PiConn {
    /// Broadcast channel for every [`Event`] this connection produces.
    event_tx: broadcast::Sender<Event>,
    /// The channel's original receiver, buffering events since channel
    /// creation. Handed to the *first* [`PiConn::events`] caller so early
    /// events — a fast `AgentExited` is the realistic one — are not lost.
    first_rx: Mutex<Option<broadcast::Receiver<Event>>>,
    /// Internal session id stamped onto emitted events.
    session_id: SessionId,
    /// Channel to the worker thread; `None` after shutdown.
    cmd_tx: Option<mpsc::UnboundedSender<Cmd>>,
    /// Worker thread driving the child process io.
    thread: Option<JoinHandle<()>>,
}

impl PiConn {
    /// Spawn `command` as a child process and speak pi's JSONL RPC protocol
    /// over its stdin/stdout. `args` should normally include
    /// `"--mode", "rpc"` (supplied by the `AdapterKind::PiRpc` profile).
    ///
    /// `env` is layered on top of the inherited environment; `cwd` becomes
    /// the child's working directory. A background thread takes over io
    /// immediately; call [`PiConn::initialize`] to probe the protocol.
    pub fn spawn(
        command: &Path,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<PiConn> {
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
            .name(format!("pi-conn-{session_id}"))
            .spawn(move || {
                actor_main(spec, session_id, thread_event_tx, cmd_rx, ready_tx);
            })
            .map_err(|e| anyhow!("failed to spawn pi io thread: {e}"))?;

        // Block until the worker reports whether the child spawned. If the
        // worker dies first, `ready_tx` is dropped and `recv` errors out.
        ready_rx
            .recv()
            .map_err(|_| anyhow!("pi io thread died before reporting spawn status"))??;

        Ok(PiConn {
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

    /// Handshake: pi RPC mode has no initialize phase, so this issues
    /// `get_state` — proving the subprocess speaks the protocol and
    /// yielding the initial session state. Times out after 10s.
    ///
    /// Resolves to the response's `data` (an `RpcSessionState` object).
    pub async fn initialize(&mut self) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Initialize(tx))?;
        tokio::time::timeout(INIT_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("pi initialize timed out after {INIT_TIMEOUT:?}"))?
            .map_err(|_| anyhow!("pi connection closed before initialize completed"))?
    }

    /// Start a fresh pi session: sends `new_session`, then `get_state`,
    /// resolving to the pi session id (`data.sessionId`). `cwd` is
    /// accepted for signature parity with `AcpConn` but unused — pi's
    /// working directory is fixed at spawn.
    pub async fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::NewSession {
            cwd: cwd.to_path_buf(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("pi connection closed before new_session completed"))?
    }

    /// Send a user prompt; resolves when the agent settles (the
    /// `agent_settled` event — pi's "no remaining automatic work" signal,
    /// matching ACP's end-of-turn resolution). A `disposition:"handled"`
    /// response resolves immediately since no run starts.
    ///
    /// `session_id` is unused: pi tracks one session per process.
    pub async fn prompt(&mut self, session_id: &str, text: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Prompt {
            session_id: session_id.to_string(),
            text,
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("pi connection closed before prompt completed"))?
    }

    /// Send `abort` for an in-flight run; pi waits for the session to go
    /// idle before responding. `session_id` is unused.
    pub async fn cancel(&mut self, session_id: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Cancel {
            session_id: session_id.to_string(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("pi connection closed before abort completed"))?
    }

    /// Subscribe to this connection's event stream.
    ///
    /// The *first* call returns the receiver that has been buffering since
    /// `spawn`, so events emitted before anyone subscribed (e.g. an early
    /// `AgentExited`) are replayed rather than dropped. Later calls get a
    /// receiver that sees only events from subscribe-time onwards.
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
            // The worker already acknowledged shutdown (or died), so the
            // join normally returns promptly — but joining a thread must
            // never block the caller's executor, so it runs on the
            // blocking pool. Without a runtime there is nothing to stall;
            // detach instead.
            if tokio::runtime::Handle::try_current().is_ok() {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            }
        }
        Ok(())
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.cmd_tx
            .as_ref()
            .ok_or_else(|| anyhow!("pi connection is shut down"))?
            .send(cmd)
            .map_err(|_| anyhow!("pi connection closed"))
    }
}

impl Drop for PiConn {
    fn drop(&mut self) {
        // Closing the command channel ends the worker loop; runtime
        // teardown on that thread then kills the child (kill_on_drop).
        // The join handle is intentionally detached — `drop` must not
        // block an async executor.
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

/// Map of in-flight command ids to their pending reply channels.
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

/// Shared RPC machinery cloned into each command task on the worker.
#[derive(Clone)]
struct RpcCore {
    /// Child stdin behind an async mutex — one whole command line per lock.
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    /// In-flight commands awaiting their `type:"response"` record.
    pending: PendingMap,
    /// Monotonic id source for command correlation (`agentmux-N`).
    next_id: Arc<AtomicU64>,
    /// Bus for session events; also used by `prompt` to watch for
    /// `agent_settled`.
    event_tx: broadcast::Sender<Event>,
}

impl RpcCore {
    /// Send one command object and await its response.
    ///
    /// Resolves to the response's `data` (`Value::Null` when absent) on
    /// `success:true`, or an `Err` carrying the response's `error` string
    /// on `success:false`.
    async fn request(&self, mut command: Value) -> Result<Value> {
        let id = format!("agentmux-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        command["id"] = Value::String(id.clone());

        // Register the pending reply *before* writing so a fast response
        // cannot arrive before the map knows about it.
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);

        // serde_json escapes embedded newlines inside strings, so the
        // serialized command is always a single LF-terminated record.
        let mut line = serde_json::to_string(&command)
            .map_err(|e| anyhow!("failed to serialize pi command: {e}"))?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(anyhow!("failed to write pi command: {e}"));
            }
        }

        rx.await
            .map_err(|_| anyhow!("pi connection closed before response"))?
    }
}

/// The worker thread: builds its own `current_thread` runtime, spawns the
/// child, then serves commands until the channel closes.
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
            let _ = ready_tx.send(Err(anyhow!("failed to build pi io runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
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

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        let core = RpcCore {
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            event_tx: event_tx.clone(),
        };

        // Drive the stdout record loop in the background; on EOF it fails
        // every pending command so callers never hang.
        tokio::spawn(read_loop(
            stdout,
            core.pending.clone(),
            event_tx.clone(),
            session_id,
        ));

        // Report child exit as `AgentExited`.
        tokio::spawn({
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
                // Each request runs as its own task so a long-running
                // `prompt` (waiting for `agent_settled`) cannot starve
                // `abort` or `Shutdown`, and a dropped `PiConn` cannot
                // strand the loop inside one request.
                cmd => {
                    tokio::spawn(handle_cmd(core.clone(), cmd));
                }
            }
        }
        // Loop done: `block_on` returns, the runtime tears down every
        // spawned task, and `kill_on_drop` reaps the child.
    });
}

/// `Child::wait` errors carry no actionable detail today; keep a hook so we
/// can wire logging later without changing the call site.
fn log_noop_wait_error(_e: &std::io::Error) {
    // TODO(observability): surface wait() failures.
}

/// The stdout reader: buffer bytes, split **only** on LF via
/// [`take_record`], and dispatch each record — responses to the pending
/// map, events to the bus. On EOF, fail all pending commands.
async fn read_loop(
    mut stdout: ChildStdout,
    pending: PendingMap,
    event_tx: broadcast::Sender<Event>,
    session_id: SessionId,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(rec) = take_record(&mut buf) {
                    handle_record(&rec, &pending, &event_tx, session_id);
                }
            }
            Err(e) => {
                emit(
                    &event_tx,
                    session_id,
                    EventKind::Orchestrator(format!("pi stdout read failed: {e}")),
                );
                break;
            }
        }
    }
    // Tolerate a torn tail: try to decode whatever the last partial
    // record left in the buffer before giving up on it.
    if !buf.is_empty() {
        handle_record(&buf, &pending, &event_tx, session_id);
    }
    // The agent is gone: no response will ever arrive for these ids.
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(anyhow!("pi process closed stdout")));
    }
}

/// Dispatch one decoded record: response → pending map, event → bus,
/// junk → an `Orchestrator` note (pi's stdout is reserved for JSONL, so
/// anomalies deserve to be visible).
fn handle_record(
    rec: &[u8],
    pending: &PendingMap,
    event_tx: &broadcast::Sender<Event>,
    session_id: SessionId,
) {
    let line = match std::str::from_utf8(rec) {
        Ok(line) => line,
        Err(_) => {
            emit(
                event_tx,
                session_id,
                EventKind::Orchestrator("pi emitted non-UTF-8 record".into()),
            );
            return;
        }
    };
    match classify_line(line) {
        Classified::Response {
            id,
            command,
            success,
            data,
            error,
        } => match id {
            Some(id) => {
                let tx = pending.lock().unwrap().remove(&id);
                match tx {
                    Some(tx) => {
                        let result = if success {
                            Ok(data.unwrap_or(Value::Null))
                        } else {
                            Err(anyhow!(
                                "pi {} failed: {}",
                                command.as_deref().unwrap_or("command"),
                                error.as_deref().unwrap_or("unknown error")
                            ))
                        };
                        let _ = tx.send(result);
                    }
                    None => emit(
                        event_tx,
                        session_id,
                        EventKind::Orchestrator(format!(
                            "pi response for unknown id {id}"
                        )),
                    ),
                }
            }
            None => emit(
                event_tx,
                session_id,
                EventKind::Orchestrator("pi response without id".into()),
            ),
        },
        Classified::Event(value) => {
            emit(event_tx, session_id, EventKind::SessionUpdate(value));
        }
        Classified::Junk => {
            // Skip pure whitespace silently; report real garbage.
            if !line.trim().is_empty() {
                let preview: String = line.trim().chars().take(120).collect();
                emit(
                    event_tx,
                    session_id,
                    EventKind::Orchestrator(format!("unparseable pi record: {preview}")),
                );
            }
        }
    }
}

/// Wait for the `agent_settled` event on the bus — pi's signal that the
/// session-level run has no remaining automatic work (the closest analog
/// of ACP's prompt end-of-turn). Terminal conditions: `AgentExited` or a
/// closed event stream.
async fn wait_for_settled(rx: &mut broadcast::Receiver<Event>) -> Result<()> {
    loop {
        match rx.recv().await {
            Ok(event) => match &event.kind {
                EventKind::SessionUpdate(v)
                    if v.get("type").and_then(|t| t.as_str()) == Some("agent_settled") =>
                {
                    return Ok(())
                }
                EventKind::AgentExited { .. } => {
                    return Err(anyhow!("pi agent exited before the prompt settled"))
                }
                _ => {}
            },
            // A lagging receiver skips ahead — 256 buffered events per turn
            // is generous, so falling behind just keeps watching.
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => {
                return Err(anyhow!("pi event stream closed before the prompt settled"))
            }
        }
    }
}

async fn handle_cmd(core: RpcCore, cmd: Cmd) {
    match cmd {
        Cmd::Initialize(reply) => {
            // pi has no initialize phase: `get_state` doubles as the
            // handshake and yields the initial session state.
            let result = core.request(serde_json::json!({"type": "get_state"})).await;
            let _ = reply.send(result);
        }
        Cmd::NewSession { cwd, reply } => {
            // pi sessions are process-global; `cwd` exists only for
            // signature parity with AcpConn. `new_session` takes no
            // parameters and returns only `{cancelled}` — the session id
            // comes from a follow-up `get_state`.
            let _ = cwd;
            let result = match core
                .request(serde_json::json!({"type": "new_session"}))
                .await
            {
                Err(e) => Err(e),
                Ok(data)
                    if data.get("cancelled").and_then(|c| c.as_bool()) == Some(true) =>
                {
                    Err(anyhow!("pi new_session was cancelled by an extension"))
                }
                Ok(_) => match core.request(serde_json::json!({"type": "get_state"})).await {
                    Err(e) => Err(e),
                    Ok(state) => extract_session_id(&state)
                        .ok_or_else(|| anyhow!("pi get_state returned no session id")),
                },
            };
            let _ = reply.send(result);
        }
        Cmd::Prompt {
            session_id,
            text,
            reply,
        } => {
            let _ = session_id; // parity arg; pi tracks one session per process
            // Subscribe *before* the request so the settling event can't
            // slip past between the response and the watch.
            let mut events = core.event_tx.subscribe();
            let result = match core
                .request(serde_json::json!({"type": "prompt", "message": text}))
                .await
            {
                Err(e) => Err(e),
                // `handled` means an extension consumed the prompt — no
                // run starts, so waiting for `agent_settled` would hang.
                Ok(data)
                    if data.get("disposition").and_then(|d| d.as_str())
                        == Some("handled") =>
                {
                    Ok(())
                }
                // `started`/`queued`/unspecified: a run is (or will be)
                // active — resolve when pi says the session settled.
                Ok(_) => wait_for_settled(&mut events).await,
            };
            let _ = reply.send(result);
        }
        Cmd::Cancel {
            session_id,
            reply,
        } => {
            let _ = session_id; // parity arg; pi tracks one session per process
            // `abort` waits for the session to go idle before responding.
            let result = core
                .request(serde_json::json!({"type": "abort"}))
                .await
                .map(|_| ());
            let _ = reply.send(result);
        }
        // Handled in the command loop itself.
        Cmd::Shutdown(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing trap, pinned at the splitter: U+2028/U+2029 bytes are
    /// payload, not boundaries; `\r\n` is one terminator.
    #[test]
    fn take_record_splits_only_on_lf() {
        let mut buf = "{\"a\":\"x\u{2028}y\"}\r\n{\"b\":\"z\u{2029}w\"}\nrest"
            .as_bytes()
            .to_vec();
        let r1 = take_record(&mut buf).unwrap();
        assert_eq!(r1, "{\"a\":\"x\u{2028}y\"}".as_bytes(), "CR stripped, U+2028 kept");
        let r2 = take_record(&mut buf).unwrap();
        assert_eq!(r2, "{\"b\":\"z\u{2029}w\"}".as_bytes(), "U+2029 kept inside record");
        // "rest" has no LF yet — it must wait.
        assert!(take_record(&mut buf).is_none());
        buf.extend_from_slice(b"\n");
        assert_eq!(take_record(&mut buf).unwrap(), b"rest");
        assert!(take_record(&mut buf).is_none());
    }

    #[test]
    fn extract_session_id_falls_back_through_known_keys() {
        assert_eq!(
            extract_session_id(&serde_json::json!({"sessionId": "abc", "sessionName": "n"})),
            Some("abc".into())
        );
        assert_eq!(
            extract_session_id(&serde_json::json!({"sessionName": "named"})),
            Some("named".into())
        );
        assert_eq!(
            extract_session_id(&serde_json::json!({"sessionFile": "/tmp/s.jsonl"})),
            Some("/tmp/s.jsonl".into())
        );
        assert_eq!(extract_session_id(&serde_json::json!({})), None);
        assert_eq!(
            extract_session_id(&serde_json::json!({"sessionId": ""})),
            None,
            "empty strings are skipped"
        );
    }
}
