//! Integration tests for [`Orchestrator`], the session manager, against
//! the real `agentmux-mock-agent` binary and real git worktrees.
//!
//! Covers the six scenarios from the Task 9 brief:
//!
//! ① `create_workspace` → `create_session` → `prompt("hi")`: subscribers
//!   see the `SessionUpdate` stream and the session ends back at `Ready`.
//! ② `prompt` while another prompt is in flight → `session busy` error.
//! ③ prompt text `"crash"` → `AgentExited`, session state `Error`, and a
//!   non-empty JSONL event log on disk.
//! ④ two sessions in one workspace: after `append_activity`, session B's
//!   prompt carries the shared-context preamble (observed through the
//!   mock's echo of the prompt text).
//! ⑤ unknown (and unavailable) `agent_id` → `Err`.
//! ⑥ `resume` on a `Done` session → fresh connection, state back to
//!   `Ready`, and the session accepts new prompts.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux_core::collab::append_activity;
use agentmux_core::orchestrator::Orchestrator;
use agentmux_core::{
    AdapterKind, AgentId, AgentProfile, AgentRegistry, Config, Event, EventKind, Project,
    ProjectId, SessionId, SessionState, Store, WorkspaceId,
};
use tempfile::TempDir;
use tokio::sync::broadcast;

/// Generous deadline for event collection; the mock replies instantly, so
/// this only bites on regression.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay between store polls in [`wait_state`].
const POLL_INTERVAL: Duration = Duration::from_millis(10);

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("failed to spawn git");
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

/// Create a throwaway git repo with one commit on `main`.
fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.email", "agentmux-test@example.com"]);
    git(repo, &["config", "user.name", "agentmux test"]);
    std::fs::write(repo.join("README.md"), "# test repo\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "init"]);
    dir
}

/// The agent the orchestrator can actually spawn: the real mock binary.
fn mock_profile() -> AgentProfile {
    AgentProfile {
        id: AgentId::new("mock"),
        name: "Mock Agent".into(),
        adapter: AdapterKind::Acp {
            command: common::mock_agent_binary(),
            args: vec![],
        },
        env: BTreeMap::new(),
        available: true,
    }
}

/// A configured but probed-unavailable agent (missing binary).
fn unavailable_profile() -> AgentProfile {
    AgentProfile {
        id: AgentId::new("mock-unavailable"),
        name: "Unavailable".into(),
        adapter: AdapterKind::Acp {
            command: PathBuf::from("/definitely/not/an/agent"),
            args: vec![],
        },
        env: BTreeMap::new(),
        available: false,
    }
}

struct TestEnv {
    /// Git repo hosting the worktrees (kept alive for the test).
    _repo: TempDir,
    /// agentmux data dir (store + session JSONL logs).
    data: TempDir,
    orch: Orchestrator,
    project_id: ProjectId,
}

fn setup() -> TestEnv {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();

    let store = Store::open(data.path()).unwrap();
    let project_id = ProjectId::new();
    store
        .insert_project(&Project {
            id: project_id,
            root_path: repo.path().to_path_buf(),
            name: "test-project".into(),
        })
        .unwrap();

    let cfg = Config {
        agents: vec![mock_profile(), unavailable_profile()],
    };
    let orch = Orchestrator::new(
        store,
        AgentRegistry::from_config(&cfg),
        data.path().to_path_buf(),
    );

    TestEnv {
        _repo: repo,
        data,
        orch,
        project_id,
    }
}

/// `create_workspace` + `create_session` against the mock agent.
async fn new_session(env: &TestEnv, workspace_name: &str) -> (WorkspaceId, SessionId) {
    let ws = env
        .orch
        .create_workspace(env.project_id, workspace_name, "main")
        .await
        .expect("create_workspace should succeed");
    let sid = env
        .orch
        .create_session(ws, &AgentId::new("mock"), None)
        .await
        .expect("create_session should succeed");
    (ws, sid)
}

/// Collect bus events until `pred` matches or `timeout` elapses; returns
/// every event seen up to and including the match.
async fn recv_until<F>(
    rx: &mut broadcast::Receiver<Event>,
    timeout: Duration,
    pred: F,
) -> Vec<Event>
where
    F: Fn(&Event) -> bool,
{
    let mut seen = Vec::new();
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return seen;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                let hit = pred(&event);
                seen.push(event);
                if hit {
                    return seen;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return seen,
        }
    }
}

