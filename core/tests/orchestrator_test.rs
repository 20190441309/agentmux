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
async fn new_session(env: &mut TestEnv, workspace_name: &str) -> (WorkspaceId, SessionId) {
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
    let mut env = setup();
    let (_ws, sid) = new_session(&mut env, "ws1").await;
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
    let mut env = setup();
    let (_ws, sid) = new_session(&mut env, "ws1").await;
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

/// ③ The `"crash"` trigger kills the agent mid-turn: the prompt errors,
/// `AgentExited` reaches the bus, the session lands in `Error`, and the
/// JSONL event log exists with content.
#[tokio::test]
async fn crash_prompt_marks_error_and_persists_events() {
    let mut env = setup();
    let (_ws, sid) = new_session(&mut env, "ws1").await;

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
    let mut env = setup();
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
    let mut env = setup();
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
    let mut env = setup();
    let (_ws, sid) = new_session(&mut env, "ws1").await;

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
