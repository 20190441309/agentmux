//! The orchestrator — agentmux's session manager, and the heart of the
//! system.
//!
//! [`Orchestrator`] ties together every other piece of `agentmux-core`:
//!
//! - [`Store`] (`Arc<Mutex<_>>`) holds workspace/session metadata and the
//!   per-session JSONL event logs. `Store` is `Send` but `!Sync`
//!   (rusqlite), so the mutex is only ever held for a single synchronous
//!   call — never across `.await`.
//! - [`AgentRegistry`] resolves `agent_id` → [`AgentProfile`] + adapter.
//! - [`WorktreeManager`] + [`collab`] back [`create_workspace`](Self::create_workspace):
//!   a git worktree per workspace plus its `.agentmux/` shared blackboard.
//! - Per session, a spawned [`SpawnedConn`] (ACP or pi RPC) lives in a
//!   [`SessionSlot`]; a fan-out task drains `conn.events()` → assigns the
//!   real `seq` (adapters emit `0`) → appends to the JSONL log →
//!   broadcasts on the global bus → appends to `activity.md` when
//!   [`collab::summarize_event`] produces a summary. `AgentExited` ends
//!   the fan-out with state `Error` (unless a caller-initiated `kill`
//!   already moved the session to `Done`).
//! - [`subscribe`](Self::subscribe) hands out receivers on the global
//!   event bus; clients filter by `Event::session_id` themselves.
//!
//! # Concurrency
//!
//! `prompt` is serialized per session via a `tokio::sync::Mutex` held for
//! the whole turn — `try_lock` failure is the `session busy` error. This
//! is also the serialization `PiConn::prompt` assumes. `cancel`/`kill`
//! deliberately do *not* take that lock: they exist to interrupt a hung
//! prompt, so connections are shared (`Arc<SpawnedConn>`) and the conn
//! wrappers' methods all take `&self`.
//!
//! # Resume semantics
//!
//! `resume` reconnects to the existing native conversation: ACP uses
//! `session/load` when advertised; Pi uses its persisted session file.
//! Unsupported or failed restoration never silently creates a replacement.
//! A setup that never obtained a native identity may retry initial creation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context};
use chrono::Utc;
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::collab;
use crate::config::{ConnTimeouts, SpawnOptions};
use crate::id::{AgentId, ProjectId, SessionId, WorkspaceId};
use crate::model::{
    AdapterKind, AgentProfile, Event, EventKind, PermissionDecision, Project, Session, SessionRef,
    SessionState, Workspace,
};
use crate::registry::AgentRegistry;
use crate::store::Store;
use crate::worktree::WorktreeManager;
use crate::{AcpConn, PiConn, Result};

/// Capacity of the global event bus subscribers read via
/// [`Orchestrator::subscribe`]. Lossy under lag on purpose — consumers
/// tolerate drops; the JSONL log is the authoritative record.
const BUS_CAPACITY: usize = 1024;

/// Setup failed after a session was persisted; clients can resume this record.
#[derive(Debug)]
pub struct SessionSetupFailure {
    pub session_id: SessionId,
    pub reason: String,
}

impl std::fmt::Display for SessionSetupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (session {})", self.reason, self.session_id)
    }
}

impl std::error::Error for SessionSetupFailure {}

/// A live connection to a spawned agent process — either ACP or pi RPC.
///
/// The two adapters share one method surface; the enum delegates so the
/// orchestrator can treat them uniformly. All request methods take
/// `&self` (they only forward over the conn's command channel), which is
/// what lets the orchestrator share a conn across an in-flight `prompt`
/// and a concurrent `cancel`/`kill`.
#[derive(Debug)]
pub enum SpawnedConn {
    Acp(AcpConn),
    Pi(PiConn),
    Native(crate::terminal::NativeTerminal),
}

impl SpawnedConn {
    /// Spawn the adapter process described by `profile`, working in `cwd`.
    /// `options` carries the config-derived connection timeouts.
    pub fn spawn(
        profile: &AgentProfile,
        cwd: &Path,
        options: &SpawnOptions,
    ) -> Result<SpawnedConn> {
        match &profile.adapter {
            AdapterKind::Acp { command, args } => {
                AcpConn::spawn(command, args, &profile.env, cwd, options).map(SpawnedConn::Acp)
            }
            AdapterKind::PiRpc { command, args } => {
                PiConn::spawn(command, args, &profile.env, cwd, options).map(SpawnedConn::Pi)
            }
            AdapterKind::Native { args, .. } => {
                crate::terminal::NativeTerminal::spawn(profile, cwd, args).map(SpawnedConn::Native)
            }
        }
    }

    /// The adapter-internal [`SessionId`] the conn stamps onto emitted
    /// events (each conn mints its own; the orchestrator restamps events
    /// with its real session id, so this is informational only).
    pub fn session_id(&self) -> SessionId {
        match self {
            SpawnedConn::Acp(c) => c.session_id(),
            SpawnedConn::Pi(c) => c.session_id(),
            SpawnedConn::Native(c) => c.session_id(),
        }
    }

    pub async fn initialize(&self) -> Result<serde_json::Value> {
        match self {
            SpawnedConn::Acp(c) => c.initialize().await,
            SpawnedConn::Pi(c) => c.initialize().await,
            SpawnedConn::Native(_) => Ok(serde_json::json!({"native":true})),
        }
    }

    pub async fn new_session(&self, cwd: &Path) -> Result<String> {
        match self {
            SpawnedConn::Acp(c) => c.new_session(cwd).await,
            SpawnedConn::Pi(c) => c.new_session(cwd).await,
            SpawnedConn::Native(_) => bail!("native agents create conversations in their own UI"),
        }
    }

    pub async fn prompt(&self, acp_session_id: &str, text: String) -> Result<()> {
        match self {
            SpawnedConn::Acp(c) => c.prompt(acp_session_id, text).await,
            SpawnedConn::Pi(c) => c.prompt(acp_session_id, text).await,
            SpawnedConn::Native(_) => bail!("open the native terminal to send input to this agent"),
        }
    }

    pub async fn cancel(&self, acp_session_id: &str) -> Result<()> {
        match self {
            SpawnedConn::Acp(c) => c.cancel(acp_session_id).await,
            SpawnedConn::Pi(c) => c.cancel(acp_session_id).await,
            SpawnedConn::Native(c) => c.interrupt(),
        }
    }

    /// Answer a parked `session/request_permission`. Only ACP agents can
    /// emit permission requests; pi sessions get a clean unsupported
    /// error (they have no permission protocol to answer).
    pub fn respond_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        match self {
            SpawnedConn::Acp(c) => c.respond_permission(request_id, decision),
            SpawnedConn::Pi(c) => c.respond_permission(request_id, decision),
            SpawnedConn::Native(_) => bail!("native approvals belong to the agent's own terminal"),
        }
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        match self {
            SpawnedConn::Acp(c) => c.events(),
            SpawnedConn::Pi(c) => c.events(),
            SpawnedConn::Native(c) => c.events(),
        }
    }

    /// Instant, synchronous teardown: closes the worker command channel,
    /// ending the worker loop; runtime teardown kills the child
    /// (`kill_on_drop`). Idempotent.
    pub fn close(&self) {
        match self {
            SpawnedConn::Acp(c) => c.close(),
            SpawnedConn::Pi(c) => c.close(),
            SpawnedConn::Native(c) => c.close(),
        }
    }
}

/// Shared event-ingest machinery, cloned into both the orchestrator's own
/// methods and every per-session fan-out task: assign `seq` → append to
/// the session's JSONL log → broadcast on the global bus.
#[derive(Clone)]
struct EventSink {
    /// `Store` is `Send` but `!Sync`; the lock is held for one synchronous
    /// call at a time, never across `.await`.
    store: Arc<Mutex<Store>>,
    /// The global event bus.
    bus: broadcast::Sender<Event>,
    /// Per-session sequence counters. Adapters emit `seq = 0`; the real
    /// seq is assigned at ingest so the JSONL log is monotonic per
    /// session. Entries persist for the orchestrator's lifetime so `seq`
    /// keeps increasing across `kill`/`resume`.
    seqs: Arc<Mutex<HashMap<SessionId, u64>>>,
}

impl EventSink {
    /// Assign `ev.seq`, append it to the session's JSONL log and broadcast
    /// it — all under one store-lock hold so the JSONL/bus order of
    /// concurrent writers can never invert vs. the order their state
    /// writes landed (same critical section [`transition`] uses).
    /// Persistence is best-effort: a failed append must not wedge the
    /// fan-out — the event still reaches bus subscribers.
    ///
    /// Lock order: `store` is always the outer lock; `seqs` is only ever
    /// taken inside it (via [`Self::next_seq`]).
    fn ingest(&self, ev: &mut Event) {
        let store = self.store.lock().unwrap();
        self.ingest_locked(&store, ev);
    }

    /// [`ingest`](Self::ingest) with the store lock already held.
    fn ingest_locked(&self, store: &Store, ev: &mut Event) {
        ev.seq = self.next_seq(ev.session_id);
        if let Err(e) = store.append_event(ev) {
            // Persistence is best-effort: a failed append must not wedge
            // the fan-out — the seq is consumed either way and the event
            // still broadcasts. No logger in the crate yet, so stderr is
            // where the gap is at least observable.
            eprintln!(
                "agentmux: failed to persist event seq {} for session {}: {e:#}",
                ev.seq, ev.session_id
            );
        }
        let _ = self.bus.send(ev.clone()); // lagging/no subscribers is fine
    }

