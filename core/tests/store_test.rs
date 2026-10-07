//! Integration tests for `Store` (SQLite metadata + per-session JSONL logs).
//!
//! Each test opens a store rooted at a fresh `TempDir` — the same layout the
//! daemon uses under `~/.local/share/agentmux/`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use agentmux_core::id::{AgentId, ProjectId, SessionId, WorkspaceId};
use agentmux_core::model::{
    AdapterKind, AgentProfile, Event, EventKind, Project, Session, SessionRef, SessionState,
    Workspace,
};
use agentmux_core::store::Store;
use chrono::Utc;
use tempfile::TempDir;

/// A store in a throwaway dir; the TempDir is returned so it outlives the store.
fn temp_store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).expect("Store::open on a fresh dir should succeed");
    (dir, store)
}

fn sample_project() -> Project {
    Project {
        id: ProjectId::new(),
        root_path: PathBuf::from("/repos/agentmux"),
        name: "agentmux".into(),
    }
}

fn sample_workspace(project_id: ProjectId, name: &str) -> Workspace {
    Workspace {
        id: WorkspaceId::new(),
        project_id,
        name: name.into(),
        worktree_path: PathBuf::from(format!("/repos/agentmux/.agentmux/worktrees/{name}")),
        branch: format!("agentmux/{name}"),
        managed_worktree: true,
        created_at: Utc::now(),
    }
}

fn sample_agent() -> AgentProfile {
    AgentProfile {
        id: AgentId::new("claude-code"),
        name: "Claude Code".into(),
        adapter: AdapterKind::Acp {
            command: PathBuf::from("/usr/bin/claude-agent-acp"),
            args: vec!["--verbose".into()],
        },
        env: BTreeMap::from([("NO_COLOR".to_string(), "1".to_string())]),
        available: true,
    }
}

fn sample_session(workspace_id: WorkspaceId, agent_id: &AgentId) -> Session {
    Session {
        id: SessionId::new(),
        workspace_id,
        agent_id: agent_id.clone(),
        state: SessionState::Created,
        acp_session_id: None,
        native_session_file: None,
        native_terminal: false,
        references: vec![],
        created_at: Utc::now(),
    }
}

fn sample_event(session_id: SessionId, seq: u64) -> Event {
    Event {
        session_id,
        seq,
        ts: Utc::now(),
        kind: EventKind::Orchestrator(format!("event {seq}")),
    }
}

/// Insert a project + workspace + agent + session chain and return the session.
fn scaffold(store: &Store) -> (Project, Workspace, AgentProfile, Session) {
    let project = sample_project();
    let workspace = sample_workspace(project.id, "feature-x");
    let agent = sample_agent();
    let session = sample_session(workspace.id, &agent.id);

    store.insert_project(&project).unwrap();
    store.insert_workspace(&workspace).unwrap();
    store.upsert_agent(&agent).unwrap();
    store.insert_session(&session).unwrap();
    (project, workspace, agent, session)
}

#[test]
fn open_creates_db_and_sessions_dir() {
    let (dir, _store) = temp_store();
    assert!(dir.path().join("db.sqlite").is_file());
    assert!(dir.path().join("sessions").is_dir());
}

#[test]
fn native_session_identity_and_file_survive_reopen() {
    let (dir, store) = temp_store();
    let (_, _, _, session) = scaffold(&store);
    let file = dir.path().join("native-session.jsonl");
    store
        .set_native_session(session.id, "native-id", Some(&file))
        .unwrap();
    drop(store);
    let store = Store::open(dir.path()).unwrap();
    let saved = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(saved.acp_session_id.as_deref(), Some("native-id"));
    assert_eq!(saved.native_session_file, Some(file));
    assert!(store
        .set_native_session(SessionId::new(), "missing", None)
        .is_err());
}

#[test]
fn upgrading_legacy_schema_preserves_sessions_and_event_logs() {
    let (dir, store) = temp_store();
    let (_, _, _, session) = scaffold(&store);
    let event = sample_event(session.id, 1);
    store.append_event(&event).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(dir.path().join("db.sqlite")).unwrap();
    conn.execute("ALTER TABLE sessions DROP COLUMN native_session_file", [])
        .unwrap();
    drop(conn);
    for _ in 0..2 {
        let upgraded = Store::open(dir.path()).unwrap();
        assert_eq!(
            upgraded.get_session(session.id).unwrap(),
            Some(session.clone())
        );
        assert_eq!(
            upgraded.read_events(session.id).unwrap(),
            vec![event.clone()]
        );
    }
}

