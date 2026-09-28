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

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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

/// Metadata + event-log store rooted at a `data_dir`.
///
/// All methods take `&self`. `Store` is `Send` but not `Sync` (rusqlite
/// `Connection` isn't); an orchestrator sharing it across tasks wraps it in
/// a `Mutex`.
pub struct Store {
    conn: Connection,
    data_dir: PathBuf,
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

        let conn = Connection::open(data_dir.join(DB_FILE)).context("failed to open db.sqlite")?;
        conn.pragma_update(None, "foreign_keys", true)
            .context("failed to enable foreign keys")?;
        conn.execute_batch(SCHEMA)
            .context("failed to apply schema")?;

        Ok(Store {
            conn,
            data_dir: data_dir.to_path_buf(),
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
                 (id, project_id, name, worktree_path, branch, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    workspace.id.to_string(),
                    workspace.project_id.to_string(),
                    workspace.name,
                    path_str(&workspace.worktree_path)?,
                    workspace.branch,
                    workspace.created_at.to_rfc3339(),
                ],
            )
            .with_context(|| format!("failed to insert workspace {}", workspace.id))?;
        Ok(())
    }

    /// Fetch a workspace by id, or `None` if it does not exist.
    pub fn get_workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        self.conn
            .query_row(
                "SELECT id, project_id, name, worktree_path, branch, created_at
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
            "SELECT id, project_id, name, worktree_path, branch, created_at
             FROM workspaces WHERE project_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![project_id.to_string()], workspace_from_row)
            .context("failed to list workspaces")?;
        collect_rows(rows)
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
                 (id, workspace_id, agent_id, state, acp_session_id, refs, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session.id.to_string(),
                    session.workspace_id.to_string(),
                    session.agent_id.0,
                    serde_json::to_string(&session.state)?,
                    session.acp_session_id,
                    serde_json::to_string(&session.references)?,
                    session.created_at.to_rfc3339(),
                ],
            )
            .with_context(|| format!("failed to insert session {}", session.id))?;
        Ok(())
    }

    /// Fetch a session by id, or `None` if it does not exist.
    pub fn get_session(&self, id: SessionId) -> Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT id, workspace_id, agent_id, state, acp_session_id, refs, created_at
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

    /// All sessions in `workspace_id`, in insertion order.
    pub fn list_sessions(&self, workspace_id: WorkspaceId) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, workspace_id, agent_id, state, acp_session_id, refs, created_at
             FROM sessions WHERE workspace_id = ?1 ORDER BY rowid",
        )?;
        let rows = stmt
            .query_map(params![workspace_id.to_string()], session_from_row)
            .context("failed to list sessions")?;
        collect_rows(rows)
    }

    // ----- event log ------------------------------------------------------

    /// Append `ev` to `<data_dir>/sessions/<session_id>.jsonl`.
    ///
    /// The event's `seq` is taken as-is — sequencing is the orchestrator's
    /// job. The file is opened per call, so appends interleaved across many
    /// sessions stay cheap and there is no per-session writer state to
    /// flush on crash.
    pub fn append_event(&self, ev: &Event) -> Result<()> {
        let path = self.event_log_path(ev.session_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open event log {}", path.display()))?;
        let line = serde_json::to_string(ev).context("failed to serialize event")?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .with_context(|| format!("failed to append to {}", path.display()))?;
        Ok(())
    }

    /// Read all persisted events of `session_id`, sorted by `seq`.
    ///
    /// A session with no log file yields an empty vec. Blank lines (e.g. a
    /// truncated final write) are skipped; a malformed non-blank line is a
    /// hard error rather than silently dropping history.
    pub fn read_events(&self, session_id: SessionId) -> Result<Vec<Event>> {
        let path = self.event_log_path(session_id);
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to open event log {}", path.display()))
            }
        };

        let mut events = Vec::new();
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line
                .with_context(|| format!("failed to read {} line {}", path.display(), idx + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            events.push(serde_json::from_str::<Event>(&line).with_context(|| {
                format!("malformed event in {} line {}", path.display(), idx + 1)
            })?);
        }
        events.sort_by_key(|e| e.seq);
        Ok(events)
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
    refs           TEXT NOT NULL,  -- serde_json of Vec<SessionRef>
    created_at     TEXT NOT NULL
);
";

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
        references: from_json::<Vec<SessionRef>>(&row.get::<_, String>(5)?)?,
        created_at: parse_ts(&row.get::<_, String>(6)?)?,
    })
}