    /// Next per-session seq. Only called while holding `store`, so
    /// `seqs` is always the inner lock.
    fn next_seq(&self, session_id: SessionId) -> u64 {
        let mut seqs = self.seqs.lock().unwrap();
        let next = seqs.entry(session_id).or_insert(0);
        *next += 1;
        *next
    }

    /// Build + ingest an orchestrator-produced event.
    fn emit(&self, session_id: SessionId, kind: EventKind) -> Event {
        let mut ev = Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind,
        };
        self.ingest(&mut ev);
        ev
    }

    /// Update the session's persisted state and emit a `StateChanged`
    /// event when the state actually changed.
    ///
    /// Returns `true` when the session is in `to` afterwards (transition
    /// applied, or it already was) and `false` when the transition was
    /// skipped.
    ///
    /// `unless_terminal`: skip the transition entirely when the session is
    /// already `Done`/`Error` — used by async epilogues (`prompt`, the
    /// fan-out's `AgentExited` handling) so a racing `kill` or a crash is
    /// never clobbered back to a live state.
    ///
    /// The DB write, seq assignment, JSONL append and the bus send all
    /// happen inside the single store-lock hold: racing `&self`
    /// transitions can never appear out of order on the bus or in the
    /// event log vs. the order the state writes landed.
    fn transition(
        &self,
        session_id: SessionId,
        to: SessionState,
        unless_terminal: bool,
    ) -> Result<bool> {
        let store = self.store.lock().unwrap();
        let session = store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        if session.state == to {
            return Ok(true);
        }
        let terminal = matches!(session.state, SessionState::Done | SessionState::Error(_));
        if unless_terminal && terminal {
            return Ok(false);
        }
        store.update_session_state(session_id, &to)?;
        let mut ev = Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::StateChanged {
                from: session.state,
                to,
            },
        };
        self.ingest_locked(&store, &mut ev);
        Ok(true)
    }

    /// Transition only while the session is still `Done`/`Error` — one
    /// atomic "still resumable" check-and-set under the store lock. This
    /// is what lets `resume` leave a terminal state *without* opening a
    /// TOCTOU window a racing `kill` could be clobbered by.
    fn transition_out_of_terminal(&self, session_id: SessionId, to: SessionState) -> Result<bool> {
        let store = self.store.lock().unwrap();
        let session = store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        if !matches!(session.state, SessionState::Done | SessionState::Error(_)) {
            return Ok(false);
        }
        if session.state == to {
            return Ok(true);
        }
        store.update_session_state(session_id, &to)?;
        let mut ev = Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::StateChanged {
                from: session.state,
                to,
            },
        };
        self.ingest_locked(&store, &mut ev);
        Ok(true)
    }

    /// Transition only while the session is in `from` — one atomic
    /// check-and-set under the store lock. The permission flow uses
    /// this to step `Prompting → WaitingPermission → Prompting`: a
    /// resolved event racing a `cancel`/`kill` (which already moved the
    /// session to `Ready`/`Done`) must not drag it back into `Prompting`.
    fn transition_from(
        &self,
        session_id: SessionId,
        from: SessionState,
        to: SessionState,
    ) -> Result<bool> {
        let store = self.store.lock().unwrap();
        let session = store
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        if session.state != from {
            return Ok(false);
        }
        store.update_session_state(session_id, &to)?;
        let mut ev = Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::StateChanged {
                from: session.state,
                to,
            },
        };
        self.ingest_locked(&store, &mut ev);
        Ok(true)
    }
}

/// A session's live connection plus its adapter-level session id —
/// bundled so `resume` can swap them atomically.
#[derive(Clone)]
struct LiveConn {
    conn: Arc<SpawnedConn>,
    acp_session_id: String,
}

/// Per-session state owned by the orchestrator.
struct SessionSlot {
    /// The current connection. `None` after `kill`, before `connect`
    /// finishes, or after a failed resume. `std::sync::Mutex`, locked only
    /// to clone/swap — never held across `.await`.
    conn: Mutex<Option<LiveConn>>,
    /// Serializes `prompt` per session. Held across the whole turn;
    /// `try_lock` failure is the `session busy` signal.
    prompt_lock: AsyncMutex<()>,
    /// The fan-out task draining `conn.events()`; aborted on `kill`/
    /// `resume` before the conn is dropped.
    fanout: Mutex<Option<JoinHandle<()>>>,
    /// Worktree root — where `.agentmux/` lives.
    worktree_path: PathBuf,
    /// Agent display name, used for `activity.md` entries.
    agent_name: String,
    native_nonce: Mutex<Option<String>>,
    native_reservation: Mutex<Option<(PathBuf, std::time::Instant)>>,
}

impl SessionSlot {
    fn new(worktree_path: PathBuf, agent_name: String) -> SessionSlot {
        SessionSlot {
            conn: Mutex::new(None),
            prompt_lock: AsyncMutex::new(()),
            fanout: Mutex::new(None),
            worktree_path,
            agent_name,
            native_nonce: Mutex::new(None),
            native_reservation: Mutex::new(None),
        }
    }
}

/// The agentmux session orchestrator.
///
/// Cheap to hold behind `Arc`: all shared state is already
/// synchronized (`sessions` is a `Mutex<HashMap<_, Arc<SessionSlot>>>`),
/// so the `&self` methods (`prompt`/`cancel`/`kill`/`subscribe`) are
/// safe to call concurrently.
pub struct Orchestrator {
    sink: EventSink,
    /// `Mutex` so `agent/register` can add profiles at runtime through
    /// `&self`. Held for map operations only, never across `.await`.
    registry: Mutex<AgentRegistry>,
    sessions: Mutex<HashMap<SessionId, Arc<SessionSlot>>>,
    data_dir: PathBuf,
    /// Connection timeouts handed to every spawned conn — captured from
    /// the registry's [`crate::config::Config`] at construction.
    timeouts: ConnTimeouts,
    native_ownership: AsyncMutex<()>,
    native_socket: Mutex<Option<PathBuf>>,
}

impl Orchestrator {
    pub fn set_native_socket(&self, path: PathBuf) {
        *self.native_socket.lock().unwrap() = Some(path);
    }