#[test]
fn project_insert_get_list_roundtrip() {
    let (_dir, store) = temp_store();
    let project = sample_project();

    store.insert_project(&project).unwrap();
    assert_eq!(
        store.get_project(project.id).unwrap(),
        Some(project.clone())
    );
    assert_eq!(store.list_projects().unwrap(), vec![project]);

    assert_eq!(store.get_project(ProjectId::new()).unwrap(), None);
}

#[test]
fn workspace_list_is_scoped_to_project() {
    let (_dir, store) = temp_store();
    let p1 = sample_project();
    let p2 = sample_project();
    store.insert_project(&p1).unwrap();
    store.insert_project(&p2).unwrap();

    let w1 = sample_workspace(p1.id, "w1");
    let w2 = sample_workspace(p1.id, "w2");
    let w_other = sample_workspace(p2.id, "other");
    for w in [&w1, &w2, &w_other] {
        store.insert_workspace(w).unwrap();
    }

    assert_eq!(store.get_workspace(w1.id).unwrap(), Some(w1.clone()));
    assert_eq!(
        store.list_workspaces(p1.id).unwrap(),
        vec![w1, w2],
        "list_workspaces must only return workspaces of the given project"
    );
    assert_eq!(store.list_workspaces(p2.id).unwrap(), vec![w_other]);
    assert_eq!(store.get_workspace(WorkspaceId::new()).unwrap(), None);
}

#[test]
fn agent_upsert_updates_in_place_and_lists() {
    let (_dir, store) = temp_store();
    let mut agent = sample_agent();
    store.upsert_agent(&agent).unwrap();

    // Re-probe: binary went missing, display name unchanged.
    agent.available = false;
    agent
        .env
        .insert("HTTPS_PROXY".into(), "http://localhost:8080".into());
    store.upsert_agent(&agent).unwrap();

    let agents = store.list_agents().unwrap();
    assert_eq!(
        agents,
        vec![agent.clone()],
        "upsert must replace, not duplicate"
    );

    assert_eq!(store.get_agent(&agent.id).unwrap(), agents.first().cloned());
    assert_eq!(store.get_agent(&AgentId::new("ghost")).unwrap(), None);
}

#[test]
fn session_state_update_is_reflected_in_list() {
    let (_dir, store) = temp_store();
    let (_p, w, _a, session) = scaffold(&store);

    store
        .update_session_state(session.id, &SessionState::Prompting)
        .unwrap();

    let sessions = store.list_sessions(w.id).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].state, SessionState::Prompting);

    // Error carries its message through the serde_json column.
    store
        .update_session_state(session.id, &SessionState::Error("agent died".into()))
        .unwrap();
    let sessions = store.list_sessions(w.id).unwrap();
    assert_eq!(sessions[0].state, SessionState::Error("agent died".into()));
}

#[test]
fn list_sessions_is_scoped_to_workspace() {
    let (_dir, store) = temp_store();
    let project = sample_project();
    store.insert_project(&project).unwrap();
    let w1 = sample_workspace(project.id, "w1");
    let w2 = sample_workspace(project.id, "w2");
    store.insert_workspace(&w1).unwrap();
    store.insert_workspace(&w2).unwrap();
    let agent = sample_agent();
    store.upsert_agent(&agent).unwrap();

    let s1 = sample_session(w1.id, &agent.id);
    let s2 = sample_session(w2.id, &agent.id);
    store.insert_session(&s1).unwrap();
    store.insert_session(&s2).unwrap();

    assert_eq!(store.list_sessions(w1.id).unwrap(), vec![s1]);
    assert_eq!(store.list_sessions(w2.id).unwrap(), vec![s2]);
}

#[test]
fn append_and_read_events_roundtrip_in_seq_order() {
    let (dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);

    let events: Vec<Event> = (0..3).map(|seq| sample_event(session.id, seq)).collect();
    for ev in &events {
        store.append_event(ev).unwrap();
    }

    assert_eq!(
        store.read_events(session.id).unwrap(),
        events,
        "events must come back in append order with identical content"
    );

    // The log lives at <data_dir>/sessions/<session_id>.jsonl, one JSON object
    // per line.
    let log = dir
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", session.id));
    let raw = std::fs::read_to_string(&log).expect("jsonl log should exist");
    assert_eq!(raw.lines().count(), 3);
    let first: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    assert_eq!(first["seq"], 0);
}

