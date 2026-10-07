//! Persistent storage for agentmux: SQLite metadata + per-session JSONL logs.
//!
//! [`Store`] owns a [`rusqlite::Connection`] to `<data_dir>/db.sqlite` holding
//! the relational metadata (projects, workspaces, agent profiles, sessions)
//! and appends each session's [`Event`] stream to
//! `<data_dir>/sessions/<session_id>.jsonl` — one JSON object per line, so a
//! crashed daemon can replay a session's UI state from disk.
//!
//! Column conventions: strongly-typed ids are stored as their `Display`
//! strings, timestamps as RFC 3339 text, and enums or structured values
//! ([`SessionState`], [`AdapterKind`], env maps, [`SessionRef`] lists) as
//! `serde_json` strings. `Event.seq` is assigned by the caller — the store
//! only persists it and guarantees `read_events` returns rows in seq order.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::id::{AgentId, ProjectId, SessionId, WorkspaceId};
use crate::model::{
    AdapterKind, AgentProfile, Event, Project, Session, SessionRef, SessionState, Workspace,
};
use crate::Result;

/// File name (inside `data_dir`) of the SQLite metadata database.
const DB_FILE: &str = "db.sqlite";

/// Directory (inside `data_dir`) holding one `<session_id>.jsonl` event log
/// per session.
const EVENTS_DIR: &str = "sessions";

/// How many sessions keep an open append handle and parsed events.
const CACHED_LOGS: usize = 32;

/// Upper bound on log bytes kept parsed in memory across sessions; the
/// least recently used parsed views go first (their append handles stay).
const PARSED_BUDGET: u64 = 64 * 1024 * 1024;

/// Bytes read from the end of a log to find its last `seq` cheaply.
const TAIL_PROBE: u64 = 256 * 1024;

/// Metadata + event-log store rooted at a `data_dir`.
///
/// All methods take `&self`. `Store` is `Send` but not `Sync` (rusqlite
/// `Connection` isn't); an orchestrator sharing it across tasks wraps it in
/// a `Mutex`.
pub struct Store {
    conn: Connection,
    data_dir: PathBuf,
    logs: RefCell<LogCache>,
}

/// Recently used event logs: an open append handle and, once read, the
/// parsed events — kept in step with appends so history pages and event
/// lookups do not re-read and re-parse the whole file. Every use checks the
/// file length against what the entry accounts for; any outside change
/// (another writer, a torn tail, deletion) drops the stale view.
#[derive(Default)]
struct LogCache {
    entries: HashMap<SessionId, LogEntry>,
    /// Least recently used first.
    order: VecDeque<SessionId>,
}

#[derive(Default)]
struct LogEntry {
    writer: Option<File>,
    /// File length this entry accounts for: complete lines only.
    len: Option<u64>,
    /// Parsed events in `seq` order, valid for exactly `len` bytes.
    events: Option<Arc<Vec<Event>>>,
}