    fn native_conn(&self, id: SessionId) -> Result<Arc<SpawnedConn>> {
        let slot = self.slot(id)?;
        let conn = slot
            .conn
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.conn.clone())
            .context("native terminal is not running; restore it or open native history")?;
        ensure!(
            matches!(conn.as_ref(), SpawnedConn::Native(_)),
            "this conversation uses a structured adapter"
        );
        Ok(conn)
    }
    pub async fn terminal_attach(
        &self,
        p: crate::rpc::TerminalAttachParams,
    ) -> Result<crate::terminal::TerminalAttached> {
        let conn = self.native_conn(p.session_id)?;
        tokio::task::spawn_blocking(move || match conn.as_ref() {
            SpawnedConn::Native(t) => t.attach(p.rows, p.cols),
            _ => unreachable!(),
        })
        .await?
    }
    pub async fn terminal_read(
        &self,
        p: crate::rpc::TerminalReadParams,
    ) -> Result<crate::terminal::TerminalFrame> {
        let conn = self.native_conn(p.session_id)?;
        match conn.as_ref() {
            SpawnedConn::Native(t) => t.read(&p.token, p.after).await,
            _ => unreachable!(),
        }
    }
    pub async fn terminal_input(&self, p: crate::rpc::TerminalInputParams) -> Result<()> {
        let conn = self.native_conn(p.session_id)?;
        tokio::task::spawn_blocking(move || match conn.as_ref() {
            SpawnedConn::Native(t) => t.input(&p.token, &p.data),
            _ => unreachable!(),
        })
        .await?
    }
    pub async fn terminal_resize(&self, p: crate::rpc::TerminalResizeParams) -> Result<()> {
        let conn = self.native_conn(p.session_id)?;
        tokio::task::spawn_blocking(move || match conn.as_ref() {
            SpawnedConn::Native(t) => t.resize(&p.token, p.rows, p.cols),
            _ => unreachable!(),
        })
        .await?
    }
    pub fn terminal_detach(&self, p: crate::rpc::TerminalDetachParams) -> Result<()> {
        let conn = self.native_conn(p.session_id)?;
        match conn.as_ref() {
            SpawnedConn::Native(t) => t.detach(&p.token),
            _ => unreachable!(),
        }
    }
    fn check_native_owner(&self, file: &Path, ignore: Option<SessionId>) -> Result<()> {
        let file = file.canonicalize()?;
        for session in self.sink.store.lock().unwrap().list_all_sessions()? {
            if Some(session.id) != ignore
                && !matches!(session.state, SessionState::Done | SessionState::Error(_))
            {
                ensure!(!session.native_session_file.as_ref().is_some_and(|p| p.canonicalize().ok().as_ref() == Some(&file)),
                    "native conversation is already running in agent {}; stop it before taking control", session.id);
            }
        }
        for (id, slot) in self.sessions.lock().unwrap().iter() {
            if Some(*id) != ignore {
                ensure!(
                    !slot
                        .native_reservation
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|(p, until)| *until > std::time::Instant::now() && *p == file),
                    "native conversation is being opened by another agent"
                );
            }
        }
        Ok(())
    }
    fn authorize_native_hook(&self, id: SessionId, token: &str) -> Result<Arc<SessionSlot>> {
        let slot = self.slot(id)?;
        ensure!(
            slot.native_nonce.lock().unwrap().as_deref() == Some(token),
            "stale or unauthorized native lifecycle callback"
        );
        let session = self.get_session(id)?.context("native session is missing")?;
        ensure!(
            session.native_terminal
                && !matches!(session.state, SessionState::Done | SessionState::Error(_)),
            "native process is no longer active"
        );
        Ok(slot)
    }
    pub async fn native_check(&self, p: crate::rpc::NativeControlParams) -> Result<()> {
        let _ownership = self.native_ownership.lock().await;
        let slot = self.authorize_native_hook(p.session_id, &p.token)?;
        self.check_native_owner(&p.session_file, Some(p.session_id))?;
        *slot.native_reservation.lock().unwrap() = Some((
            p.session_file.canonicalize()?,
            std::time::Instant::now() + Duration::from_secs(10),
        ));
        Ok(())
    }
    pub async fn native_report(&self, p: crate::rpc::NativeControlParams) -> Result<()> {
        let _ownership = self.native_ownership.lock().await;
        let slot = self.authorize_native_hook(p.session_id, &p.token)?;
        self.check_native_owner(&p.session_file, Some(p.session_id))?;
        let (id, file) = crate::pi_sessions::identity(&p.session_file)?;
        self.sink
            .store
            .lock()
            .unwrap()
            .set_native_session(p.session_id, &id, Some(&file))?;
        *slot.native_reservation.lock().unwrap() = None;
        Ok(())
    }
    pub async fn native_list(&self, id: SessionId) -> Result<crate::rpc::NativeListResult> {
        let session = self.get_session(id)?.context("session is missing")?;
        let profile = self
            .registry
            .lock()
            .unwrap()
            .get(&session.agent_id)
            .cloned()
            .context("agent is not configured")?;
        ensure!(
            matches!(
                profile.adapter,
                AdapterKind::PiRpc { .. }
                    | AdapterKind::Native {
                        session_backend: Some(crate::NativeSessionBackend::Pi),
                        ..
                    }
            ),
            "this adapter's history is available in its native interface"
        );
        let cwd = self
            .get_workspace(session.workspace_id)?
            .context("workspace is missing")?
            .worktree_path;
        let listing = profile.clone();
        let conversations =
            tokio::task::spawn_blocking(move || crate::pi_sessions::list(&listing, &cwd)).await??;
        let registry = self.registry.lock().unwrap();
        let native_available = registry.profiles().iter().any(|a| {
            a.available
                && a.env == profile.env
                && matches!(
                    a.adapter,
                    AdapterKind::Native {
                        session_backend: Some(crate::NativeSessionBackend::Pi),
                        ..
                    }
                )
        });
        let structured_available = registry.profiles().iter().any(|a| {
            a.available && a.env == profile.env && matches!(a.adapter, AdapterKind::PiRpc { .. })
        });
        Ok(crate::rpc::NativeListResult {
            conversations,
            native_available,
            structured_available,
        })
    }
    pub async fn native_open(&self, p: crate::rpc::NativeOpenParams) -> Result<Session> {
        let _ownership = self.native_ownership.lock().await;
        let mut source = self
            .get_session(p.session_id)?
            .context("source conversation is missing")?;
        let profile = self
            .registry
            .lock()
            .unwrap()
            .get(&source.agent_id)
            .cloned()
            .context("source agent is not configured")?;
        ensure!(matches!(profile.adapter, AdapterKind::PiRpc { .. } | AdapterKind::Native { .. }), "this adapter has no portable native conversation identity; create a native agent instead");
        if p.history {
            ensure!(
                source.native_terminal,
                "select a native agent to open its history"
            );
            let id = self
                .create_session_inner(source.workspace_id, &source.agent_id, None, None, true)
                .await?;
            return self
                .get_session(id)?
                .context("native history launcher disappeared");
        }
        let file = p
            .session_file
            .or_else(|| source.native_session_file.clone())
            .context("no native file is selected; open native history first")?;
        let workspace = self
            .get_workspace(source.workspace_id)?
            .context("workspace is missing")?;
        let cwd = workspace.worktree_path.clone();
        let inspect_file = file.clone();
        let imported =
            tokio::task::spawn_blocking(move || crate::pi_sessions::inspect(&inspect_file, &cwd))
                .await??;
        let target_native = p.native;
        let target = if target_native != source.native_terminal {
            self.registry.lock().unwrap().profiles().iter().find(|a| a.available && a.env == profile.env && if target_native {
                matches!(a.adapter, AdapterKind::Native { session_backend: Some(crate::NativeSessionBackend::Pi), .. })
            } else { matches!(a.adapter, AdapterKind::PiRpc { .. }) }).map(|a| a.id.clone()).context("configure compatible Pi profiles with matching environments before switching modes")?
        } else {
            profile.id.clone()
        };
        if source
            .native_session_file
            .as_ref()
            .and_then(|f| f.canonicalize().ok())
            .as_ref()
            == Some(&imported.session_file)
        {
            if target_native == source.native_terminal {
                if matches!(source.state, SessionState::Done | SessionState::Error(_)) {
                    // Ownership is already held here; resume performs the same
                    // checks internally without reacquiring it.
                    drop(_ownership);
                    self.resume(source.id).await?;
                }
                return self
                    .get_session(source.id)?
                    .context("conversation disappeared");
            }
            let slot = self.slot(source.id)?;
            let _turn = slot
                .prompt_lock
                .try_lock()
                .map_err(|_| anyhow!("wait for this agent's current turn before native handoff"))?;
            ensure!(
                !matches!(
                    source.state,
                    SessionState::Prompting
                        | SessionState::WaitingPermission
                        | SessionState::Connecting
                ),
                "agent is busy; native handoff was not performed"
            );
            self.kill(source.id).await?;
            source.state = SessionState::Done;
        }
        self.check_native_owner(&imported.session_file, None)?;
        let title = imported.title.clone();
        let id = self
            .create_session_inner(source.workspace_id, &target, None, Some(imported), false)
            .await?;
        let _ = self.set_session_title(id, title.chars().take(60).collect());
        self.get_session(id)?
            .context("opened conversation disappeared")
    }
    /// Create an orchestrator. `data_dir` is where `store` lives and where
    /// session JSONL logs land (`<data_dir>/sessions/`).
    ///
    /// Boots with a sweep over the persisted session table: rows still in
    /// a non-terminal state are leftovers from a previous daemon life —
    /// their conn and fan-out died with the process — and are marked
    /// `Error("daemon restarted")` so `prompt` doesn't wedge on a phantom
    /// live session and `kill`/`resume` can reach them again.
    pub fn new(store: Store, registry: AgentRegistry, data_dir: PathBuf) -> Orchestrator {
        let (bus, _) = broadcast::channel(BUS_CAPACITY);
        let timeouts = registry.timeouts();
        let orch = Orchestrator {
            sink: EventSink {
                store: Arc::new(Mutex::new(store)),
                bus,
                seqs: Arc::new(Mutex::new(HashMap::new())),
            },
            registry: Mutex::new(registry),
            sessions: Mutex::new(HashMap::new()),
            data_dir,
            timeouts,
            native_ownership: AsyncMutex::new(()),
            native_socket: Mutex::new(None),
        };
        orch.sweep_restarted_sessions();
        orch
    }

    /// Boot sweep over the persisted session table (see [`Self::new`]).
    ///
    /// Two repairs per row:
    ///
    /// - **seq seeding**: the per-session `seqs` counters start empty on
    ///   boot while the JSONL logs persist across restarts — seed each
    ///   counter from the log's max seq so post-restart events keep the
    ///   log's monotonic ordering instead of reusing seqs.
    /// - **zombie sweep**: a non-terminal state (`Ready`, `Prompting`,
    ///   `Connecting`, `Created`, `WaitingPermission`) on disk can only
    ///   describe a conn that no longer exists. Route through
    ///   [`EventSink::transition`] so the fix is itself a persisted
    ///   `StateChanged` event, then `kill`/`resume` work normally.
    ///
    /// Best-effort: a corrupt row or unreadable log is reported on stderr
    /// and skipped — boot must not fail over one bad record.
    fn sweep_restarted_sessions(&self) {
        let sessions = {
            let store = self.sink.store.lock().unwrap();
            match store.list_all_sessions() {
                Ok(sessions) => sessions,
                Err(e) => {
                    eprintln!("agentmux: startup session sweep failed: {e:#}");
                    return;
                }
            }
        };
        for session in sessions {
            // Only the last seq is needed; read the log's tail, not all of it.
            match self.sink.store.lock().unwrap().last_seq(session.id) {
                Ok(max) => {
                    if max > 0 {
                        self.sink.seqs.lock().unwrap().insert(session.id, max);
                    }
                }
                Err(e) => eprintln!(
                    "agentmux: unreadable event log for session {}: {e:#}",
                    session.id
                ),
            }
            if matches!(session.state, SessionState::Done | SessionState::Error(_)) {
                continue;
            }
            if let Err(e) = self.sink.transition(
                session.id,
                SessionState::Error("daemon restarted".into()),
                false,
            ) {
                eprintln!(
                    "agentmux: failed to mark stale session {} dead at boot: {e:#}",
                    session.id
                );
            }
        }
    }

    /// The agentmux data directory handed to [`Orchestrator::new`].
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Subscribe to the global event bus. Clients filter by
    /// `Event::session_id` themselves.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sink.bus.subscribe()
    }

    /// Fetch a session's persisted record.
    pub fn get_session(&self, session_id: SessionId) -> Result<Option<Session>> {
        self.sink.store.lock().unwrap().get_session(session_id)
    }

    /// Fetch a workspace's persisted record.
    pub fn get_workspace(&self, workspace_id: WorkspaceId) -> Result<Option<Workspace>> {
        self.sink.store.lock().unwrap().get_workspace(workspace_id)
    }

    /// List all sessions in a workspace, in insertion order.
    pub fn list_sessions(&self, workspace_id: WorkspaceId) -> Result<Vec<Session>> {
        self.sink.store.lock().unwrap().list_sessions(workspace_id)
    }

    /// Replay a session's persisted event log (seq order).
    pub fn read_events(&self, session_id: SessionId) -> Result<Vec<Event>> {
        self.sink.store.lock().unwrap().read_events(session_id)
    }

    /// Shared, cached view of a session's persisted events (see
    /// [`Store::events`]) — for read paths that page or search history.
    pub fn events(&self, session_id: SessionId) -> Result<Arc<Vec<Event>>> {
        self.sink.store.lock().unwrap().events(session_id)
    }

    pub fn set_session_title(&self, session_id: SessionId, title: String) -> Result<Event> {
        let title = title.trim();
        ensure!(!title.is_empty(), "title must not be empty");
        ensure!(
            title.chars().count() <= 60,
            "title must be at most 60 characters"
        );
        ensure!(
            !title
                .chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')),
            "title must be a single line without control characters"
        );
        let store = self.sink.store.lock().unwrap();
        ensure!(
            store.get_session(session_id)?.is_some(),
            "session not found"
        );
        let event = Event {
            session_id,
            seq: self.sink.next_seq(session_id),
            ts: Utc::now(),
            kind: EventKind::TitleChanged {
                title: title.into(),
            },
        };
        // A successful rename promises persistence, unlike best-effort telemetry.
        store.append_event(&event)?;
        let _ = self.sink.bus.send(event.clone());
        Ok(event)
    }

    /// Number of sessions with a live slot — the `sessions` field of
    /// `server/status`.
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Register `root_path` as a [`Project`] (`project/register`).
    ///
    /// `root_path` must be an existing directory; `name` defaults to the
    /// directory's final component (or the whole path when it has none,
    /// e.g. `/`).
    pub fn register_project(&self, root_path: PathBuf, name: Option<String>) -> Result<Project> {
        ensure!(
            root_path.is_dir(),
            "project root {} is not an existing directory",
            root_path.display()
        );
        let name = name
            .or_else(|| {
                root_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| root_path.display().to_string());
        let project = Project {
            id: ProjectId::new(),
            root_path,
            name,
        };
        self.sink.store.lock().unwrap().insert_project(&project)?;
        Ok(project)
    }

    /// All registered projects (`project/list`).
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        self.sink.store.lock().unwrap().list_projects()
    }

    /// Remove a project (`project/remove`). Fails while it still owns
    /// workspaces — remove them first via [`remove_workspace`](Self::remove_workspace).
    /// Returns `false` when no project with that id existed.
    pub fn remove_project(&self, project_id: ProjectId) -> Result<bool> {
        let store = self.sink.store.lock().unwrap();
        let workspaces = store.list_workspaces(project_id)?;
        ensure!(
            workspaces.is_empty(),
            "project {project_id} still has {} workspace(s); remove them first",
            workspaces.len()
        );
        store.delete_project(project_id)
    }

    /// All workspaces of a project (`workspace/list`).
    pub fn list_workspaces(&self, project_id: ProjectId) -> Result<Vec<Workspace>> {
        self.sink.store.lock().unwrap().list_workspaces(project_id)
    }
    pub fn open_workspace(&self, project_id: ProjectId) -> Result<Workspace> {
        let store = self.sink.store.lock().unwrap();
        if let Some(workspace) = store
            .list_workspaces(project_id)?
            .into_iter()
            .find(|w| !w.managed_worktree)
        {
            return Ok(workspace);
        }
        let project = store
            .get_project(project_id)?
            .context("project is not registered")?;
        let workspace = Workspace {
            id: WorkspaceId::new(),
            project_id,
            name: project.name,
            worktree_path: project.root_path.canonicalize()?,
            branch: "project directory".into(),
            managed_worktree: false,
            created_at: Utc::now(),
        };
        store.insert_workspace(&workspace)?;
        Ok(workspace)
    }

    /// Remove a workspace (`workspace/remove`): removes the git worktree,
    /// then — under a single store lock — re-verifies no live sessions and
    /// deletes session rows + the workspace record.
    ///
    /// Refuses while any of its sessions is live (not `Done`/`Error`) —
    /// kill them first. Returns `false` when no workspace with that id
    /// existed. Worktree removal runs before the record delete so a git
    /// failure leaves the record intact and retryable.
    ///
    /// Race with `session/create`/`resume`: the post-worktree re-check and
    /// the row deletes share one store-lock hold, so a racing insert
    /// either lands before the hold (seen as live → refusal; its spawn
    /// into the now-deleted worktree fails to `Error`) or after the
    /// workspace row is gone (FK-violates the insert — nothing spawned).
    /// A create that was already mid-connect when its row is deleted fails
    /// its `Ready` transition and tears itself down there.
    pub fn remove_workspace(&self, workspace_id: WorkspaceId) -> Result<bool> {
        let workspace = {
            let store = self.sink.store.lock().unwrap();
            let Some(workspace) = store.get_workspace(workspace_id)? else {
                return Ok(false);
            };
            let sessions = store.list_sessions(workspace_id)?;
            Self::ensure_no_live_sessions(workspace_id, &sessions)?;
            workspace
        };

        let repo_root = {
            let store = self.sink.store.lock().unwrap();
            store
                .get_project(workspace.project_id)?
                .ok_or_else(|| anyhow!("project {} is gone", workspace.project_id))?
                .root_path
        };
        // Tolerate a missing worktree path — a previous attempt may have
        // partially cleaned up, and the record should still be removable.
        if workspace.managed_worktree && workspace.worktree_path.exists() {
            WorktreeManager::remove(&repo_root, &workspace.worktree_path)?;
        }

        // Atomic re-check + delete (see doc comment): the gap between the
        // first live-check and this hold is where `session/create` could
        // have slipped in — catch it here, not after the rows are gone.
        let session_ids = {
            let store = self.sink.store.lock().unwrap();
            let sessions = store.list_sessions(workspace_id)?;
            Self::ensure_no_live_sessions(workspace_id, &sessions)?;
            let ids = store.delete_workspace_sessions(workspace_id)?;
            store.delete_workspace(workspace_id)?;
            ids
        };

        // Detach any lingering slots (e.g. a dead conn an Error session
        // never closed) and drop their event logs.
        for session_id in &session_ids {
            if let Some(slot) = self.sessions.lock().unwrap().remove(session_id) {
                Self::teardown_slot(&slot);
            }
        }
        {
            let store = self.sink.store.lock().unwrap();
            for session_id in session_ids {
                let _ = store.delete_event_log(session_id); // best-effort
                let _ = store.delete_stderr_log(session_id); // best-effort
            }
        }
        Ok(true)
    }

    fn ensure_no_live_sessions(workspace_id: WorkspaceId, sessions: &[Session]) -> Result<()> {
        let live = sessions
            .iter()
            .filter(|s| !matches!(s.state, SessionState::Done | SessionState::Error(_)))
            .count();
        ensure!(
            live == 0,
            "workspace {workspace_id} still has {live} live session(s); kill them first"
        );
        Ok(())
    }

    /// All configured agents with availability freshly probed
    /// (`agent/list`).
    ///
    /// The probe is written back into the registry, so what `agent/list`
    /// reports is what `create_session`/`resume` will enforce — an agent
    /// installed after daemon boot becomes usable on the next list.
    pub fn list_agents(&self) -> Vec<AgentProfile> {
        let mut registry = self.registry.lock().unwrap();
        let probed = registry.probe();
        for profile in &probed {
            registry.register(profile.clone());
        }
        probed
    }

    /// Register (or update) an agent profile (`agent/register`).
    ///
    /// `available` is recomputed by probing `command` — the value the
    /// caller supplied is ignored. The profile is also upserted into the
    /// store so a restart could rehydrate it (config files remain the
    /// primary source on boot).
    pub fn register_agent(&self, profile: AgentProfile) -> Result<AgentProfile> {
        let probed = AgentRegistry::probe_profile(&profile);
        self.registry.lock().unwrap().register(probed.clone());
        self.sink.store.lock().unwrap().upsert_agent(&probed)?;
        Ok(probed)
    }

    /// Look up a session's live slot.
    fn slot(&self, session_id: SessionId) -> Result<Arc<SessionSlot>> {
        self.sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow!("session {session_id} is not running"))
    }

    /// Create a workspace: git worktree + `.agentmux/` blackboard + store
    /// record. On failure after the worktree exists, it is removed so a
    /// failed workspace leaves no residue.
    pub async fn create_workspace(
        &self,
        project_id: ProjectId,
        name: &str,
        base: &str,
    ) -> Result<WorkspaceId> {
        let project = {
            self.sink
                .store
                .lock()
                .unwrap()
                .get_project(project_id)?
                .ok_or_else(|| anyhow!("unknown project {project_id}"))?
        };

        let (worktree_path, branch) = WorktreeManager::create(&project.root_path, name, base)?;
        let workspace = Workspace {
            id: WorkspaceId::new(),
            project_id,
            name: name.to_string(),
            worktree_path,
            branch,
            managed_worktree: true,
            created_at: Utc::now(),
        };

        let result = collab::init_shared_dir(&workspace.worktree_path)
            .and_then(|_| self.sink.store.lock().unwrap().insert_workspace(&workspace));
        if let Err(e) = result {
            let _ = WorktreeManager::remove(&project.root_path, &workspace.worktree_path);
            return Err(e);
        }
        Ok(workspace.id)
    }

    /// Create a session: look up the agent, spawn its adapter process,
    /// `initialize` + `session/new` in the workspace's worktree, then mark
    /// it `Ready`. `prompt`, when given, is sent as the first turn.
    ///
    /// Errors when the agent id is unknown or `available == false`. On a
    /// mid-setup failure (spawn/initialize/new_session), the child is
    /// killed best-effort and the session record is left in `Error` state
    /// rather than deleted, so the failure is inspectable/resumable.
    pub async fn create_session(
        &self,
        workspace_id: WorkspaceId,
        agent_id: &AgentId,
        prompt: Option<String>,
    ) -> Result<SessionId> {
        self.create_session_inner(workspace_id, agent_id, prompt, None, false)
            .await
    }

    async fn create_session_inner(
        &self,
        workspace_id: WorkspaceId,
        agent_id: &AgentId,
        prompt: Option<String>,
        imported: Option<crate::pi_sessions::NativeConversation>,
        native_history: bool,
    ) -> Result<SessionId> {
        let mut profile = self
            .registry
            .lock()
            .unwrap()
            .get(agent_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown agent {agent_id}"))?;
        ensure!(profile.available, "agent {agent_id} is not available");
        let workspace = self
            .get_workspace(workspace_id)?
            .ok_or_else(|| anyhow!("unknown workspace {workspace_id}"))?;

        let native_terminal = matches!(profile.adapter, AdapterKind::Native { .. });
        ensure!(
            !native_terminal || prompt.is_none(),
            "native agents receive input in their own terminal, not as structured prompts"
        );
        let imported = if let Some(imported) = imported {
            Some(imported)
        } else if !native_history
            && matches!(
                profile.adapter,
                AdapterKind::Native {
                    session_backend: Some(crate::NativeSessionBackend::Pi),
                    ..
                }
            )
        {
            let p = profile.clone();
            let cwd = workspace.worktree_path.clone();
            Some(tokio::task::spawn_blocking(move || crate::pi_sessions::create(&p, &cwd)).await??)
        } else {
            None
        };
        if native_history {
            let AdapterKind::Native {
                args, history_args, ..
            } = &mut profile.adapter
            else {
                bail!("select a native agent to open native history");
            };
            args.extend(history_args.clone());
        }

        let session = Session {
            id: SessionId::new(),
            workspace_id,
            agent_id: agent_id.clone(),
            state: SessionState::Created,
            acp_session_id: imported.as_ref().map(|s| s.session_id.clone()),
            native_session_file: imported.as_ref().map(|s| s.session_file.clone()),
            native_terminal,
            references: vec![],
            created_at: Utc::now(),
        };
        let session_id = session.id;
        {
            self.sink.store.lock().unwrap().insert_session(&session)?;
        }
        let slot = Arc::new(SessionSlot::new(
            workspace.worktree_path.clone(),
            profile.name.clone(),
        ));
        self.sessions
            .lock()
            .unwrap()
            .insert(session_id, slot.clone());

        // `unless_terminal`: an in-process `kill` racing setup owns `Done`
        // already — don't start the connect at all (unreachable via the
        // wire, where the id only exists after this method returns). On a
        // vanished row (`workspace/remove` raced the insert) drop the slot
        // rather than leak it.
        match self
            .sink
            .transition(session_id, SessionState::Connecting, true)
        {
            Ok(true) => {}
            Ok(false) => {
                self.sessions.lock().unwrap().remove(&session_id);
                bail!("session {session_id} was killed during setup");
            }
            Err(e) => {
                self.sessions.lock().unwrap().remove(&session_id);
                return Err(e);
            }
        }
        match self
            .connect(
                &profile,
                &workspace.worktree_path,
                session_id,
                &slot,
                imported.as_ref().map(|_| &session),
            )
            .await
        {
            Ok(()) => {
                // `unless_terminal`: a `kill` landing during `connect`
                // already owns `Done` — tear the fresh conn down instead
                // of resurrecting the session. A missing row means the
                // workspace was removed mid-setup: teardown + drop the
                // slot so no orphan conn survives.
                match self.sink.transition(session_id, SessionState::Ready, true) {
                    Ok(true) => {}
                    Ok(false) => {
                        Self::teardown_slot(&slot);
                        return Ok(session_id);
                    }
                    Err(e) => {
                        Self::teardown_slot(&slot);
                        self.sessions.lock().unwrap().remove(&session_id);
                        return Err(e);
                    }
                }
                if let Some(text) = prompt {
                    // Errors from the initial prompt propagate — the
                    // session record (in `Error` or a settled state) is
                    // still queryable via `list_sessions`.
                    self.prompt(session_id, text, vec![]).await?;
                }
                Ok(session_id)
            }
            Err(e) => {
                let _ = self.sink.transition(
                    session_id,
                    SessionState::Error(format!("setup failed: {e:#}")),
                    false,
                );
                Err(SessionSetupFailure {
                    session_id,
                    reason: format!("{e:#}"),
                }
                .into())
            }
        }
    }

    /// Spawn the adapter + run its handshake; on success `slot` holds the
    /// live conn and the fan-out task is draining events. On failure the
    /// child is closed and `slot.conn` stays `None`.
    async fn connect(
        &self,
        profile: &AgentProfile,
        worktree: &Path,
        session_id: SessionId,
        slot: &SessionSlot,
        previous: Option<&Session>,
    ) -> Result<()> {
        // `SpawnedConn::spawn` blocks briefly on the child-spawn
        // handshake; keep that off the async executor.
        let mut profile = profile.clone();
        let record = self
            .get_session(session_id)?
            .context("session vanished during setup")?;
        if let AdapterKind::Native {
            args, resume_args, ..
        } = &mut profile.adapter
        {
            ensure!(
                record.native_terminal,
                "cannot silently convert a structured conversation to native mode"
            );
            if record.acp_session_id.is_some() || record.native_session_file.is_some() {
                ensure!(
                    !resume_args.is_empty(),
                    "native CLI has no configured resume arguments"
                );
                for arg in resume_args {
                    let mut expanded = arg.clone();
                    if expanded.contains("{session_file}") {
                        expanded = expanded.replace(
                            "{session_file}",
                            record
                                .native_session_file
                                .as_deref()
                                .and_then(Path::to_str)
                                .context("native session file is missing")?,
                        );
                    }
                    if expanded.contains("{session_id}") {
                        expanded = expanded.replace(
                            "{session_id}",
                            record
                                .acp_session_id
                                .as_deref()
                                .context("native session identity is missing")?,
                        );
                    }
                    args.push(expanded);
                }
            } else if previous.is_some_and(
                |s| !matches!(&s.state, SessionState::Error(e) if e.starts_with("setup failed:")),
            ) {
                bail!("this native CLI has no saved restore identity; open native history or create a native agent explicitly");
            }
        } else {
            ensure!(
                !record.native_terminal,
                "cannot silently convert a native conversation to structured mode"
            );
        }
        if matches!(
            profile.adapter,
            AdapterKind::Native {
                session_backend: Some(crate::NativeSessionBackend::Pi),
                ..
            }
        ) {
            if let Some(socket) = self.native_socket.lock().unwrap().clone() {
                let dir = self.data_dir.join("native").join(session_id.to_string());
                std::fs::create_dir_all(&dir)?;
                let bridge = dir.join("agentmux-native.mjs");
                std::fs::write(&bridge, include_str!("pi_native_bridge.mjs"))?;
                let nonce = uuid::Uuid::new_v4().to_string();
                *slot.native_nonce.lock().unwrap() = Some(nonce.clone());
                profile.env.insert(
                    "AGENTMUX_NATIVE_SOCKET".into(),
                    socket
                        .to_str()
                        .context("native socket path is not UTF-8")?
                        .into(),
                );
                profile
                    .env
                    .insert("AGENTMUX_NATIVE_SESSION".into(), session_id.to_string());
                profile.env.insert("AGENTMUX_NATIVE_TOKEN".into(), nonce);
                if let AdapterKind::Native { args, .. } = &mut profile.adapter {
                    args.extend([
                        "-e".into(),
                        bridge
                            .to_str()
                            .context("native bridge path is not UTF-8")?
                            .into(),
                    ]);
                }
            }
        }
        let cwd = worktree.to_path_buf();
        let stderr_log = {
            let store = self.sink.store.lock().unwrap();
            Some(store.stderr_log_path(session_id))
        };
        let options = SpawnOptions {
            timeouts: self.timeouts,
            stderr_log,
        };
        let conn =
            tokio::task::spawn_blocking(move || SpawnedConn::spawn(&profile, &cwd, &options))
                .await
                .context("spawn task failed")??;
        let conn = Arc::new(conn);

        // Grab the replay receiver *before* the handshake so events the
        // worker emits early (e.g. a fast `AgentExited`) are not lost.
        let mut rx = conn.events();

        let restoring =
            previous.is_some_and(|s| s.acp_session_id.is_some() || s.native_session_file.is_some());
        let setup = async {
            let init = conn.initialize().await.context("agent initialize failed")?;
            if matches!(conn.as_ref(), SpawnedConn::Native(_)) {
                return Ok((record.acp_session_id.clone().unwrap_or_default(), record.native_session_file.clone()));
            }
            if restoring {
                let previous = previous.unwrap();
                let id = previous.acp_session_id.as_deref().context("native session identity is missing; original conversation was not replaced")?;
                match conn.as_ref() {
                    SpawnedConn::Acp(acp) => {
                        ensure!(init.pointer("/agentCapabilities/loadSession").and_then(serde_json::Value::as_bool) == Some(true),
                            "this ACP adapter does not support native session restoration; original conversation was not replaced. Create a new conversation explicitly with /mux new");
                        acp.load_session(id, worktree).await?;
                        Ok((id.to_owned(), None))
                    }
                    SpawnedConn::Pi(pi) => {
                        let file = previous.native_session_file.as_deref().context(
                            "this older Pi session has no saved native session file; original conversation was not replaced")?;
                        ensure!(tokio::fs::metadata(file).await.with_context(|| format!("cannot restore Pi session file {}", file.display()))?.is_file(),
                            "Pi session path is not a file: {}", file.display());
                        let state = pi.load_session(file).await?;
                        let restored_id = crate::pi_rpc::extract_session_id(&state).context("Pi restored no session identity")?;
                        ensure!(restored_id == id, "Pi restored a different native session; original conversation was not replaced");
                        Ok((restored_id, Some(file.to_path_buf())))
                    }
                    SpawnedConn::Native(_) => unreachable!(),
                }
            } else {
                match conn.as_ref() {
                    SpawnedConn::Pi(pi) => {
                        let state = pi.new_session_state(worktree).await.context("agent session/new failed")?;
                        let id = crate::pi_rpc::extract_session_id(&state).context("Pi returned no session identity")?;
                        let file = state.get("sessionFile").and_then(serde_json::Value::as_str)
                            .filter(|s| !s.is_empty()).map(PathBuf::from)
                            .map(|p| if p.is_absolute() { p } else { worktree.join(p) });
                        Ok((id, file))
                    }
                    SpawnedConn::Acp(acp) => Ok((acp.new_session(worktree).await.context("agent session/new failed")?, None)),
                    SpawnedConn::Native(_) => unreachable!(),
                }
            }
        }.await;
        let (acp_session_id, native_file) = match setup {
            Ok(identity) => identity,
            Err(e) => {
                conn.close();
                return Err(e);
            }
        };

        // ACP load replays old messages. The persisted workbench history already
        // contains them; do not append a second copy on every reconnection.
        if restoring && !matches!(conn.as_ref(), SpawnedConn::Native(_)) {
            let mut commands_seen = None;
            let mut replay_lagged = false;
            loop {
                match rx.try_recv() {
                    Ok(Event {
                        kind: EventKind::AgentExited { .. },
                        ..
                    }) => {
                        conn.close();
                        bail!("agent exited during native session restoration");
                    }
                    Ok(mut event) => {
                        // Drop replayed transcript chunks, not the new adapter's
                        // command catalog needed by current and reconnecting UIs.
                        if event.available_commands().is_some() {
                            if let EventKind::SessionUpdate(value) = &event.kind {
                                commands_seen = Some(value.clone());
                            }
                            event.session_id = session_id;
                            self.sink.ingest(&mut event);
                        }
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => replay_lagged = true,
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        conn.close();
                        bail!("agent disconnected during native session restoration");
                    }
                }
            }
            if replay_lagged {
                if let SpawnedConn::Acp(acp) = conn.as_ref() {
                    if let Some(value) = acp
                        .command_catalog()
                        .filter(|value| commands_seen.as_ref() != Some(value))
                    {
                        self.sink.ingest(&mut Event {
                            session_id,
                            seq: 0,
                            ts: Utc::now(),
                            kind: EventKind::SessionUpdate(value),
                        });
                    }
                }
            }
        }
        if !matches!(conn.as_ref(), SpawnedConn::Native(_)) {
            if let Err(e) = self.sink.store.lock().unwrap().set_native_session(
                session_id,
                &acp_session_id,
                native_file.as_deref(),
            ) {
                conn.close();
                return Err(e);
            }
        }
        let handle = spawn_fanout(
            self.sink.clone(),
            session_id,
            rx,
            slot.worktree_path.clone(),
            slot.agent_name.clone(),
        );
        *slot.fanout.lock().unwrap() = Some(handle);
        *slot.conn.lock().unwrap() = Some(LiveConn {
            conn,
            acp_session_id,
        });
        Ok(())
    }

    /// Send one prompt turn.
    ///
    /// The session must be live (`Ready`; a stale `Prompting` left by a
    /// dropped caller is retried). The full outgoing text is
    /// `[shared-context preamble] + [refs context] + text`:
    ///
    /// - **preamble**: when the workspace hosts ≥2 sessions and the
    ///   `.agentmux/` blackboard is non-empty, `shared_context_preamble`
    ///   is prepended so siblings can coordinate.
    /// - **refs**: each [`SessionRef`] renders the referenced session's
    ///   event log (`seq <= event_seq`; `0` = everything) as one-line
    ///   summaries appended to the prompt.
    ///
    /// Errors with `session busy` when a prompt is already in flight —
    /// the per-session `prompt_lock` enforces the serialization
    /// `PiConn::prompt` assumes.
    pub async fn prompt(
        &self,
        session_id: SessionId,
        text: String,
        refs: Vec<SessionRef>,
    ) -> Result<()> {
        let session = self
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        if matches!(session.state, SessionState::Done | SessionState::Error(_)) {
            bail!(
                "session {session_id} is not live (state {:?}); resume it first",
                session.state
            );
        }
        // Reject before any transition: failing later would mark a live
        // native PTY session `Error` while its child keeps running.
        ensure!(
            !session.native_terminal,
            "open the native terminal to send input to this agent"
        );
        let slot = self.slot(session_id)?;
        // Serializes turns; a held lock means a prompt is in flight.
        let _permit = slot
            .prompt_lock
            .try_lock()
            .map_err(|_| anyhow!("session busy"))?;
        let live = slot
            .conn
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session {session_id} has no live connection"))?;
        let user_text = text.clone();
        let text = if text.starts_with('/') {
            // Agents recognize commands at byte zero. A collaboration preamble
            // would turn a native command into an ordinary model prompt.
            ensure!(
                refs.is_empty(),
                "Agent slash commands cannot carry quoted session references"
            );
            text
        } else {
            self.compose_prompt(&session, &text, &refs)?
        };

        // `unless_terminal`: a `kill` landing between the liveness check
        // and here already owns the terminal state — a `Prompting`
        // transition must never resurrect it.
        if !self
            .sink
            .transition(session_id, SessionState::Prompting, true)?
        {
            bail!("session {session_id} was terminated");
        }
        // Persist the operator's prompt, without injected shared context, for
        // transcript replay and a stable task title after reopening the TUI.
        self.sink.ingest(&mut Event {
            session_id,
            seq: 0,
            ts: chrono::Utc::now(),
            kind: EventKind::SessionUpdate(serde_json::json!({
                "sessionId": live.acp_session_id,
                "update": {
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": user_text}
                }
            })),
        });
        let result = live.conn.prompt(&live.acp_session_id, text).await;
        match &result {
            Ok(()) => {
                // `unless_terminal`: a racing kill/crash already owns the
                // final state.
                self.sink
                    .transition(session_id, SessionState::Ready, true)?;
            }
            Err(e) => {
                // A failed turn almost always means the connection died
                // (the fan-out's `AgentExited` marks `Error` too —
                // `unless_terminal` keeps the two paths from fighting).
                // A timed-out turn may still be running, though. Give a
                // dying agent's exit path (stderr tail + `AgentExited`)
                // time to land, then stop the agent if its fan-out is still
                // draining — before `Error` lets workspace removal proceed
                // under it. Only this turn's conn is closed, never one a
                // later resume installed.
                let (slot, conn) = (slot.clone(), live.conn.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(FAILED_TURN_GRACE).await;
                    let running = slot
                        .fanout
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|fanout| !fanout.is_finished());
                    let mut current = slot.conn.lock().unwrap();
                    if running
                        && current
                            .as_ref()
                            .is_some_and(|c| Arc::ptr_eq(&c.conn, &conn))
                    {
                        current.take();
                        conn.close();
                    }
                });
                let _ = self.sink.transition(
                    session_id,
                    SessionState::Error(format!("prompt failed: {e:#}")),
                    true,
                );
            }
        }
        result
    }

    /// Execute a bounded, explicit Pi control surface on the existing adapter.
    pub async fn pi_command(
        &self,
        session_id: SessionId,
        command: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let name = command.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let readonly = matches!(
            name,
            "get_commands"
                | "get_state"
                | "get_available_models"
                | "get_available_thinking_levels"
                | "get_session_stats"
                | "get_messages"
        );
        ensure!(
            readonly
                || matches!(
                    name,
                    "set_model"
                        | "set_thinking_level"
                        | "set_auto_compaction"
                        | "set_auto_retry"
                        | "set_steering_mode"
                        | "set_follow_up_mode"
                        | "compact"
                        | "set_session_name"
                ),
            "unsupported Pi control command: {name}"
        );
        let session = self
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;
        ensure!(
            !matches!(
                session.state,
                SessionState::Done
                    | SessionState::Error(_)
                    | SessionState::Connecting
                    | SessionState::Created
            ),
            "agent is not ready; resume the task first"
        );
        let slot = self.slot(session_id)?;
        let _permit = if readonly {
            None
        } else {
            Some(
                slot.prompt_lock
                    .try_lock()
                    .map_err(|_| anyhow!("session busy; wait for the current turn"))?,
            )
        };
        let live = slot
            .conn
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session has no live connection"))?;
        match live.conn.as_ref() {
            SpawnedConn::Pi(conn) => conn.request(command).await,
            _ => bail!("selected agent does not use Pi RPC"),
        }
    }

    /// Build the outgoing prompt text: preamble + refs + user text.
    fn compose_prompt(&self, session: &Session, text: &str, refs: &[SessionRef]) -> Result<String> {
        let (worktree_path, session_count) = {
            let store = self.sink.store.lock().unwrap();
            let workspace = store
                .get_workspace(session.workspace_id)?
                .ok_or_else(|| anyhow!("workspace {} is gone", session.workspace_id))?;
            let count = store.list_sessions(session.workspace_id)?.len();
            (workspace.worktree_path, count)
        };

        let mut out = String::new();
        if let Some(preamble) = collab::shared_context_preamble(&worktree_path, session_count)? {
            out.push_str(&preamble);
            out.push_str("\n\n");
        }
        for r in refs {
            out.push_str(&self.render_ref(r)?);
            out.push_str("\n\n");
        }
        out.push_str(text);
        Ok(out)
    }

    /// Render one [`SessionRef`] as a context block: the referenced
    /// session's events summarized one per line.
    fn render_ref(&self, r: &SessionRef) -> Result<String> {
        let mut events = self.sink.store.lock().unwrap().read_events(r.session_id)?;
        // `event_seq == 0` reads as "everything so far".
        if r.event_seq > 0 {
            events.retain(|e| e.seq <= r.event_seq);
        }
        let mut out = format!(
            "Context from session {} ({} events):\n",
            r.session_id,
            events.len()
        );
        for ev in &events {
            if let Some(line) = describe_event(ev) {
                out.push_str("- ");
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push_str("--- end of referenced context ---");
        Ok(out)
    }

    /// Forward `session/cancel` to a session's live connection, then mark
    /// it `Ready` again (unless a terminal state won the race meanwhile).
    /// Deliberately does *not* take the prompt lock — cancelling a hung
    /// prompt is the point.
    pub async fn cancel(&self, session_id: SessionId) -> Result<()> {
        let slot = self.slot(session_id)?;
        let live = slot
            .conn
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session {session_id} has no live connection"))?;
        live.conn.cancel(&live.acp_session_id).await?;
        self.sink
            .transition(session_id, SessionState::Ready, true)?;
        Ok(())
    }

    /// Answer a parked permission request (`session/permission`). The
    /// `request_id` came from a `PermissionRequest` event; `decision`
    /// maps onto the options the agent offered (see
    /// [`AcpConn::respond_permission`] for the mapping and its errors).
    ///
    /// No state transition happens here: the conn emits
    /// `PermissionResolved` when the parked agent request unwinds, and
    /// the fan-out moves `WaitingPermission → Prompting` off that event —
    /// so a response racing a `kill`/`cancel` can't resurrect the
    /// session. Not async: the conn's resolve is a sync channel send.
    pub fn respond_permission(
        &self,
        session_id: SessionId,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<()> {
        let slot = self.slot(session_id)?;
        let live = slot
            .conn
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("session {session_id} has no live connection"))?;
        live.conn.respond_permission(request_id, decision)
    }

    /// Stop a slot's fan-out and close its conn — the shared teardown used
    /// by `kill`, `resume` and the post-`connect` kill-race cleanup.
    /// Idempotent; a slot may hold neither.
    fn teardown_slot(slot: &SessionSlot) {
        *slot.native_nonce.lock().unwrap() = None;
        *slot.native_reservation.lock().unwrap() = None;
        if let Some(handle) = slot.fanout.lock().unwrap().take() {
            handle.abort();
        }
        if let Some(live) = slot.conn.lock().unwrap().take() {
            live.conn.close();
        }
    }

    /// Terminate a session: mark it `Done`, then stop the fan-out and
    /// close the connection (the child dies via `kill_on_drop`).
    ///
    /// The `Done` transition runs first and teardown is unconditional:
    /// a failing store write must not strand a live child process with
    /// no fan-out draining it, and a conn spawned by a racing setup can
    /// never survive past the kill.
    ///
    /// A caller-initiated kill produces no `AgentExited` from the conn —
    /// the worker is torn down before its exit watcher can report — so we
    /// emit `StateChanged → Done` ourselves to give clients a terminal
    /// signal. Idempotent: killing an already-`Done` session is a no-op.
    pub async fn kill(&self, session_id: SessionId) -> Result<()> {
        // A session row is required even when no live slot exists.
        self.get_session(session_id)?
            .ok_or_else(|| anyhow!("no such session {session_id}"))?;

        let transition = self.sink.transition(session_id, SessionState::Done, false);
        if let Ok(slot) = self.slot(session_id) {
            Self::teardown_slot(&slot);
        }
        // Propagate the store error only after teardown ran.
        transition?;
        Ok(())
    }

    /// Reconnect a `Done`/`Error` session to its original native conversation.
    /// Restore failure preserves its identity and history; it never falls back
    /// to new_session unless initial setup never acquired a native identity.
    pub async fn resume(&self, session_id: SessionId) -> Result<()> {
        let (mut session, workspace) = {
            let store = self.sink.store.lock().unwrap();
            let session = store
                .get_session(session_id)?
                .ok_or_else(|| anyhow!("no such session {session_id}"))?;
            let workspace = store
                .get_workspace(session.workspace_id)?
                .ok_or_else(|| anyhow!("workspace {} is gone", session.workspace_id))?;
            (session, workspace)
        };
        match session.state {
            SessionState::Done | SessionState::Error(_) => {}
            ref other => bail!("cannot resume session {session_id} in state {other:?}"),
        }
        let profile = self
            .registry
            .lock()
            .unwrap()
            .get(&session.agent_id)
            .cloned()
            .ok_or_else(|| anyhow!("agent {} is no longer configured", session.agent_id))?;
        ensure!(
            profile.available,
            "agent {} is not available",
            session.agent_id
        );

        let _ownership = if matches!(
            profile.adapter,
            AdapterKind::PiRpc { .. }
                | AdapterKind::Native {
                    session_backend: Some(crate::NativeSessionBackend::Pi),
                    ..
                }
        ) {
            Some(self.native_ownership.lock().await)
        } else {
            None
        };
        let preflight = async {
        if matches!(profile.adapter, AdapterKind::PiRpc { .. }) && session.native_session_file.is_none() {
            if let Some(native_id) = session.acp_session_id.clone() {
                let p = profile.clone(); let cwd = workspace.worktree_path.clone();
                let candidates = tokio::task::spawn_blocking(move || crate::pi_sessions::list(&p, &cwd)).await??;
                let matching: Vec<_> = candidates.into_iter().filter(|s| s.session_id == native_id).collect();
                ensure!(matching.len() == 1, "cannot uniquely locate the original Pi session file; open native history to choose it explicitly");
                let file = matching[0].session_file.clone();
                self.sink.store.lock().unwrap().set_native_session(session_id, &native_id, Some(&file))?;
                session.native_session_file = Some(file);
            }
        }
        if let Some(file) = &session.native_session_file { self.check_native_owner(file, Some(session_id))?; }
        Ok::<(), anyhow::Error>(())
        }.await;
        if let Err(error) = preflight {
            // Only a still-terminal record may be changed; never disturb a winning resume.
            let _ = self.sink.transition_out_of_terminal(
                session_id,
                SessionState::Error(format!("resume preflight failed: {error:#}")),
            );
            return Err(error);
        }

        let slot = self.slot(session_id).unwrap_or_else(|_| {
            // Defensive: a session row always gets a slot at creation, but
            // after a daemon restart only the store remains — recreate the
            // slot so resume works on rehydrated sessions too.
            let slot = Arc::new(SessionSlot::new(
                workspace.worktree_path.clone(),
                profile.name.clone(),
            ));
            self.sessions
                .lock()
                .unwrap()
                .entry(session_id)
                .or_insert_with(|| slot.clone())
                .clone()
        });

        // A killed turn may still be unwinding. It must not mutate state after
        // a new controller has taken ownership of this native conversation.
        let _permit = slot
            .prompt_lock
            .try_lock()
            .map_err(|_| anyhow!("previous agent turn is still unwinding; retry resume shortly"))?;

        // Claim the resume *before* touching the slot: a concurrent losing
        // `resume` must not run `teardown_slot` — it could land between the
        // winner's `fanout`/`conn` installs inside `connect` and leave the
        // session `Ready` with a live conn but an aborted fan-out (silent
        // event loss).
        match self
            .sink
            .transition_out_of_terminal(session_id, SessionState::Connecting)
        {
            Ok(true) => {}
            Ok(false) => bail!("session {session_id} left a resumable state mid-resume"),
            Err(e) => {
                // The session row vanished under us (e.g. `workspace/remove`
                // deleted it) — drop the stale slot so no unreachable conn
                // lingers.
                if let Some(slot) = self.sessions.lock().unwrap().remove(&session_id) {
                    Self::teardown_slot(&slot);
                }
                return Err(e);
            }
        }

        // We own the resume now: tear down whatever the slot still holds
        // (an Error session's dead conn, a Done session's leftover handle).
        Self::teardown_slot(&slot);

        match self
            .connect(
                &profile,
                &workspace.worktree_path,
                session_id,
                &slot,
                Some(&session),
            )
            .await
        {
            Ok(()) => {
                // `unless_terminal`: a `kill` racing the `connect` await
                // owns the terminal state — tear the fresh conn down
                // rather than resurrect. A missing row (`workspace/remove`)
                // means the session is unreachable — teardown + drop the
                // slot so no orphan conn survives.
                match self.sink.transition(session_id, SessionState::Ready, true) {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        Self::teardown_slot(&slot);
                        Ok(())
                    }
                    Err(e) => {
                        Self::teardown_slot(&slot);
                        self.sessions.lock().unwrap().remove(&session_id);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                let _ = self.sink.transition(
                    session_id,
                    SessionState::Error(format!("resume failed: {e:#}")),
                    false,
                );
                Err(e)
            }
        }
    }
}

/// How long a failed turn's agent may take to exit on its own (its exit
/// watcher waits up to [`crate::stderr::EOF_WAIT`] for stderr) before the
/// orchestrator stops it.
const FAILED_TURN_GRACE: Duration = Duration::from_secs(2);

/// The per-session fan-out task: drain `conn.events()`, restamp each
/// event with the orchestrator's `session_id`, assign the real `seq`,
/// persist to the JSONL log, broadcast on the bus, and feed the
/// `.agentmux/activity.md` blackboard when the event summarizes.
///
/// `AgentExited` (or the channel closing) ends the task; an exit moves
/// the session to `Error` unless `kill` already put it in `Done`.
fn spawn_fanout(
    sink: EventSink,
    session_id: SessionId,
    mut rx: broadcast::Receiver<Event>,
    worktree_path: PathBuf,
    agent_name: String,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(mut ev) => {
                    // Adapters stamp their own internal session id and
                    // seq=0; the orchestrator owns both.
                    ev.session_id = session_id;
                    let exited = matches!(ev.kind, EventKind::AgentExited { .. });
                    sink.ingest(&mut ev);
                    // Permission bookkeeping rides the event stream so
                    // the WaitingPermission step lands strictly after the
                    // PermissionRequest it answers. `transition_from`
                    // (not the generic `transition`) keeps a resolved
                    // racing a cancel/kill from resurrecting the session.
                    match &ev.kind {
                        EventKind::PermissionRequest { .. } => {
                            let _ = sink.transition_from(
                                session_id,
                                SessionState::Prompting,
                                SessionState::WaitingPermission,
                            );
                        }
                        EventKind::PermissionResolved { .. } => {
                            let _ = sink.transition_from(
                                session_id,
                                SessionState::WaitingPermission,
                                SessionState::Prompting,
                            );
                        }
                        _ => {}
                    }
                    if let Some(summary) = collab::summarize_event(&ev.kind) {
                        let _ = collab::append_activity(&worktree_path, &agent_name, &summary);
                    }
                    if exited {
                        let _ = sink.transition(
                            session_id,
                            SessionState::Error("agent exited".into()),
                            true,
                        );
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // The conn's 256-deep event channel overflowed
                    // mid-burst — `n` adapter events are gone for good.
                    // Persist a marker through the sink (real seq, JSONL
                    // + bus like any other event) so the gap in the log
                    // is explainable instead of silent.
                    sink.emit(
                        session_id,
                        EventKind::Orchestrator(format!(
                            "event channel lagged; dropped {n} adapter event(s) \
                             (the burst exceeded the conn's buffer)"
                        )),
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Render one persisted event as a one-line summary for cross-session
/// context injection (`refs`). Returns `None` for noise not worth
/// sharing.
fn describe_event(ev: &Event) -> Option<String> {
    if let Some(summary) = collab::summarize_event(&ev.kind) {
        return Some(summary);
    }
    match &ev.kind {
        EventKind::ConversationStarted | EventKind::TitleChanged { .. } => None,
        EventKind::SessionUpdate(v) => {
            // Unwrap the `{"sessionId", "update"}` notification envelope.
            let update = v.get("update").unwrap_or(v);
            match update.get("sessionUpdate").and_then(|u| u.as_str()) {
                Some("agent_message_chunk") => update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                    .map(|t| format!("message: {t}")),
                Some(other) => Some(format!("session update: {other}")),
                // No `sessionUpdate` key: a pi-native record that
                // survived translation unnormalized (`agent_settled`,
                // lifecycle records, raw `tool_execution_*`, …) or a
                // persisted pre-translation pi event.
                None => crate::pi_shape::summary(update),
            }
        }
        EventKind::StateChanged { from, to } => Some(format!("state {from:?} → {to:?}")),
        EventKind::AgentExited { code } => Some(format!("agent exited (code {code:?})")),
        EventKind::Orchestrator(note) => Some(note.clone()),
        EventKind::PermissionRequest { request, .. } => {
            // Best-effort title dig — the serialized shape is
            // `{"toolCall": {"title": …, …}, "options": […]}`.
            let title = request
                .pointer("/toolCall/title")
                .and_then(|t| t.as_str())
                .unwrap_or("tool call");
            Some(format!("permission requested: {title}"))
        }
        EventKind::PermissionResolved { outcome, .. } => Some(format!("permission {outcome}")),
        // summarize_event handled FileEdited above.
        EventKind::FileEdited { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sink(data_dir: &Path) -> EventSink {
        let (bus, _) = broadcast::channel(BUS_CAPACITY);
        EventSink {
            store: Arc::new(Mutex::new(Store::open(data_dir).unwrap())),
            bus,
            seqs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn burst_event(session_id: SessionId, i: usize) -> Event {
        Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::Orchestrator(format!("burst {i}")),
        }
    }

    fn update_event(session_id: SessionId, value: serde_json::Value) -> Event {
        Event {
            session_id,
            seq: 1,
            ts: Utc::now(),
            kind: EventKind::SessionUpdate(value),
        }
    }

    /// pi passthrough records must produce non-empty relay lines — a
    /// `SessionRef` into a pi session is useless if every line drops.
    /// `pi_shape::summary` does the digging; this pins the contract at
    /// the relay-rendering seam.
    #[test]
    fn describe_event_summarizes_pi_records() {
        let sid = SessionId::new();

        // A text delta that arrived unnormalized (persisted log).
        let delta = update_event(
            sid,
            serde_json::json!({"type":"message_update","assistantMessageEvent":
                {"type":"text_delta","contentIndex":0,"delta":"half a reply"}}),
        );
        assert_eq!(
            describe_event(&delta).as_deref(),
            Some("message: half a reply")
        );

        // `message_end` carries the complete message — the best relay
        // line a pi turn offers.
        let end = update_event(
            sid,
            serde_json::json!({"type":"message_end","message":{"role":"assistant",
                "content":[{"type":"text","text":"full reply"}]}}),
        );
        assert_eq!(describe_event(&end).as_deref(), Some("message: full reply"));

        // Tool lifecycle passthrough and settle records summarize too.
        let tool = update_event(
            sid,
            serde_json::json!({"type":"tool_execution_start","toolCallId":"t",
                "toolName":"edit","args":{"path":"src/x.rs"}}),
        );
        assert!(
            describe_event(&tool).unwrap().contains("src/x.rs"),
            "tool start should name the path"
        );
        let settled = update_event(sid, serde_json::json!({"type": "agent_settled"}));
        assert_eq!(describe_event(&settled).as_deref(), Some("run settled"));

        // Pure scaffolding stays out of the context block.
        let noise = update_event(sid, serde_json::json!({"type": "turn_start"}));
        assert_eq!(describe_event(&noise), None);

        // Normalized pi output takes the ACP path unchanged.
        let chunk = update_event(
            sid,
            serde_json::json!({"sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"hi"},
                "pi":{"type":"message_update"}}),
        );
        assert_eq!(describe_event(&chunk).as_deref(), Some("message: hi"));
    }

    /// Regression: the conn-side event channel is bounded (256 deep); an
    /// adapter burst bigger than that makes the fan-out's `recv` return
    /// `Lagged`. The fan-out must mark the loss with a persisted,
    /// seq'd `EventKind::Orchestrator` note instead of silently
    /// continuing — otherwise the JSONL log shows an inexplicable jump.
    #[tokio::test]
    async fn fanout_lagged_persists_orchestrator_note() {
        let data = tempfile::tempdir().unwrap();
        let sink = test_sink(data.path());
        let session_id = SessionId::new();
        let worktree = data.path().to_path_buf();

        // Capacity-4 channel, 7 events queued before the fan-out starts:
        // the oldest 3 are already overwritten → `Lagged(3)`.
        let (tx, rx) = broadcast::channel(4);
        for i in 0..7 {
            tx.send(burst_event(session_id, i)).unwrap();
        }
        let handle = spawn_fanout(sink.clone(), session_id, rx, worktree, "mock-agent".into());
        // The exit ends the fan-out; the no-session-row transition error
        // is swallowed (`let _`) as designed.
        tx.send(Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::AgentExited { code: Some(0) },
        })
        .unwrap();
        handle.await.expect("fan-out should finish");

        let events = sink.store.lock().unwrap().read_events(session_id).unwrap();
        let note = events
            .iter()
            .find(|e| matches!(&e.kind, EventKind::Orchestrator(m) if m.contains("lagged")))
            .expect("lag must leave a persisted Orchestrator note");
        assert!(
            note.seq > 0,
            "the lag marker is seq'd like any other event: {note:?}"
        );
        // Retained burst events and the exit persisted too — the note
        // sits *before* the retained burst in seq order, marking where
        // the loss happened.
        let retained: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::Orchestrator(m) if m.starts_with("burst")))
            .collect();
        assert!(
            !retained.is_empty() && retained.iter().all(|e| e.seq > note.seq),
            "retained events must follow the lag note: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.kind, EventKind::AgentExited { .. })),
            "the trailing AgentExited must persist: {events:?}"
        );
    }
}