#[test]
fn read_events_sorts_by_seq_and_empty_log_is_empty_vec() {
    let (_dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);

    // No events yet -> empty vec, not an error.
    assert_eq!(store.read_events(session.id).unwrap(), Vec::<Event>::new());
    // Unknown session -> also empty (the file just does not exist).
    assert_eq!(
        store.read_events(SessionId::new()).unwrap(),
        Vec::<Event>::new()
    );

    // seq is assigned by the caller and may arrive out of order; the store
    // must return events sorted by seq.
    for seq in [2u64, 0, 1] {
        store.append_event(&sample_event(session.id, seq)).unwrap();
    }
    let seqs: Vec<u64> = store
        .read_events(session.id)
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[test]
fn every_event_kind_survives_the_jsonl_roundtrip() {
    let (_dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);

    let kinds = [
        EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"}
        })),
        EventKind::StateChanged {
            from: SessionState::Ready,
            to: SessionState::WaitingPermission,
        },
        EventKind::FileEdited {
            path: PathBuf::from("src/main.rs"),
        },
        EventKind::AgentExited { code: Some(1) },
        EventKind::Orchestrator("note".into()),
    ];
    for (seq, kind) in kinds.into_iter().enumerate() {
        store
            .append_event(&Event {
                session_id: session.id,
                seq: seq as u64,
                ts: Utc::now(),
                kind,
            })
            .unwrap();
    }
    assert_eq!(store.read_events(session.id).unwrap().len(), 5);
}

/// Path of `session_id`'s JSONL log inside `data_dir`.
fn event_log_path(data_dir: &Path, session_id: SessionId) -> PathBuf {
    data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"))
}

/// Append raw bytes to `session_id`'s log, bypassing `Store` — used to
/// simulate a crash that left a torn or corrupt line on disk.
fn append_raw(data_dir: &Path, session_id: SessionId, bytes: &[u8]) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(event_log_path(data_dir, session_id))
        .unwrap();
    f.write_all(bytes).unwrap();
}

#[test]
fn torn_tail_is_tolerated_and_next_append_heals_it() {
    let (dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);
    let e0 = sample_event(session.id, 0);
    let e1 = sample_event(session.id, 1);
    store.append_event(&e0).unwrap();
    store.append_event(&e1).unwrap();

    // Simulate a daemon crash mid-append: a partial JSON object with no
    // terminating newline.
    append_raw(dir.path(), session.id, b"{\"session_id\":\"deadbeef");

    // The torn tail is dropped; the complete events still replay.
    assert_eq!(
        store.read_events(session.id).unwrap(),
        vec![e0.clone(), e1.clone()],
        "torn final line must not poison the readable prefix"
    );

    // The next append truncates the torn tail first, so the new event does
    // not glue onto it and the log stays fully parseable.
    let e2 = sample_event(session.id, 2);
    store.append_event(&e2).unwrap();
    assert_eq!(store.read_events(session.id).unwrap(), vec![e0, e1, e2]);
    let raw = std::fs::read_to_string(event_log_path(dir.path(), session.id)).unwrap();
    assert_eq!(raw.lines().count(), 3);
    assert!(raw.ends_with('\n'));
}

#[test]
fn corrupt_complete_line_still_errors_loudly() {
    let (dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);
    store.append_event(&sample_event(session.id, 0)).unwrap();

    // A newline-terminated garbage line is *complete* corruption, not a
    // torn write — mid-file and even as the last content line it must
    // error rather than be silently dropped.
    append_raw(dir.path(), session.id, b"this is not json\n");

    assert!(
        store.read_events(session.id).is_err(),
        "a complete malformed line must error loudly"
    );

    // A file that is *only* a torn tail (no newline at all) yields no
    // events instead of erroring.
    let other = SessionId::new();
    append_raw(dir.path(), other, b"{\"partial");
    assert_eq!(store.read_events(other).unwrap(), Vec::<Event>::new());
}

#[test]
fn session_refs_and_acp_id_roundtrip() {
    let (_dir, store) = temp_store();
    let (_p, w, _a, mut session) = scaffold(&store);
    session.references = vec![SessionRef {
        session_id: SessionId::new(),
        event_seq: 7,
    }];
    session.acp_session_id = Some("acp-123".into());
    // Reinsert via a fresh insert on a new id to check full roundtrip.
    let session2 = Session {
        id: SessionId::new(),
        ..session.clone()
    };
    store.insert_session(&session2).unwrap();

    let got = &store.list_sessions(w.id).unwrap()[1];
    assert_eq!(got.acp_session_id.as_deref(), Some("acp-123"));
    assert_eq!(got.references.len(), 1);
    assert_eq!(got.references[0].event_seq, 7);
}

#[test]
fn get_session_and_set_acp_session_id() {
    let (_dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);

    assert_eq!(
        store.get_session(session.id).unwrap(),
        Some(session.clone())
    );
    assert_eq!(store.get_session(SessionId::new()).unwrap(), None);

    // Adapter reports its session id after initialize.
    store
        .set_acp_session_id(session.id, Some("acp-999"))
        .unwrap();
    let got = store.get_session(session.id).unwrap().unwrap();
    assert_eq!(got.acp_session_id.as_deref(), Some("acp-999"));

    // Updating a missing session is an error, not a silent no-op.
    assert!(store
        .set_acp_session_id(SessionId::new(), Some("x"))
        .is_err());
    assert!(store
        .update_session_state(SessionId::new(), &SessionState::Done)
        .is_err());
}