/// Poll the persisted session state until `pred` holds; panics on timeout.
async fn wait_state(
    orch: &Orchestrator,
    session_id: SessionId,
    pred: impl Fn(&SessionState) -> bool,
) -> SessionState {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let state = orch
            .get_session(session_id)
            .unwrap()
            .unwrap_or_else(|| panic!("session {session_id} should exist"))
            .state;
        if pred(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for state predicate; last state: {state:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `orch.prompt` with a deadline: a stuck prompt must fail the test, not
/// hang it forever.
async fn prompt(orch: &Orchestrator, sid: SessionId, text: &str) -> agentmux_core::Result<()> {
    tokio::time::timeout(EVENT_TIMEOUT, orch.prompt(sid, text.to_string(), vec![]))
        .await
        .expect("prompt timed out")
}

/// Whether `e` is the mock's `tool_call` session update (the last update
/// of a normal turn — see the mock contract).
fn is_tool_call_update(e: &Event) -> bool {
    matches!(&e.kind, EventKind::SessionUpdate(v)
        if v["update"]["sessionUpdate"] == "tool_call")
}

/// Whether `e` is an `agent_message_chunk` for `session_id`; returns the
/// chunk's text.
fn chunk_text(e: &Event, session_id: SessionId) -> Option<&str> {
    if e.session_id != session_id {
        return None;
    }
    match &e.kind {
        EventKind::SessionUpdate(v) if v["update"]["sessionUpdate"] == "agent_message_chunk" => {
            v["update"]["content"]["text"].as_str()
        }
        _ => None,
    }
}

/// ① Full roundtrip: workspace → session → prompt; the mock's echo +
/// tool_call updates reach bus subscribers (restamped with the real
/// session id and a nonzero seq), and the session ends back at `Ready`.
#[tokio::test]
async fn prompt_roundtrip_streams_updates_and_returns_to_ready() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Ready
    );

    let mut rx = env.orch.subscribe();
    prompt(&env.orch, sid, "hi")
        .await
        .expect("prompt should complete");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        e.session_id == sid && is_tool_call_update(e)
    })
    .await;

    let session_events: Vec<&Event> = events.iter().filter(|e| e.session_id == sid).collect();
    let updates: Vec<&serde_json::Value> = session_events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SessionUpdate(v) => Some(v),
            _ => None,
        })
        .collect();
    assert!(
        updates.len() >= 2,
        "expected echo + tool_call updates, got {updates:?}"
    );
    assert_eq!(
        updates[0]["update"]["content"]["text"],
        serde_json::json!("mock reply: hi")
    );
    assert_eq!(updates[1]["update"]["kind"], serde_json::json!("edit"));

    // The orchestrator assigned real sequence numbers to adapter events.
    assert!(
        session_events.iter().all(|e| e.seq > 0),
        "events should carry nonzero seqs: {session_events:?}"
    );

    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Ready,
        "session should return to Ready after the turn"
    );
}

/// ② A prompt already in flight holds the per-session serialization
/// lock; a second `prompt` fails fast with `session busy`.
#[tokio::test]
async fn second_prompt_while_prompting_is_session_busy() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;
    let orch = Arc::new(env.orch);
    // `env.data`/`env._repo` keep the tempdirs alive to scope end.

    // The "hang" trigger keeps the first prompt pending forever.
    let o2 = orch.clone();
    let hanging = tokio::spawn(async move { o2.prompt(sid, "hang".into(), vec![]).await });

    // Once the store reports Prompting, the in-flight prompt holds the lock.
    wait_state(&orch, sid, |s| matches!(s, SessionState::Prompting)).await;

    let err = orch
        .prompt(sid, "second".to_string(), vec![])
        .await
        .expect_err("concurrent prompt must be rejected");
    assert!(
        err.to_string().contains("session busy"),
        "expected `session busy`, got {err}"
    );

    // Cleanup: killing the session kills the agent and resolves the
    // in-flight prompt with an error.
    orch.kill(sid).await.expect("kill should succeed");
    assert!(hanging.await.unwrap().is_err());
    wait_state(&orch, sid, |s| matches!(s, SessionState::Done)).await;
}

