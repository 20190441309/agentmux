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
//!   `agent_settled` record (pi's "no remaining automatic work" signal)
//!   via the dedicated `SettledWatch` — deliberately not the lossy
//!   broadcast bus, which can drop records under lag. `disposition ==
//!   "handled"` means an extension consumed the prompt and no run
//!   starts, so it resolves immediately. `session_id` is unused — pi
//!   tracks one session per process.
//! - `cancel(session_id)` — sends `abort`, which waits for the session to
//!   go idle before responding.
//!
//! Events stream onto the broadcast bus *normalized* by
//! [`PiTranslator`]: pi records that map cleanly onto the ACP-shaped
//! `sessionUpdate` vocabulary (`message_update` text deltas →
//! `agent_message_chunk`, `tool_execution_*` → `tool_call` /
//! `tool_call_update`, a completed file edit →
//! [`EventKind::FileEdited`]) arrive as `SessionUpdate`s downstream
//! consumers already understand, with the original record preserved
//! under a `"pi"` key. Everything else (`agent_start`, `turn_end`,
//! `agent_settled`, extension/retry records, …) passes through as a
//! `SessionUpdate` holding the raw pi JSON — see [`crate::pi_shape`]
//! for the helpers consumers use to read those. Process exit produces
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
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    sync::{broadcast, mpsc, oneshot, Notify},
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
            id: value.get("id").and_then(|i| i.as_str()).map(str::to_owned),
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

/// Stateful pi-record → [`EventKind`] normalizer — the translation half
/// of this adapter. One instance lives in the stdout `read_loop` so
/// records that reference earlier ones (`tool_execution_end` carries no
/// `args`, only the `toolCallId` minted at `start`) resolve correctly.
///
/// What maps onto the existing ACP-shaped consumer vocabulary:
///
/// - `message_update` + `assistantMessageEvent.type == "text_delta"` →
///   `SessionUpdate({"sessionUpdate":"agent_message_chunk",
///   "content":{"type":"text","text":<delta>}})`
/// - `thinking_delta` → same shape with `agent_thought_chunk`
/// - `tool_execution_start` → `SessionUpdate({"sessionUpdate":
///   "tool_call", "toolCallId", "title", "status":"in_progress",
///   "locations":[{"path"}]?})`
/// - `tool_execution_update` → `tool_call_update` (`in_progress`)
/// - `tool_execution_end` → `tool_call_update` (`completed`/`failed`),
///   plus an [`EventKind::FileEdited`] when the tracked tool is
///   file-editing (`edit`/`write`/…, see [`pi_shape::tool_edits_file`]),
///   a path was captured from `args`, and `isError` is not true
///
/// Every normalized update keeps the original pi record under a `"pi"`
/// key — nothing is lost, but `update.get("update")` envelope-unwraps in
/// consumers must never see it (hence `"pi"`, not `"update"`).
///
/// Everything else passes through untouched as `SessionUpdate(raw)` —
/// `agent_start`/`turn_*`/`message_*`/`agent_end`/`agent_settled`/
/// `bash_execution_update`/`queue_update`/retry/compaction/extension
/// records have no ACP `sessionUpdate` counterpart, and consumers read
/// them via [`crate::pi_shape`]. `agent_settled` in particular MUST keep
/// passing through: [`SettledWatch`] detects it on the raw record before
/// translation, but bus subscribers (and the persisted log) still see
/// the real record.
///
/// Command *responses* never reach here — [`classify_line`] routes them
/// to the pending map first.
#[derive(Default)]
pub struct PiTranslator {
    /// `toolCallId` → tool info captured at `tool_execution_start`,
    /// so a path-less `tool_execution_end` can still attribute the
    /// completed edit. Entries drop on their `end`.
    tools: HashMap<String, TrackedTool>,
}

/// A tool call tracked between `tool_execution_start` and `…_end`.
struct TrackedTool {
    name: String,
    path: Option<PathBuf>,
}