#[test]
fn reopening_store_preserves_metadata_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let (session, events) = {
        let store = Store::open(dir.path()).unwrap();
        let (_p, _w, _a, session) = scaffold(&store);
        let events: Vec<Event> = (0..2).map(|s| sample_event(session.id, s)).collect();
        for ev in &events {
            store.append_event(ev).unwrap();
        }
        store
            .update_session_state(session.id, &SessionState::Done)
            .unwrap();
        (session, events)
    };
    // store dropped; reopen the same data_dir.
    let store = Store::open(dir.path()).unwrap();
    let ws_id = session.workspace_id;
    let sessions = store.list_sessions(ws_id).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].state, SessionState::Done);
    assert_eq!(store.read_events(session.id).unwrap(), events);
}

/// The cached view is reused while the file is unchanged, follows the
/// store's own appends, and notices outside writes and deletion.
#[test]
fn cached_events_follow_appends_and_outside_changes() {
    let (dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);
    let e1 = sample_event(session.id, 1);
    store.append_event(&e1).unwrap();
    let first = store.events(session.id).unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&first, &store.events(session.id).unwrap()),
        "an unchanged log must not be re-parsed"
    );
    drop(first);

    let e2 = sample_event(session.id, 2);
    store.append_event(&e2).unwrap();
    assert_eq!(
        *store.events(session.id).unwrap(),
        vec![e1.clone(), e2.clone()]
    );

    // Another writer appends a complete event behind the store's back.
    let e3 = sample_event(session.id, 3);
    let mut line = serde_json::to_vec(&e3).unwrap();
    line.push(b'\n');
    append_raw(dir.path(), session.id, &line);
    assert_eq!(
        store.read_events(session.id).unwrap(),
        vec![e1.clone(), e2.clone(), e3.clone()]
    );
    // ...and the store's next append lands after it, still one per line.
    let e4 = sample_event(session.id, 4);
    store.append_event(&e4).unwrap();
    assert_eq!(store.read_events(session.id).unwrap(), vec![e1, e2, e3, e4]);

    store.delete_event_log(session.id).unwrap();
    assert!(store.events(session.id).unwrap().is_empty());
    let e5 = sample_event(session.id, 5);
    store.append_event(&e5).unwrap();
    assert_eq!(store.read_events(session.id).unwrap(), vec![e5]);
}

/// `last_seq` reads only the tail, ignores a torn final line, and falls
/// back to a full parse when one event outgrows the probe window.
#[test]
fn last_seq_reads_the_tail_and_handles_torn_and_huge_events() {
    let (dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);
    assert_eq!(store.last_seq(session.id).unwrap(), 0);
    for seq in 1..=3000 {
        store.append_event(&sample_event(session.id, seq)).unwrap();
    }
    append_raw(dir.path(), session.id, b"{\"session_id\":\"torn");
    assert_eq!(store.last_seq(session.id).unwrap(), 3000);

    let big = SessionId::new();
    store
        .append_event(&Event {
            session_id: big,
            seq: 7,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("x".repeat(600 * 1024)),
        })
        .unwrap();
    assert_eq!(store.last_seq(big).unwrap(), 7);
}

/// Manual benchmark: `cargo test -p agentmux-core --test store_test
/// bench_event_log -- --ignored --nocapture`. Streams many events into one
/// session, then times the reads a history pager and the boot sweep do.
#[test]
#[ignore = "manual event-log benchmark"]
fn bench_event_log() {
    let (_dir, store) = temp_store();
    let (_p, _w, _a, session) = scaffold(&store);
    let n = 20_000;
    let start = std::time::Instant::now();
    for seq in 1..=n {
        store.append_event(&sample_event(session.id, seq)).unwrap();
    }
    let append = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..20 {
        assert_eq!(store.read_events(session.id).unwrap().len(), n as usize);
    }
    let reads = start.elapsed() / 20;
    let start = std::time::Instant::now();
    for _ in 0..20 {
        assert_eq!(store.events(session.id).unwrap().len(), n as usize);
    }
    let shared = start.elapsed() / 20;
    // A pager polling history while the agent streams.
    let start = std::time::Instant::now();
    for seq in n + 1..=n + 500 {
        store.append_event(&sample_event(session.id, seq)).unwrap();
        store.events(session.id).unwrap();
    }
    let interleaved = start.elapsed() / 500;
    println!(
        "append {n}: {append:?} ({:?}/event); read_events: {reads:?}; \
         events: {shared:?}; append+events while streaming: {interleaved:?}",
        append / n as u32
    );
}