/// Review-round-1 regression test: `kill` landing while a prompt is in
/// flight must end in terminal `Done` — the interrupted turn errors out,
/// but `Done` is never clobbered back to `Prompting`/`Error` (both the
/// `prompt` path's `Prompting` transition and its error epilogue honour
/// `unless_terminal`).
#[tokio::test]
async fn kill_during_prompt_ends_done_and_is_never_resurrected() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;
    let orch = Arc::new(env.orch);

    // "hang" keeps the prompt in flight while `kill` lands.
    let o2 = orch.clone();
    let hanging = tokio::spawn(async move { o2.prompt(sid, "hang".into(), vec![]).await });
    wait_state(&orch, sid, |s| matches!(s, SessionState::Prompting)).await;

    orch.kill(sid).await.expect("kill should succeed");

    // The interrupted turn surfaces an error — its conn is gone.
    assert!(hanging.await.expect("prompt task panicked").is_err());

    // Let the whole event pipeline settle, then pin the terminal state.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Done,
        "a killed session must stay Done — never Error, never resurrected"
    );

    // The persisted trail shows the `→ Done`, and nothing ever leaves
    // it (no `Done → Prompting`, no `Done → Error` clobber).
    let log = orch.read_events(sid).unwrap();
    assert!(
        log.iter().any(|e| matches!(
            &e.kind,
            EventKind::StateChanged { to, .. } if matches!(to, SessionState::Done)
        )),
        "expected a → Done transition in {log:?}"
    );
    assert!(
        !log.iter().any(|e| matches!(
            &e.kind,
            EventKind::StateChanged { from, .. } if matches!(from, SessionState::Done)
        )),
        "no transition may leave Done: {log:?}"
    );
}

/// ③ The `"crash"` trigger kills the agent mid-turn: the prompt errors,
/// `AgentExited` reaches the bus, the session lands in `Error`, and the
/// JSONL event log exists with content.
#[tokio::test]
async fn crash_prompt_marks_error_and_persists_events() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;

    let mut rx = env.orch.subscribe();
    let result = prompt(&env.orch, sid, "please crash now").await;
    assert!(result.is_err(), "a crashing agent should fail the prompt");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        e.session_id == sid && matches!(e.kind, EventKind::AgentExited { .. })
    })
    .await;
    let exit = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::AgentExited { .. }))
        .map(|e| &e.kind);
    assert!(
        matches!(exit, Some(EventKind::AgentExited { code: Some(1) })),
        "expected AgentExited code 1, got {exit:?} in {events:?}"
    );

    let state = wait_state(&env.orch, sid, |s| matches!(s, SessionState::Error(_))).await;
    assert!(matches!(state, SessionState::Error(_)));

    // The event log was persisted.
    let log_path = env
        .data
        .path()
        .join("sessions")
        .join(format!("{sid}.jsonl"));
    assert!(log_path.is_file(), "missing event log {log_path:?}");
    assert!(
        std::fs::metadata(&log_path).unwrap().len() > 0,
        "event log should be non-empty"
    );
    assert!(
        !env.orch.read_events(sid).unwrap().is_empty(),
        "read_events should return the persisted events"
    );
}

/// ④ Two sessions share one workspace: after an activity line lands on
/// the blackboard, prompting session B injects the shared-context
/// preamble — visible in the mock's echo of the prompt text.
#[tokio::test]
async fn sibling_prompt_includes_shared_context_preamble() {
    let env = setup();
    let ws = env
        .orch
        .create_workspace(env.project_id, "shared", "main")
        .await
        .unwrap();
    let agent = AgentId::new("mock");
    let _sa = env.orch.create_session(ws, &agent, None).await.unwrap();
    let sb = env.orch.create_session(ws, &agent, None).await.unwrap();

    // Put a recognizable line on the shared blackboard.
    let worktree = env.orch.get_workspace(ws).unwrap().unwrap().worktree_path;
    append_activity(&worktree, "mock-agent", "MARKER: rewrote frobnicator.rs").unwrap();

    let mut rx = env.orch.subscribe();
    prompt(&env.orch, sb, "continue the work")
        .await
        .expect("prompt should complete");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        chunk_text(e, sb).is_some_and(|t| t.contains("MARKER: rewrote frobnicator.rs"))
    })
    .await;
    let text = events
        .iter()
        .find_map(|e| chunk_text(e, sb))
        .expect("expected the echoed prompt chunk for session B");

    assert!(
        text.contains("Shared blackboard"),
        "preamble should include context.md, got: {text}"
    );
    assert!(
        text.contains("MARKER: rewrote frobnicator.rs"),
        "preamble should include the activity line, got: {text}"
    );
    assert!(
        text.contains("continue the work"),
        "the user's own prompt text should still be there, got: {text}"
    );
}