impl PiTranslator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one decoded pi *event* object into the [`EventKind`]s it
    /// represents — usually one; a `tool_execution_end` for a completed
    /// file edit yields the `tool_call_update` AND a `FileEdited`.
    pub fn event_kinds(&mut self, value: &Value) -> Vec<EventKind> {
        use crate::pi_shape as ps;
        match ps::kind(value) {
            Some("message_update") => {
                match ps::delta(value) {
                    Some((ps::Delta::Message, text)) => vec![EventKind::SessionUpdate(
                        chunk_update("agent_message_chunk", text, value),
                    )],
                    Some((ps::Delta::Thought, text)) => vec![EventKind::SessionUpdate(
                        chunk_update("agent_thought_chunk", text, value),
                    )],
                    // text_start/end, thinking_*_end, toolcall_*, done,
                    // error — message plumbing; pass through.
                    None => vec![EventKind::SessionUpdate(value.clone())],
                }
            }
            Some("tool_execution_start") => vec![self.tool_execution_start(value)],
            Some("tool_execution_update") => {
                vec![self.tool_execution_update(value)]
            }
            Some("tool_execution_end") => self.tool_execution_end(value),
            _ => vec![EventKind::SessionUpdate(value.clone())],
        }
    }

    /// `tool_execution_start` → a `tool_call` update; records the call so
    /// its `end` can resolve the edited path.
    fn tool_execution_start(&mut self, value: &Value) -> EventKind {
        use crate::pi_shape as ps;
        let name = ps::tool_name(value).unwrap_or("tool").to_string();
        let path = ps::tool_path(value).map(PathBuf::from);
        if let Some(id) = ps::tool_call_id(value) {
            self.tools.insert(
                id.to_string(),
                TrackedTool {
                    name: name.clone(),
                    path: path.clone(),
                },
            );
        }
        let title = match &path {
            Some(p) => format!("{name} {}", p.display()),
            None => name.clone(),
        };
        let mut update = serde_json::json!({
            "sessionUpdate": "tool_call",
            "title": title,
            "status": "in_progress",
            "pi": value,
        });
        if let Some(id) = ps::tool_call_id(value) {
            update["toolCallId"] = Value::String(id.to_string());
        }
        if let Some(p) = &path {
            update["locations"] = serde_json::json!([{"path": p.display().to_string()}]);
        }
        EventKind::SessionUpdate(update)
    }

    /// `tool_execution_update` → a `tool_call_update`; refreshes the
    /// tracked path when `args` carries one (progress events keep args).
    fn tool_execution_update(&mut self, value: &Value) -> EventKind {
        use crate::pi_shape as ps;
        if let (Some(id), Some(p)) = (ps::tool_call_id(value), ps::tool_path(value)) {
            if let Some(t) = self.tools.get_mut(id) {
                t.path = Some(PathBuf::from(p));
            }
        }
        let mut update = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "status": "in_progress",
            "pi": value,
        });
        self.fill_tool_fields(&mut update, value);
        EventKind::SessionUpdate(update)
    }

    /// `tool_execution_end` → a `tool_call_update` (completed/failed) —
    /// plus [`EventKind::FileEdited`] when a file-editing tool finished
    /// successfully on a known path.
    fn tool_execution_end(&mut self, value: &Value) -> Vec<EventKind> {
        use crate::pi_shape as ps;
        let failed = ps::tool_end_failed(value);
        let call_id = ps::tool_call_id(value).map(str::to_owned);
        let tracked = call_id.as_ref().and_then(|id| self.tools.remove(id));

        // Path and edit-ness come from the tracked `start` first (end
        // records carry no `args`), then the record itself — some pi
        // versions may still ship args on `end`, and a missed `start`
        // (broadcast lag, log replay) shouldn't lose the edit.
        let path = tracked
            .as_ref()
            .and_then(|t| t.path.clone())
            .or_else(|| ps::tool_path(value).map(PathBuf::from));
        let name = tracked
            .map(|t| t.name)
            .or_else(|| ps::tool_name(value).map(str::to_owned));
        let edited =
            !failed && path.is_some() && name.as_deref().map(ps::tool_edits_file).unwrap_or(false);

        let mut update = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "status": if failed { "failed" } else { "completed" },
            "pi": value,
        });
        if let Some(name) = &name {
            let title = match &path {
                Some(p) => format!("{name} {}", p.display()),
                None => name.clone(),
            };
            update["title"] = Value::String(title);
        }
        if let Some(id) = &call_id {
            update["toolCallId"] = Value::String(id.clone());
        }
        let mut kinds = vec![EventKind::SessionUpdate(update)];
        if edited {
            kinds.push(EventKind::FileEdited {
                path: path.expect("checked above"),
            });
        }
        kinds
    }

    /// Copy `toolCallId`/`title`(from `toolName`) onto a normalized
    /// `tool_call_update`, preferring tracked-start info.
    fn fill_tool_fields(&self, update: &mut Value, value: &Value) {
        use crate::pi_shape as ps;
        let id = ps::tool_call_id(value).map(str::to_owned);
        let name = id
            .as_ref()
            .and_then(|id| self.tools.get(id))
            .map(|t| t.name.clone())
            .or_else(|| ps::tool_name(value).map(str::to_owned));
        if let Some(id) = id {
            update["toolCallId"] = Value::String(id);
        }
        if let Some(name) = name {
            let path = ps::tool_path(value).map(PathBuf::from);
            let title = match &path {
                Some(p) => format!("{name} {}", p.display()),
                None => name,
            };
            update["title"] = Value::String(title);
        }
    }

    /// Translate one raw stdout line into bus [`Event`]s:
    ///
    /// - a pi event record → [`Self::event_kinds`] output (one or more)
    /// - a command response (`type:"response"`) → empty: responses are
    ///   routed to the pending-command map, never onto the bus
    /// - blank/garbage/non-object lines → empty
    ///
    /// The returned events carry a nil `session_id` placeholder —
    /// callers broadcasting them must stamp their connection's real
    /// session id first — and `seq` 0 (the orchestrator assigns real
    /// sequence numbers).
    ///
    /// The framing guarantee is that `line` is already one complete
    /// LF-delimited record — a line containing raw U+2028/U+2029 inside
    /// a string payload translates as a single record, unsplit.
    pub fn translate_line(&mut self, line: &str) -> Vec<Event> {
        match classify_line(line) {
            Classified::Event(value) => self
                .event_kinds(&value)
                .into_iter()
                .map(|kind| Event {
                    session_id: SessionId(Uuid::nil()),
                    seq: 0,
                    ts: Utc::now(),
                    kind,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// An `agent_*_chunk` update carrying a pi delta's text, with the source
/// record preserved under `"pi"`.
fn chunk_update(kind: &str, delta: &str, raw: &Value) -> Value {
    serde_json::json!({
        "sessionUpdate": kind,
        "content": {"type": "text", "text": delta},
        "pi": raw,
    })
}

/// Translate one raw stdout line into bus [`Event`]s — a stateless
/// convenience wrapper around [`PiTranslator::translate_line`] for tests
/// and one-shot digs. Because a fresh translator has no `toolCallId`
/// history, a lone `tool_execution_end` can only emit `FileEdited` when
/// the record itself carries `args`.
pub fn translate_line(line: &str) -> Vec<Event> {
    PiTranslator::new().translate_line(line)
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

/// Turn-completion signaling shared between the stdout reader and prompt
/// waiters — deliberately independent of the lossy broadcast ring.
///
/// The event bus (`broadcast`, capacity 256) exists for *consumers*, who
/// tolerate lag. A prompt's end-of-turn wait must NOT ride it: a burst of
/// over-256 `message_update` deltas inside one read chunk can overflow
/// the ring and a `Lagged` receiver silently skips the dropped range — if
/// a settling record were ever skipped, `prompt` would hang until process
/// exit. Instead the reader bumps `gen` every time it sees
/// `agent_settled` and wakes `notify`; waiters compare against the
/// generation they snapshotted *before* sending their prompt, so a settle
/// from history cannot resolve them early.
struct SettledWatch {
    /// How many `agent_settled` records the reader has seen so far.
    gen: AtomicU64,
    /// The stdout reader ended (EOF or read error) — no settle can arrive.
    reader_done: AtomicBool,
    /// Wakes waiters when `gen` advances or `reader_done` flips.
    notify: Notify,
}

impl SettledWatch {
    fn new() -> Self {
        Self {
            gen: AtomicU64::new(0),
            reader_done: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// Generation snapshot taken before sending a prompt.
    fn snapshot(&self) -> u64 {
        self.gen.load(Ordering::SeqCst)
    }

    /// The reader saw an `agent_settled` record.
    fn note_settled(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// The stdout reader is done — settle will never arrive again.
    fn note_reader_done(&self) {
        self.reader_done.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_done(&self) -> bool {
        self.reader_done.load(Ordering::SeqCst)
    }
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
    ///
    /// Interior-mutable so [`PiConn::close`]/[`PiConn::shutdown`] work on
    /// `&self`: an `Arc`'d connection shared with an in-flight prompt must
    /// still be stoppable (the orchestrator's kill path).
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<Cmd>>>,
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
            cmd_tx: Mutex::new(Some(cmd_tx)),
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
    ///
    /// All request methods take `&self`: they only forward a command over
    /// the worker channel, so a shared (`Arc`) connection supports e.g.
    /// `cancel` while a `prompt` is in flight.
    pub async fn initialize(&self) -> Result<Value> {
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
    pub async fn new_session(&self, cwd: &Path) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::NewSession {
            cwd: cwd.to_path_buf(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("pi connection closed before new_session completed"))?
    }

    /// Send a user prompt; resolves when the agent settles (the
    /// `agent_settled` record — pi's "no remaining automatic work"
    /// signal, matching ACP's end-of-turn resolution). A
    /// `disposition:"handled"` response resolves immediately since no
    /// run starts.
    ///
    /// `session_id` is unused: pi tracks one session per process.
    ///
    /// Assumption (v1): the orchestrator serializes prompt/cancel per
    /// session. A stray `agent_settled` emitted between this call and
    /// the run's start — e.g. a concurrent `abort` — can resolve the
    /// wait early; pi events carry no run id to disambiguate.
    pub async fn prompt(&self, session_id: &str, text: String) -> Result<()> {
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
    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Cancel {
            session_id: session_id.to_string(),
            reply: tx,
        })?;
        rx.await
            .map_err(|_| anyhow!("pi connection closed before abort completed"))?
    }

    /// Pi's RPC protocol has no `session/request_permission` counterpart —
    /// agents on this adapter never emit `PermissionRequest` events, so
    /// there is never a parked request to answer. Kept as an explicit
    /// method (rather than the enum dispatching an inline error) so the
    /// unsupported case has one clean, greppable home.
    pub fn respond_permission(
        &self,
        request_id: &str,
        _decision: crate::PermissionDecision,
    ) -> Result<()> {
        Err(anyhow!(
            "pi sessions do not support permission responses (request {request_id})"
        ))
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

    /// Force-close the command channel: the worker loop ends and runtime
    /// teardown kills the child (`kill_on_drop`). Synchronous, instant and
    /// idempotent — the orchestrator's kill path, usable on a shared
    /// (`Arc`) connection even while a prompt is in flight. Pending
    /// requests resolve with an error once the worker exits.
    ///
    /// Unlike [`PiConn::shutdown`] this does not wait for the worker to
    /// acknowledge or join its thread; use `shutdown` when a clean,
    /// awaited teardown is wanted and `&mut` access is available.
    pub fn close(&self) {
        self.cmd_tx.lock().unwrap().take();
    }

    /// Shut the connection down: the worker thread exits and the child
    /// process is killed (`kill_on_drop`). Idempotent.
    pub async fn shutdown(&mut self) -> Result<()> {
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
            .lock()
            .unwrap()
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
        // (Even if the lock were poisoned, field destruction would drop the
        // sender anyway, closing the channel — this just does it early.)
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
    /// Turn-completion signal fed by the stdout reader — `prompt` waits on
    /// this, not on the lossy broadcast ring.
    watch: Arc<SettledWatch>,
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

        let watch = Arc::new(SettledWatch::new());
        let core = RpcCore {
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            watch: watch.clone(),
        };

        // Drive the stdout record loop in the background; on EOF it fails
        // every pending command so callers never hang.
        tokio::spawn(read_loop(
            stdout,
            core.pending.clone(),
            watch,
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
/// map, events to the bus, `agent_settled` to the [`SettledWatch`]. On
/// EOF, fail all pending commands and all settle waiters.
async fn read_loop(
    mut stdout: ChildStdout,
    pending: PendingMap,
    watch: Arc<SettledWatch>,
    event_tx: broadcast::Sender<Event>,
    session_id: SessionId,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    // One translator per stream: toolCallId correlation spans records.
    let mut translator = PiTranslator::new();
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(rec) = take_record(&mut buf) {
                    handle_record(
                        &rec,
                        &pending,
                        &watch,
                        &mut translator,
                        &event_tx,
                        session_id,
                    );
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
        handle_record(
            &buf,
            &pending,
            &watch,
            &mut translator,
            &event_tx,
            session_id,
        );
    }
    // The agent is gone: no response will ever arrive for these ids.
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(anyhow!("pi process closed stdout")));
    }
    // …and no `agent_settled` will ever arrive either.
    watch.note_reader_done();
}

/// Dispatch one decoded record: response → pending map, event → bus (and
/// `agent_settled` → [`SettledWatch`]), junk → an `Orchestrator` note
/// (pi's stdout is reserved for JSONL, so anomalies deserve to be
/// visible).
fn handle_record(
    rec: &[u8],
    pending: &PendingMap,
    watch: &SettledWatch,
    translator: &mut PiTranslator,
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
                        EventKind::Orchestrator(format!("pi response for unknown id {id}")),
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
            // Settle detection runs on the RAW record, before
            // translation: `agent_settled` passes through unchanged, but
            // the signal must not depend on what the translator emits.
            if value.get("type").and_then(|t| t.as_str()) == Some("agent_settled") {
                watch.note_settled();
            }
            for kind in translator.event_kinds(&value) {
                emit(event_tx, session_id, kind);
            }
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

/// Wait for `agent_settled` — pi's signal that the session-level run has
/// no remaining automatic work (the closest analog of ACP's prompt
/// end-of-turn) — via the dedicated [`SettledWatch`], NOT the broadcast
/// bus. The bus is lossy under lag; this signal is not.
///
/// `since` is the generation snapshotted before the prompt was sent: only
/// a settle *after* that point resolves the wait. A settle that lands
/// between snapshot and our own run's start (e.g. `abort` racing a
/// `prompt`) can still resolve early — pi events carry no run id to
/// disambiguate. v1 accepts this: the orchestrator serializes
/// prompt/cancel per session (Task 9), so `since` can only be beaten by
/// our own run.
///
/// Terminal condition: the stdout reader finishing (`reader_done`) means
/// no settle will ever arrive → `Err`.
async fn wait_for_settled(watch: &SettledWatch, since: u64) -> Result<()> {
    loop {
        if watch.gen.load(Ordering::SeqCst) > since {
            return Ok(());
        }
        if watch.is_done() {
            return Err(anyhow!("pi agent exited before the prompt settled"));
        }
        // Register the waiter *before* re-checking so a notification
        // landing between check and await cannot be missed.
        let notified = watch.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if watch.gen.load(Ordering::SeqCst) > since {
            return Ok(());
        }
        if watch.is_done() {
            return Err(anyhow!("pi agent exited before the prompt settled"));
        }
        notified.await;
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
                Ok(data) if data.get("cancelled").and_then(|c| c.as_bool()) == Some(true) => {
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
                                // Snapshot the settle generation *before* the request so only
                                // a settle from our own run can resolve the wait — settles
                                // already in history are excluded (see `wait_for_settled` for
                                // the documented concurrent-abort caveat).
            let since = core.watch.snapshot();
            let result = match core
                .request(serde_json::json!({"type": "prompt", "message": text}))
                .await
            {
                Err(e) => Err(e),
                // `handled` means an extension consumed the prompt — no
                // run starts, so waiting for `agent_settled` would hang.
                Ok(data) if data.get("disposition").and_then(|d| d.as_str()) == Some("handled") => {
                    Ok(())
                }
                // `started`/`queued`/unspecified: a run is (or will be)
                // active — resolve when pi says the session settled.
                Ok(_) => wait_for_settled(&core.watch, since).await,
            };
            let _ = reply.send(result);
        }
        Cmd::Cancel { session_id, reply } => {
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
        assert_eq!(
            r1,
            "{\"a\":\"x\u{2028}y\"}".as_bytes(),
            "CR stripped, U+2028 kept"
        );
        let r2 = take_record(&mut buf).unwrap();
        assert_eq!(
            r2,
            "{\"b\":\"z\u{2029}w\"}".as_bytes(),
            "U+2029 kept inside record"
        );
        // "rest" has no LF yet — it must wait.
        assert!(take_record(&mut buf).is_none());
        buf.extend_from_slice(b"\n");
        assert_eq!(take_record(&mut buf).unwrap(), b"rest");
        assert!(take_record(&mut buf).is_none());
    }

    /// A settle that happened *before* the snapshot must not resolve the
    /// wait; only a generation advance after it does.
    #[tokio::test]
    async fn settled_watch_resolves_only_on_new_settle() {
        let watch = Arc::new(SettledWatch::new());
        watch.note_settled(); // history — before our prompt's snapshot
        let since = watch.snapshot();

        let w = watch.clone();
        let waiter = tokio::spawn(async move { wait_for_settled(&w, since).await });
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "a settle from before the snapshot must not resolve the wait"
        );

        watch.note_settled(); // our run's settle
        waiter.await.unwrap().unwrap();
    }

    /// If the stdout reader ends without a settle, the wait errors
    /// instead of hanging.
    #[tokio::test]
    async fn settled_watch_errors_when_reader_ends() {
        let watch = Arc::new(SettledWatch::new());
        let w = watch.clone();
        let waiter = tokio::spawn(async move {
            let since = w.snapshot();
            wait_for_settled(&w, since).await
        });
        tokio::task::yield_now().await;

        watch.note_reader_done();
        let err = waiter.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("exited"), "got: {err}");
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