impl LogCache {
    /// The entry for `id`, created if missing and marked most recent;
    /// the least recently used entry is evicted past [`CACHED_LOGS`].
    fn entry(&mut self, id: SessionId) -> &mut LogEntry {
        if let Some(pos) = self.order.iter().position(|s| *s == id) {
            self.order.remove(pos);
        } else if self.order.len() >= CACHED_LOGS {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.order.push_back(id);
        self.entries.entry(id).or_default()
    }

    /// Drop parsed views, least recently used first (never `keep`), until
    /// the parsed logs fit in [`PARSED_BUDGET`].
    fn trim(&mut self, keep: SessionId) {
        let mut total: u64 = self
            .entries
            .values()
            .filter(|e| e.events.is_some())
            .filter_map(|e| e.len)
            .sum();
        for id in &self.order {
            if total <= PARSED_BUDGET {
                break;
            }
            if *id == keep {
                continue;
            }
            if let Some(entry) = self.entries.get_mut(id) {
                if entry.events.take().is_some() {
                    total -= entry.len.unwrap_or(0);
                }
            }
        }
    }

    fn forget(&mut self, id: SessionId) {
        self.entries.remove(&id);
        self.order.retain(|s| *s != id);
    }
}

impl Store {
    /// Open (creating if necessary) the store at `data_dir`.
    ///
    /// Creates `data_dir` and its `sessions/` subdirectory, opens
    /// `db.sqlite`, enables foreign-key enforcement and applies the schema
    /// with `CREATE TABLE IF NOT EXISTS`, so reopening an existing store is
    /// a no-op.
    pub fn open(data_dir: &Path) -> Result<Store> {
        std::fs::create_dir_all(data_dir.join(EVENTS_DIR))
            .context("failed to create data dir / sessions dir")?;

        let mut conn =
            Connection::open(data_dir.join(DB_FILE)).context("failed to open db.sqlite")?;
        conn.pragma_update(None, "foreign_keys", true)
            .context("failed to enable foreign keys")?;
        conn.execute_batch(SCHEMA)
            .context("failed to apply schema")?;

        let migration = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let has_native_file: bool = migration.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'native_session_file')",
            [],
            |row| row.get(0),
        )?;
        if !has_native_file {
            migration.execute(
                "ALTER TABLE sessions ADD COLUMN native_session_file TEXT",
                [],
            )?;
        }
        let has_native_terminal: bool = migration.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'native_terminal')", [], |row| row.get(0),
        )?;
        if !has_native_terminal {
            migration.execute(
                "ALTER TABLE sessions ADD COLUMN native_terminal INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        let has_worktree_flag: bool = migration.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('workspaces') WHERE name = 'managed_worktree')", [], |row| row.get(0))?;
        if !has_worktree_flag {
            migration.execute(
                "ALTER TABLE workspaces ADD COLUMN managed_worktree INTEGER NOT NULL DEFAULT 1",
                [],
            )?;
        }
        migration
            .commit()
            .context("failed to migrate native session metadata")?;

        Ok(Store {
            conn,
            data_dir: data_dir.to_path_buf(),
            logs: RefCell::default(),
        })
    }

    // ----- projects -------------------------------------------------------

    /// Insert a new project; fails if `project.id` already exists.
    pub fn insert_project(&self, project: &Project) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO projects (id, root_path, name) VALUES (?1, ?2, ?3)",
                params![
                    project.id.to_string(),
                    path_str(&project.root_path)?,
                    project.name,
                ],
            )
            .with_context(|| format!("failed to insert project {}", project.id))?;
        Ok(())
    }

    /// Fetch a project by id, or `None` if it does not exist.
    pub fn get_project(&self, id: ProjectId) -> Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT id, root_path, name FROM projects WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok(Project {
                        id: ProjectId(parse_uuid(&row.get::<_, String>(0)?)?),
                        root_path: PathBuf::from(row.get::<_, String>(1)?),
                        name: row.get(2)?,
                    })
                },
            )
            .optional()
            .context("failed to read project")
    }

    /// Delete a project row. Returns `false` when no project with that id
    /// existed. Fails with a foreign-key error while workspaces still
    /// reference it — callers should remove workspaces first (the
    /// orchestrator checks this and produces a friendlier message).
    pub fn delete_project(&self, id: ProjectId) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM projects WHERE id = ?1",
                params![id.to_string()],
            )
            .with_context(|| format!("failed to delete project {id}"))?;
        Ok(n == 1)
    }

    /// All projects, in insertion order.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, root_path, name FROM projects ORDER BY rowid")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Project {
                    id: ProjectId(parse_uuid(&row.get::<_, String>(0)?)?),
                    root_path: PathBuf::from(row.get::<_, String>(1)?),
                    name: row.get(2)?,
                })
            })
            .context("failed to list projects")?;
        collect_rows(rows)
    }

    // ----- workspaces -----------------------------------------------------

    /// Insert a new workspace; fails if `workspace.id` already exists or
    /// `workspace.project_id` references a missing project.
    pub fn insert_workspace(&self, workspace: &Workspace) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO workspaces
                 (id, project_id, name, worktree_path, branch, created_at, managed_worktree)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    workspace.id.to_string(),
                    workspace.project_id.to_string(),
                    workspace.name,
                    path_str(&workspace.worktree_path)?,
                    workspace.branch,
                    workspace.created_at.to_rfc3339(),
                    workspace.managed_worktree,
                ],
            )
            .with_context(|| format!("failed to insert workspace {}", workspace.id))?;
        Ok(())
    }

    /// Fetch a workspace by id, or `None` if it does not exist.
    pub fn get_workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, worktree_path, branch, created_at, managed_worktree
                 FROM workspaces WHERE id = ?1",
                params![id.to_string()],
                workspace_from_row,
            )
            .optional()
            .context("failed to read workspace")
    }

    /// All workspaces belonging to `project_id`, in insertion order.
    pub fn list_workspaces(&self, project_id: ProjectId) -> Result<Vec<Workspace>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, worktree_path, branch, created_at, managed_worktree
             FROM workspaces WHERE project_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![project_id.to_string()], workspace_from_row)
            .context("failed to list workspaces")?;
        collect_rows(rows)
    }

    /// Delete a workspace row. Returns `false` when no workspace with that
    /// id existed. Fails with a foreign-key error while sessions still
    /// reference it — the orchestrator deletes session rows first.
    pub fn delete_workspace(&self, id: WorkspaceId) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM workspaces WHERE id = ?1",
                params![id.to_string()],
            )
            .with_context(|| format!("failed to delete workspace {id}"))?;
        Ok(n == 1)
    }

    /// Delete every session row of `workspace_id`, returning their ids so
    /// the caller can drop the per-session JSONL logs.
    pub fn delete_workspace_sessions(&self, workspace_id: WorkspaceId) -> Result<Vec<SessionId>> {
        let ids = self
            .list_sessions(workspace_id)?
            .into_iter()
            .map(|s| s.id)
            .collect::<Vec<_>>();
        self.conn
            .execute(
                "DELETE FROM sessions WHERE workspace_id = ?1",
                params![workspace_id.to_string()],
            )
            .with_context(|| format!("failed to delete sessions of workspace {workspace_id}"))?;
        Ok(ids)
    }

    // ----- agent profiles -------------------------------------------------

    /// Insert or replace an agent profile (keyed by `agent.id`).
    ///
    /// Profiles are config-driven: `upsert` lets a config reload overwrite a
    /// previous probe/name/env without erroring on the existing row.
    pub fn upsert_agent(&self, agent: &AgentProfile) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO agents (id, name, adapter, env, available)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   adapter = excluded.adapter,
                   env = excluded.env,
                   available = excluded.available",
                params![
                    agent.id.0,
                    agent.name,
                    serde_json::to_string(&agent.adapter)?,
                    serde_json::to_string(&agent.env)?,
                    agent.available,
                ],
            )
            .with_context(|| format!("failed to upsert agent {}", agent.id))?;
        Ok(())
    }

    /// Fetch an agent profile by id, or `None` if it does not exist.
    pub fn get_agent(&self, id: &AgentId) -> Result<Option<AgentProfile>> {
        self.conn
            .query_row(
                "SELECT id, name, adapter, env, available FROM agents WHERE id = ?1",
                params![id.0],
                agent_from_row,
            )
            .optional()
            .context("failed to read agent")
    }

    /// All agent profiles, ordered by id for deterministic listings.
    pub fn list_agents(&self) -> Result<Vec<AgentProfile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, adapter, env, available FROM agents ORDER BY id")?;
        let rows = stmt
            .query_map([], agent_from_row)
            .context("failed to list agents")?;
        collect_rows(rows)
    }

    // ----- sessions -------------------------------------------------------

    /// Insert a new session; fails if `session.id` already exists or
    /// `session.workspace_id` references a missing workspace.
    ///
    /// `session.agent_id` is deliberately *not* a foreign key: profiles come
    /// from config and may be renamed or deleted while historical sessions
    /// must remain readable.
    pub fn insert_session(&self, session: &Session) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO sessions
                 (id, workspace_id, agent_id, state, acp_session_id, refs, created_at, native_session_file, native_terminal)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session.id.to_string(),
                    session.workspace_id.to_string(),
                    session.agent_id.0,
                    serde_json::to_string(&session.state)?,
                    session.acp_session_id,
                    serde_json::to_string(&session.references)?,
                    session.created_at.to_rfc3339(),
                    session.native_session_file.as_deref().map(path_str).transpose()?,
                    session.native_terminal,
                ],
            )
            .with_context(|| format!("failed to insert session {}", session.id))?;
        Ok(())
    }

    /// Fetch a session by id, or `None` if it does not exist.
    pub fn get_session(&self, id: SessionId) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, workspace_id, agent_id, state, acp_session_id, refs, created_at, native_session_file, native_terminal
                 FROM sessions WHERE id = ?1",
                params![id.to_string()],
                session_from_row,
            )
            .optional()
            .context("failed to read session")
    }

    /// Set `sessions.state` for `session_id`.
    ///
    /// Fails if no row was updated — a state transition for a missing
    /// session is a caller bug, not a silent no-op.
    pub fn update_session_state(&self, session_id: SessionId, state: &SessionState) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE sessions SET state = ?2 WHERE id = ?1",
                params![session_id.to_string(), serde_json::to_string(state)?],
            )
            .with_context(|| format!("failed to update state of session {session_id}"))?;
        anyhow::ensure!(n == 1, "no session {session_id} to update");
        Ok(())
    }

    /// Set `sessions.acp_session_id` once the adapter has created (or loaded)
    /// the adapter-level session. `None` clears it.
    pub fn set_acp_session_id(
        &self,
        session_id: SessionId,
        acp_session_id: Option<&str>,
    ) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE sessions SET acp_session_id = ?2 WHERE id = ?1",
                params![session_id.to_string(), acp_session_id],
            )
            .with_context(|| format!("failed to set acp_session_id of session {session_id}"))?;
        anyhow::ensure!(n == 1, "no session {session_id} to update");
        Ok(())
    }

    /// Commit the native identity and file together after successful setup.
    pub fn set_native_session(
        &self,
        session_id: SessionId,
        native_id: &str,
        native_file: Option<&Path>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE sessions SET acp_session_id = ?2, native_session_file = ?3 WHERE id = ?1",
            params![
                session_id.to_string(),
                native_id,
                native_file.map(path_str).transpose()?
            ],
        )?;
        anyhow::ensure!(n == 1, "no session {session_id} to update");
        Ok(())
    }

    /// All sessions in `workspace_id`, in insertion order.
    pub fn list_sessions(&self, workspace_id: WorkspaceId) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workspace_id, agent_id, state, acp_session_id, refs, created_at, native_session_file, native_terminal
             FROM sessions WHERE workspace_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![workspace_id.to_string()], session_from_row)
            .context("failed to list sessions")?;
        collect_rows(rows)
    }

    /// Every session across all workspaces, in insertion order — the
    /// orchestrator's boot sweep needs the whole table, not one
    /// workspace's slice of it.
    pub fn list_all_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workspace_id, agent_id, state, acp_session_id, refs, created_at, native_session_file, native_terminal
             FROM sessions ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map([], session_from_row)
            .context("failed to list sessions")?;
        collect_rows(rows)
    }

    // ----- event log ------------------------------------------------------

    /// Append `ev` to `<data_dir>/sessions/<session_id>.jsonl`.
    ///
    /// The event's `seq` is taken as-is — sequencing is the orchestrator's
    /// job. Recently used logs keep their append handle open, so a
    /// streaming agent costs one `fstat` + one `write` per event.
    ///
    /// Crash safety: if the file is not exactly as long as this store left
    /// it — a torn write from a crash, or an outside writer — the partial
    /// tail is truncated *before* appending so it can never glue onto this
    /// event's line. The event itself is written as a single `write_all` of
    /// `json + '\n'`, keeping the torn window as small as a single syscall
    /// can make it.
    pub fn append_event(&self, ev: &Event) -> Result<()> {
        let path = self.event_log_path(ev.session_id);
        let mut line = serde_json::to_string(ev).context("failed to serialize event")?;
        line.push('\n');
        let mut logs = self.logs.borrow_mut();
        let entry = logs.entry(ev.session_id);
        let result = (|| -> Result<()> {
            if entry.writer.is_none() {
                entry.writer = Some(
                    OpenOptions::new()
                        .create(true)
                        .read(true)
                        .append(true)
                        .open(&path)
                        .with_context(|| format!("failed to open event log {}", path.display()))?,
                );
            }
            let file = entry.writer.as_mut().unwrap();
            if entry.len != Some(file.metadata()?.len()) {
                drop_torn_tail(file, &path)?;
                entry.len = Some(file.metadata()?.len());
                entry.events = None;
            }
            file.write_all(line.as_bytes())
                .with_context(|| format!("failed to append to {}", path.display()))?;
            entry.len = entry.len.map(|len| len + line.len() as u64);
            if let Some(events) = &mut entry.events {
                let events = Arc::make_mut(events);
                // Appends arrive in seq order; tolerate a racing writer's
                // small reordering without re-sorting.
                let at = events.partition_point(|e| e.seq <= ev.seq);
                events.insert(at, ev.clone());
            }
            Ok(())
        })();
        if result.is_err() {
            logs.forget(ev.session_id);
        }
        result
    }

    /// Read all persisted events of `session_id`, sorted by `seq`.
    ///
    /// A session with no log file yields an empty vec. Blank lines are
    /// skipped. A non-blank **final** line that fails to parse is a torn
    /// tail — a crash mid-append — and is dropped so a dead daemon can
    /// still replay the session (this is the recovery path the log exists
    /// for). A malformed *complete* line (anything before a `'\n'`) is a
    /// hard error rather than silently dropping history.
    pub fn read_events(&self, session_id: SessionId) -> Result<Vec<Event>> {
        Ok(self.events(session_id)?.as_ref().clone())
    }

    /// Shared, cached view of [`read_events`](Self::read_events): parsed
    /// once, then kept current by appends. Revalidated against the file
    /// length on every call, so outside changes are always observed.
    pub fn events(&self, session_id: SessionId) -> Result<Arc<Vec<Event>>> {
        let path = self.event_log_path(session_id);
        let len = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.logs.borrow_mut().forget(session_id);
                return Ok(Arc::new(Vec::new()));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read event log {}", path.display()))
            }
        };
        let mut logs = self.logs.borrow_mut();
        let entry = logs.entry(session_id);
        if let (Some(events), Some(cached)) = (&entry.events, entry.len) {
            if cached == len {
                return Ok(events.clone());
            }
        }
        let raw = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                logs.forget(session_id);
                return Ok(Arc::new(Vec::new()));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read event log {}", path.display()))
            }
        };
        let events = Arc::new(parse_log(&raw, &path)?);
        let complete = raw.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let entry = logs.entry(session_id);
        entry.len = Some(complete as u64);
        entry.events = Some(events.clone());
        logs.trim(session_id);
        Ok(events)
    }

    /// Highest persisted `seq` of `session_id` (0 for an empty or missing
    /// log), read from the file's tail so a boot sweep over many large
    /// logs stays cheap. Falls back to a full parse when one event is
    /// larger than the probe window.
    pub fn last_seq(&self, session_id: SessionId) -> Result<u64> {
        let path = self.event_log_path(session_id);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to read event log {}", path.display()))
            }
        };
        let len = file.metadata()?.len();
        let start = len.saturating_sub(TAIL_PROBE);
        file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        let mut lines = tail.split(|&b| b == b'\n');
        if start > 0 {
            lines.next(); // Partial first line.
        }
        // Appends are near seq order: the max lies within the last lines.
        let seqs: Vec<u64> = lines
            .filter_map(|bytes| serde_json::from_slice::<Event>(bytes).ok())
            .map(|e| e.seq)
            .collect();
        match seqs.into_iter().max() {
            Some(max) => Ok(max),
            None if start > 0 => {
                let mut raw = Vec::new();
                file.seek(SeekFrom::Start(0))?;
                file.read_to_end(&mut raw)?;
                Ok(parse_log(&raw, &path)?
                    .iter()
                    .map(|e| e.seq)
                    .max()
                    .unwrap_or(0))
            }
            None => Ok(0),
        }
    }

    /// Remove `session_id`'s JSONL event log, if present. A missing log is
    /// not an error (a session may have produced no events).
    pub fn delete_event_log(&self, session_id: SessionId) -> Result<()> {
        self.logs.borrow_mut().forget(session_id);
        let path = self.event_log_path(session_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
        }
    }

    /// Path of `session_id`'s captured agent-stderr log —
    /// `<data_dir>/sessions/<id>.stderr.log`, beside the JSONL event log.
    /// Created lazily by the conn's stderr drain (a quiet agent leaves no
    /// file); the orchestrator passes it in via
    /// [`crate::config::SpawnOptions::stderr_log`].
    pub fn stderr_log_path(&self, session_id: SessionId) -> PathBuf {
        self.data_dir
            .join(EVENTS_DIR)
            .join(format!("{session_id}.stderr.log"))
    }

    /// Remove `session_id`'s stderr log, if present. A missing log is not
    /// an error (a quiet agent never creates one).
    pub fn delete_stderr_log(&self, session_id: SessionId) -> Result<()> {
        let path = self.stderr_log_path(session_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
        }
    }

    /// Path of `session_id`'s JSONL log.
    fn event_log_path(&self, session_id: SessionId) -> PathBuf {
        self.data_dir
            .join(EVENTS_DIR)
            .join(format!("{session_id}.jsonl"))
    }
}