/// ⑤ `create_session` rejects an unknown agent id — and a configured
/// agent that was probed unavailable.
#[tokio::test]
async fn create_session_rejects_unknown_or_unavailable_agent() {
    let env = setup();
    let ws = env
        .orch
        .create_workspace(env.project_id, "ws1", "main")
        .await
        .unwrap();

    let err = env
        .orch
        .create_session(ws, &AgentId::new("no-such-agent"), None)
        .await
        .expect_err("unknown agent id should fail");
    assert!(
        err.to_string().contains("no-such-agent"),
        "error should name the agent: {err}"
    );

    let err = env
        .orch
        .create_session(ws, &AgentId::new("mock-unavailable"), None)
        .await
        .expect_err("unavailable agent should fail");
    assert!(
        err.to_string().contains("not available"),
        "error should mention availability: {err}"
    );
}

/// ⑥ `resume` on a `Done` session spawns a fresh connection, returns the
/// session to `Ready`, and it accepts prompts again.
#[tokio::test]
async fn resume_done_session_respawns_to_ready() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;

    env.orch.kill(sid).await.expect("kill should succeed");
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Done
    );

    env.orch.resume(sid).await.expect("resume should succeed");
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Ready
    );

    // The fresh connection is usable: a new prompt echoes through the bus.
    let mut rx = env.orch.subscribe();
    prompt(&env.orch, sid, "back from the dead")
        .await
        .expect("prompt on a resumed session should work");
    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        chunk_text(e, sid).is_some_and(|t| t.contains("back from the dead"))
    })
    .await;
    assert!(
        events.iter().any(|e| chunk_text(e, sid).is_some()),
        "resumed session should echo the new prompt"
    );
}

/// Merge-blocker regression: a session persisted in a non-terminal state
/// is dead the moment the daemon process exits — its conn and fan-out
/// died with it. Before the boot sweep, a post-restart `Ready` row could
/// neither `prompt` ("not running") nor `resume` ("not terminal") — only
/// `kill` → `resume` reached it, and nothing marked it dead. A fresh
/// `Orchestrator` over the same data dir must sweep such rows to
/// `Error`, keeping the seq counter continuous with the old log.
#[tokio::test]
async fn restart_sweeps_stale_sessions_to_error_and_resumable() {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();

    // First daemon lifetime: a live `Ready` session with a persisted
    // event log. Dropping the orchestrator simulates the restart — the
    // conn dies with it but the row stays `Ready`.
    let sid;
    {
        let store = Store::open(data.path()).unwrap();
        let project_id = ProjectId::new();
        store
            .insert_project(&Project {
                id: project_id,
                root_path: repo.path().to_path_buf(),
                name: "test-project".into(),
            })
            .unwrap();
        let cfg = Config {
            agents: vec![mock_profile()],
        };
        let orch = Orchestrator::new(
            store,
            AgentRegistry::from_config(&cfg),
            data.path().to_path_buf(),
        );
        let ws = orch
            .create_workspace(project_id, "ws", "main")
            .await
            .unwrap();
        sid = orch
            .create_session(ws, &AgentId::new("mock"), None)
            .await
            .unwrap();
        assert_eq!(
            orch.get_session(sid).unwrap().unwrap().state,
            SessionState::Ready
        );
    }

    // Second daemon lifetime over the same data dir.
    let cfg = Config {
        agents: vec![mock_profile()],
    };
    let orch = Orchestrator::new(
        Store::open(data.path()).unwrap(),
        AgentRegistry::from_config(&cfg),
        data.path().to_path_buf(),
    );

    // The stale `Ready` row was swept to `Error` at boot…
    let state = orch.get_session(sid).unwrap().unwrap().state;
    assert!(
        matches!(&state, SessionState::Error(m) if m.contains("daemon restarted")),
        "stale session must be swept to Error, got {state:?}"
    );

    // …with a persisted `Ready → Error` StateChanged whose seq continues
    // the pre-restart log instead of restarting at 1.
    let log = orch.read_events(sid).unwrap();
    let sweep_seq = log
        .iter()
        .find(|e| {
            matches!(
                &e.kind,
                EventKind::StateChanged {
                    from: SessionState::Ready,
                    to: SessionState::Error(_),
                }
            )
        })
        .map(|e| e.seq)
        .expect("sweep must persist a Ready→Error StateChanged");
    let max_before = log
        .iter()
        .filter(|e| e.seq < sweep_seq)
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    assert_eq!(
        sweep_seq,
        max_before + 1,
        "the sweep event must continue the pre-restart seq: {log:?}"
    );

    // Recovery path: the swept session is resumable (Error is terminal),
    // and kill → resume brings it back to Ready on a fresh conn.
    orch.kill(sid).await.expect("kill on swept session");
    orch.resume(sid).await.expect("resume on swept session");
    assert_eq!(
        orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Ready
    );
}