/// Schema applied by [`Store::open`]. `CREATE TABLE IF NOT EXISTS` keeps
/// reopening an existing store idempotent.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id        TEXT PRIMARY KEY,
    root_path TEXT NOT NULL,
    name      TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS workspaces (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL REFERENCES projects(id),
    name         TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    branch       TEXT NOT NULL,
    managed_worktree INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS agents (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL,
    adapter   TEXT NOT NULL,  -- serde_json of AdapterKind
    env       TEXT NOT NULL,  -- serde_json of BTreeMap<String, String>
    available INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id             TEXT PRIMARY KEY,
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id),
    agent_id       TEXT NOT NULL,
    state          TEXT NOT NULL,  -- serde_json of SessionState
    acp_session_id TEXT,
    native_session_file TEXT,
    native_terminal INTEGER NOT NULL DEFAULT 0,
    refs           TEXT NOT NULL,  -- serde_json of Vec<SessionRef>
    created_at     TEXT NOT NULL
);
";

/// Parse a JSONL log, sorted by `seq`; see [`Store::read_events`] for the
/// torn-tail and corruption rules.
fn parse_log(raw: &[u8], path: &Path) -> Result<Vec<Event>> {
    // `split` yields a trailing empty slice when the file ends with
    // `'\n'`, so a non-blank element at `last` can only exist when the
    // final line is unterminated — i.e. torn.
    let lines: Vec<&[u8]> = raw.split(|&b| b == b'\n').collect();
    let last = lines.len() - 1;

    let mut events = Vec::new();
    for (idx, bytes) in lines.iter().enumerate() {
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice::<Event>(bytes) {
            Ok(ev) => events.push(ev),
            // Torn tail: partial write left by a crash; drop it.
            Err(_) if idx == last => break,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("malformed event in {} line {}", path.display(), idx + 1)
                })
            }
        }
    }
    events.sort_by_key(|e| e.seq);
    Ok(events)
}