/// A `pi` profile backed by the `tests/fake_pi.py` stub — same trick as
/// `pi_rpc_test.rs`'s `spawn_fake`, routed through the adapter registry.
fn pi_profile() -> AgentProfile {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_pi.py");
    assert!(
        script.is_file(),
        "fake pi stub missing at {}",
        script.display()
    );
    AgentProfile {
        id: AgentId::new("pi"),
        name: "Fake Pi".into(),
        adapter: AdapterKind::PiRpc {
            command: PathBuf::from("python3"),
            args: vec![script.to_string_lossy().into_owned()],
        },
        env: BTreeMap::new(),
        available: true,
    }
}

/// `setup()` with an extra pi agent alongside the mock.
fn setup_with_pi() -> TestEnv {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();

    let store = Store::open(data.path()).unwrap();
    let project_id = ProjectId::new();
    store
        .insert_project(&Project {
            id: project_id,
            root_path: repo.path().to_path_buf(),
            name: "test-project".into(),
        })
        .unwrap();

    let cfg = Config {
        agents: vec![mock_profile(), pi_profile()],
    };
    let orch = Orchestrator::new(
        store,
        AgentRegistry::from_config(&cfg),
        data.path().to_path_buf(),
    );
    TestEnv {
        _repo: repo,
        data,
        orch,
        project_id,
    }
}

/// The `perm` trigger's full lifecycle through the orchestrator: the bus
/// carries `PermissionRequest`, the fan-out steps
/// `Prompting → WaitingPermission`, `respond_permission` selects the
/// offered `always` option, `PermissionResolved` steps
/// `WaitingPermission → Prompting`, and the turn ends back at `Ready`.
#[tokio::test]
async fn permission_roundtrip_walks_waiting_permission_states() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;
    let orch = Arc::new(env.orch);
    let mut rx = orch.subscribe();

    let o2 = orch.clone();
    let turn = tokio::spawn(async move { o2.prompt(sid, "perm".into(), vec![]).await });

    // The request is parked: bus event + WaitingPermission state.
    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        e.session_id == sid && matches!(e.kind, EventKind::PermissionRequest { .. })
    })
    .await;
    let request_id = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::PermissionRequest { request_id, .. } if e.session_id == sid => {
                Some(request_id.clone())
            }
            _ => None,
        })
        .expect("PermissionRequest event should reach the bus");
    wait_state(&orch, sid, |s| matches!(s, SessionState::WaitingPermission)).await;

    // The turn still holds the session while it parks.
    let err = orch
        .prompt(sid, "second".to_string(), vec![])
        .await
        .expect_err("concurrent prompt must be rejected while parked");
    assert!(err.to_string().contains("session busy"), "{err}");

    orch.respond_permission(sid, &request_id, agentmux_core::PermissionDecision::AllowAlways)
        .expect("respond_permission should accept the parked request");
    turn.await
        .expect("prompt task panicked")
        .expect("prompt should complete once answered");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        e.session_id == sid
            && matches!(&e.kind, EventKind::SessionUpdate(v)
                if v["update"]["sessionUpdate"] == "agent_message_chunk"
                    && v["update"]["content"]["text"] == "permission outcome: selected:always")
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::PermissionResolved { request_id: r, outcome }
                if *r == request_id && outcome == "selected:always"
        )),
        "expected a paired PermissionResolved: {events:?}"
    );

    assert_eq!(
        wait_state(&orch, sid, |s| matches!(s, SessionState::Ready)).await,
        SessionState::Ready,
        "the session should return to Ready after the resolved turn"
    );

    // Both transitions were persisted in order — WaitingPermission is a
    // real state, not a UI fiction.
    let log = orch.read_events(sid).unwrap();
    let mut waiting = false;
    let mut back = false;
    for e in &log {
        match &e.kind {
            EventKind::StateChanged {
                from: SessionState::Prompting,
                to: SessionState::WaitingPermission,
            } => waiting = true,
            EventKind::StateChanged {
                from: SessionState::WaitingPermission,
                to: SessionState::Prompting,
            } if waiting => back = true,
            _ => {}
        }
    }
    assert!(
        waiting && back,
        "expected Prompting→WaitingPermission→Prompting in {log:?}"
    );
}