/// If `file` does not end with `'\n'`, a previous append was torn by a
/// crash — truncate back to the last complete line so the next append
/// starts on a line boundary. The check itself is a 1-byte read at EOF;
/// the full scan for the last `'\n'` only runs on the rare torn path.
fn drop_torn_tail(file: &mut File, path: &Path) -> Result<()> {
    if file.metadata()?.len() == 0 {
        return Ok(());
    }
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::End(-1))?;
    file.read_exact(&mut byte)?;
    if byte[0] == b'\n' {
        return Ok(());
    }

    file.seek(SeekFrom::Start(0))?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    // No newline at all means the whole file is one torn line.
    let keep = raw.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    file.set_len(keep as u64)
        .with_context(|| format!("failed to truncate torn tail of {}", path.display()))?;
    Ok(())
}

/// View a `Path` as `&str` for storage. Errors rather than silently
/// corrupting a non-UTF-8 path via `to_string_lossy`.
fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

/// Collect a `MappedRows` iterator, attaching context to row errors.
fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    rows.collect::<std::result::Result<Vec<T>, _>>()
        .context("failed to decode row")
}

/// Parse a `TEXT` column back into a [`Uuid`], mapping parse failures into
/// rusqlite errors so row decoders stay `?`-friendly.
fn parse_uuid(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Parse an RFC 3339 `TEXT` column back into `DateTime<Utc>`.
fn parse_ts(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

/// Decode a `serde_json` `TEXT` column back into `T`.
fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: WorkspaceId(parse_uuid(&row.get::<_, String>(0)?)?),
        project_id: ProjectId(parse_uuid(&row.get::<_, String>(1)?)?),
        name: row.get(2)?,
        worktree_path: PathBuf::from(row.get::<_, String>(3)?),
        branch: row.get(4)?,
        managed_worktree: row.get(6)?,
        created_at: parse_ts(&row.get::<_, String>(5)?)?,
    })
}

fn agent_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentProfile> {
    Ok(AgentProfile {
        id: AgentId(row.get::<_, String>(0)?),
        name: row.get(1)?,
        adapter: from_json::<AdapterKind>(&row.get::<_, String>(2)?)?,
        env: from_json(&row.get::<_, String>(3)?)?,
        available: row.get(4)?,
    })
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: SessionId(parse_uuid(&row.get::<_, String>(0)?)?),
        workspace_id: WorkspaceId(parse_uuid(&row.get::<_, String>(1)?)?),
        agent_id: AgentId(row.get::<_, String>(2)?),
        state: from_json::<SessionState>(&row.get::<_, String>(3)?)?,
        acp_session_id: row.get(4)?,
        native_session_file: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
        native_terminal: row.get(8)?,
        references: from_json::<Vec<SessionRef>>(&row.get::<_, String>(5)?)?,
        created_at: parse_ts(&row.get::<_, String>(6)?)?,
    })
}