/// `respond_permission` on a session with no parked request fails, and so
/// does one on a session with no live connection.
#[tokio::test]
async fn respond_permission_without_a_parked_request_errors() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;

    let err = env
        .orch
        .respond_permission(sid, "req-1", agentmux_core::PermissionDecision::AllowOnce)
        .expect_err("nothing is parked — this must fail");
    assert!(err.to_string().contains("req-1"), "{err}");

    env.orch.kill(sid).await.unwrap();
    let err = env
        .orch
        .respond_permission(sid, "req-1", agentmux_core::PermissionDecision::AllowOnce)
        .expect_err("a killed session has no conn to answer on");
    assert!(
        err.to_string().contains("no live connection"),
        "{err}"
    );

    let err = env
        .orch
        .respond_permission(
            SessionId::new(),
            "req-1",
            agentmux_core::PermissionDecision::AllowOnce,
        )
        .expect_err("an unknown session id must fail");
    assert!(err.to_string().contains("not running"), "{err}");
}

/// Pi sessions have no permission protocol: `respond_permission` must
/// report a clear unsupported error instead of silently doing nothing.
#[tokio::test]
async fn respond_permission_on_pi_session_is_unsupported() {
    let env = setup_with_pi();
    let ws = env
        .orch
        .create_workspace(env.project_id, "pi-ws", "main")
        .await
        .unwrap();
    let sid = env
        .orch
        .create_session(ws, &AgentId::new("pi"), None)
        .await
        .expect("create_session with fake pi should succeed");

    let err = env
        .orch
        .respond_permission(sid, "req-9", agentmux_core::PermissionDecision::AllowOnce)
        .expect_err("pi has no permission protocol");
    assert!(
        err.to_string().contains("do not support permission"),
        "{err}"
    );
}

/// `kill` racing a parked permission: the conn teardown cancels the
/// request, the session lands `Done`, and the `PermissionResolved`
/// (or its `WaitingPermission → Prompting` step) can never resurrect it.
#[tokio::test]
async fn kill_during_waiting_permission_ends_done() {
    let env = setup();
    let (_ws, sid) = new_session(&env, "ws1").await;
    let orch = Arc::new(env.orch);
    let mut rx = orch.subscribe();

    let o2 = orch.clone();
    let turn = tokio::spawn(async move { o2.prompt(sid, "perm".into(), vec![]).await });

    // Wait for the parked request, then kill mid-wait.
    recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        e.session_id == sid && matches!(e.kind, EventKind::PermissionRequest { .. })
    })
    .await;
    wait_state(&orch, sid, |s| matches!(s, SessionState::WaitingPermission)).await;

    orch.kill(sid).await.expect("kill should succeed");
    let _ = turn.await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Done,
        "a killed session must stay Done even if PermissionResolved lands late"
    );
    let log = orch.read_events(sid).unwrap();
    assert!(
        !log.iter().any(|e| matches!(
            &e.kind,
            EventKind::StateChanged { from: SessionState::Done, .. }
        )),
        "no transition may leave Done: {log:?}"
    );
}
