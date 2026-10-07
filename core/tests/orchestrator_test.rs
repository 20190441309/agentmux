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
    AdapterKind, AgentId, AgentProfile, AgentRegistry, Config, ConnTimeouts, Event, EventKind,
    Project, ProjectId, SessionId, SessionRef, SessionState, Store, WorkspaceId,
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
        ..Config::default()
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

#[tokio::test]
async fn workbench_titles_are_validated_persisted_and_broadcast() {
    let env = setup();
    let (_, id) = new_session(&env, "names").await;
    let mut events = env.orch.subscribe();
    let name = env
        .orch
        .set_session_title(id, "  回归测试  ".into())
        .unwrap();
    assert!(matches!(&name.kind, EventKind::TitleChanged { title } if title == "回归测试"));
    let received = recv_until(&mut events, EVENT_TIMEOUT, |e| {
        e.seq == name.seq && e.session_id == id
    })
    .await;
    assert!(received.iter().any(|e| e == &name));
    assert!(env.orch.read_events(id).unwrap().iter().any(|e| e == &name));
    for bad in [
        " ".into(),
        "two\nlines".into(),
        "escape\x1b".into(),
        "two\u{2028}lines".into(),
        "two\u{2029}paragraphs".into(),
        "x".repeat(61),
    ] {
        assert!(env.orch.set_session_title(id, bad).is_err());
    }
    assert!(env
        .orch
        .set_session_title(SessionId::new(), "unknown".into())
        .is_err());
    env.orch.kill(id).await.unwrap();
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
        EventKind::SessionUpdate(v) => {
            let update = v.get("update").unwrap_or(v);
            if update["sessionUpdate"] == "agent_message_chunk" {
                update["content"]["text"].as_str()
            } else {
                None
            }
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
            EventKind::SessionUpdate(v) if v["update"]["sessionUpdate"] != "user_message_chunk" => {
                Some(v)
            }
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

    assert!(
        env.orch
            .read_events(sid)
            .unwrap()
            .iter()
            .any(|e| matches!(&e.kind,
        EventKind::SessionUpdate(v) if v["update"]["sessionUpdate"] == "user_message_chunk"
            && v["update"]["content"]["text"] == "hi")),
        "original prompt is replayable without injected context"
    );

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

#[tokio::test]
async fn acp_native_commands_are_not_prefixed_with_shared_context_or_references() {
    let env = setup();
    let (ws, sid) = new_session(&env, "commands").await;
    let other = env
        .orch
        .create_session(ws, &AgentId::new("mock"), None)
        .await
        .unwrap();
    let worktree = env.orch.get_workspace(ws).unwrap().unwrap().worktree_path;
    append_activity(
        &worktree,
        "mock",
        "shared activity must not precede slash commands",
    )
    .unwrap();
    let mut rx = env.orch.subscribe();
    env.orch
        .prompt(sid, "/resume".into(), vec![])
        .await
        .unwrap();
    let seen = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        chunk_text(e, sid).is_some_and(|t| t == "mock reply: /resume")
    })
    .await;
    assert!(seen
        .iter()
        .any(|e| chunk_text(e, sid).is_some_and(|t| t == "mock reply: /resume")));
    assert!(env
        .orch
        .prompt(
            sid,
            "/resume".into(),
            vec![SessionRef {
                session_id: other,
                event_seq: 0
            }]
        )
        .await
        .is_err());
    env.orch.kill(sid).await.unwrap();
    env.orch.kill(other).await.unwrap();
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
    let original_id = env.orch.get_session(sid).unwrap().unwrap().acp_session_id;

    env.orch.kill(sid).await.expect("kill should succeed");
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Done
    );

    let mut profile = mock_profile();
    profile.env.insert("MOCK_FORBID_NEW".into(), "1".into());
    env.orch.register_agent(profile).unwrap();
    env.orch
        .resume(sid)
        .await
        .expect("resume must load the native conversation, not create another");
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().acp_session_id,
        original_id
    );
    let history = env.orch.read_events(sid).unwrap();
    assert!(!history.iter().any(|e| e.starts_conversation()));
    assert!(!history
        .iter()
        .any(|e| chunk_text(e, sid).is_some_and(|t| t.contains("mock replay sentinel"))));
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

#[tokio::test]
async fn acp_command_catalog_survives_setup_and_native_restore_without_replaying_messages() {
    let env = setup();
    let mut profile = mock_profile();
    profile
        .env
        .insert("MOCK_COMMANDS".into(), "init,review".into());
    env.orch.register_agent(profile.clone()).unwrap();
    let (_, sid) = new_session(&env, "commands").await;
    // A prompt makes fanout catch up before inspecting the persisted log.
    env.orch
        .prompt(sid, "/review branch main".into(), vec![])
        .await
        .unwrap();
    let mut rx = env.orch.subscribe();
    profile
        .env
        .insert("MOCK_COMMANDS".into(), "new,refreshed".into());
    profile.env.insert("MOCK_FORBID_NEW".into(), "1".into());
    env.orch.kill(sid).await.unwrap();
    env.orch.register_agent(profile).unwrap();
    env.orch.resume(sid).await.unwrap();
    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        e.session_id == sid && e.available_commands().is_some()
    })
    .await;
    assert!(events.iter().any(|e| e
        .available_commands()
        .is_some_and(|commands| { commands.iter().any(|c| c["name"] == "refreshed") })));
    let history = env.orch.read_events(sid).unwrap();
    let catalogs: Vec<_> = history
        .iter()
        .filter_map(Event::available_commands)
        .collect();
    assert_eq!(catalogs.len(), 2);
    assert_eq!(catalogs[0][0]["name"], "init");
    assert_eq!(catalogs[1][1]["name"], "refreshed");
    assert!(!history
        .iter()
        .any(|e| chunk_text(e, sid).is_some_and(|t| t.contains("mock replay sentinel"))));
}

#[tokio::test]
async fn acp_catalog_survives_native_replay_larger_than_the_connection_buffer() {
    let env = setup();
    let mut profile = mock_profile();
    profile
        .env
        .insert("MOCK_COMMANDS".into(), "init,review".into());
    env.orch.register_agent(profile.clone()).unwrap();
    let (_, sid) = new_session(&env, "catalog-overflow").await;
    env.orch
        .prompt(sid, "before restore".into(), vec![])
        .await
        .unwrap();
    env.orch.kill(sid).await.unwrap();
    profile
        .env
        .insert("MOCK_COMMANDS".into(), "changed-command".into());
    profile
        .env
        .insert("MOCK_LOAD_REPLAY_COUNT".into(), "600".into());
    env.orch.register_agent(profile).unwrap();
    env.orch.resume(sid).await.unwrap();
    let history = env.orch.read_events(sid).unwrap();
    let latest = history
        .iter()
        .rev()
        .find_map(Event::available_commands)
        .unwrap();
    assert_eq!(latest[0]["name"], "changed-command");
    assert!(!history
        .iter()
        .any(|e| chunk_text(e, sid).is_some_and(|t| t.contains("mock replay sentinel"))));
}

#[tokio::test]
async fn acp_restore_capability_missing_or_load_failed_never_creates_replacement() {
    for flag in ["MOCK_NO_LOAD", "MOCK_LOAD_FAIL"] {
        let env = setup();
        let (_, sid) = new_session(&env, "restore").await;
        let original = env.orch.get_session(sid).unwrap().unwrap();
        env.orch.kill(sid).await.unwrap();
        let mut profile = mock_profile();
        profile.env.insert(flag.into(), "1".into());
        env.orch.register_agent(profile).unwrap();
        let error = env.orch.resume(sid).await.unwrap_err();
        if flag == "MOCK_NO_LOAD" {
            assert!(
                error
                    .to_string()
                    .contains("does not support native session restoration"),
                "{error:#}"
            );
        }
        let failed = env.orch.get_session(sid).unwrap().unwrap();
        assert!(matches!(failed.state, SessionState::Error(_)));
        assert_eq!(failed.acp_session_id, original.acp_session_id);
        assert!(!env
            .orch
            .read_events(sid)
            .unwrap()
            .iter()
            .any(|e| e.starts_conversation()));
        env.orch.register_agent(mock_profile()).unwrap();
        env.orch.resume(sid).await.unwrap();
        assert_eq!(
            env.orch.get_session(sid).unwrap().unwrap().acp_session_id,
            original.acp_session_id
        );
        env.orch.kill(sid).await.unwrap();
    }
}

#[tokio::test]
async fn failed_initial_setup_can_retry_without_an_existing_native_conversation() {
    let env = setup();
    let ws = env
        .orch
        .create_workspace(env.project_id, "retry", "main")
        .await
        .unwrap();
    let mut profile = mock_profile();
    profile.adapter = AdapterKind::Acp {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 1".into()],
    };
    env.orch.register_agent(profile).unwrap();
    let error = env
        .orch
        .create_session(ws, &AgentId::new("mock"), None)
        .await
        .unwrap_err();
    let sid = error
        .downcast_ref::<agentmux_core::orchestrator::SessionSetupFailure>()
        .unwrap()
        .session_id;
    assert!(env
        .orch
        .get_session(sid)
        .unwrap()
        .unwrap()
        .acp_session_id
        .is_none());
    env.orch.register_agent(mock_profile()).unwrap();
    env.orch.resume(sid).await.unwrap();
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Ready
    );
    env.orch.kill(sid).await.unwrap();
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
            ..Config::default()
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
        ..Config::default()
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
        ..Config::default()
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

#[tokio::test]
async fn pi_restore_after_daemon_restart_retains_native_context_and_workbench_history() {
    let env = setup_with_pi();
    let ws = env
        .orch
        .create_workspace(env.project_id, "native", "main")
        .await
        .unwrap();
    let sid = env
        .orch
        .create_session(ws, &AgentId::new("pi"), None)
        .await
        .unwrap();
    env.orch
        .prompt(sid, "native_context_secret".into(), vec![])
        .await
        .unwrap();
    env.orch
        .set_session_title(sid, "keep this title".into())
        .unwrap();
    let original = env.orch.get_session(sid).unwrap().unwrap();
    assert!(original.native_session_file.as_ref().unwrap().is_file());
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !env
        .orch
        .read_events(sid)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, EventKind::SessionUpdate(v) if v["type"] == "agent_settled"))
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let original_history = env.orch.read_events(sid).unwrap();
    drop(env.orch);
    let cfg = Config {
        agents: vec![pi_profile()],
        ..Config::default()
    };
    let orch = Orchestrator::new(
        Store::open(env.data.path()).unwrap(),
        AgentRegistry::from_config(&cfg),
        env.data.path().to_path_buf(),
    );
    orch.resume(sid).await.unwrap();
    let restored = orch.get_session(sid).unwrap().unwrap();
    assert_eq!(restored.acp_session_id, original.acp_session_id);
    assert_eq!(restored.native_session_file, original.native_session_file);
    assert_eq!(restored.state, SessionState::Ready);
    assert_eq!(
        orch.pi_command(sid, serde_json::json!({"type":"get_state"}))
            .await
            .unwrap()["messageCount"],
        1
    );
    let history = orch.read_events(sid).unwrap();
    assert!(history.starts_with(&original_history));
    assert!(!history.iter().any(|e| e.resets_conversation_view()));
    let mut rx = orch.subscribe();
    orch.prompt(sid, "recallprobe".into(), vec![])
        .await
        .unwrap();
    let seen = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        chunk_text(e, sid).is_some_and(|t| t.contains("remembered context: native_context_secret"))
    })
    .await;
    assert!(
        seen.iter().any(|e| chunk_text(e, sid)
            .is_some_and(|t| t.contains("remembered context: native_context_secret"))),
        "the agent must recall native context, without event-log injection"
    );
    orch.kill(sid).await.unwrap();
}

#[tokio::test]
async fn pi_restore_cancel_failure_and_wrong_identity_preserve_original_locator() {
    for flag in [
        "FAKE_PI_CANCEL_SWITCH",
        "FAKE_PI_FAIL_SWITCH",
        "FAKE_PI_WRONG_ID",
    ] {
        let env = setup_with_pi();
        let ws = env
            .orch
            .create_workspace(env.project_id, "native", "main")
            .await
            .unwrap();
        let sid = env
            .orch
            .create_session(ws, &AgentId::new("pi"), None)
            .await
            .unwrap();
        let original = env.orch.get_session(sid).unwrap().unwrap();
        env.orch.kill(sid).await.unwrap();
        let mut profile = pi_profile();
        profile.env.insert(flag.into(), "1".into());
        env.orch.register_agent(profile).unwrap();
        assert!(env.orch.resume(sid).await.is_err(), "{flag}");
        let failed = env.orch.get_session(sid).unwrap().unwrap();
        assert_eq!(failed.acp_session_id, original.acp_session_id);
        assert_eq!(failed.native_session_file, original.native_session_file);
        assert!(matches!(failed.state, SessionState::Error(_)));
        assert!(!env
            .orch
            .read_events(sid)
            .unwrap()
            .iter()
            .any(|e| e.starts_conversation()));
        env.orch.register_agent(pi_profile()).unwrap();
        env.orch.resume(sid).await.unwrap();
        env.orch.kill(sid).await.unwrap();
    }
}

#[tokio::test]
async fn pi_missing_or_legacy_locator_does_not_silently_start_a_new_conversation() {
    for legacy in [true, false] {
        let env = setup_with_pi();
        let ws = env
            .orch
            .create_workspace(env.project_id, "native", "main")
            .await
            .unwrap();
        let sid = env
            .orch
            .create_session(ws, &AgentId::new("pi"), None)
            .await
            .unwrap();
        let original = env.orch.get_session(sid).unwrap().unwrap();
        env.orch.kill(sid).await.unwrap();
        if legacy {
            Store::open(env.data.path())
                .unwrap()
                .set_native_session(sid, original.acp_session_id.as_deref().unwrap(), None)
                .unwrap();
        } else {
            std::fs::remove_file(original.native_session_file.as_ref().unwrap()).unwrap();
        }
        assert!(env.orch.resume(sid).await.is_err());
        let failed = env.orch.get_session(sid).unwrap().unwrap();
        assert_eq!(failed.acp_session_id, original.acp_session_id);
        assert_eq!(
            failed.native_session_file,
            if legacy {
                None
            } else {
                original.native_session_file
            }
        );
        assert!(matches!(failed.state, SessionState::Error(_)));
        assert!(!env
            .orch
            .read_events(sid)
            .unwrap()
            .iter()
            .any(|e| e.starts_conversation()));
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

    orch.respond_permission(
        sid,
        &request_id,
        agentmux_core::PermissionDecision::AllowAlways,
    )
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
    assert!(err.to_string().contains("no live connection"), "{err}");

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

/// A `SessionRef` into a pi session must render real relay context: the
/// pi session's persisted (normalized + passthrough) events feed
/// `describe_event`, whose lines land in the referenced block the mock
/// echoes back. Before pi-shape awareness this produced an empty
/// "Context from" shell.
#[tokio::test]
async fn session_ref_into_pi_session_renders_event_text() {
    let env = setup_with_pi();
    let ws = env
        .orch
        .create_workspace(env.project_id, "pi-ws", "main")
        .await
        .unwrap();
    let sa = env
        .orch
        .create_session(ws, &AgentId::new("pi"), None)
        .await
        .expect("pi session should start");
    let sb = env
        .orch
        .create_session(ws, &AgentId::new("mock"), None)
        .await
        .expect("mock session should start");

    // Run the pi turn so its log has content worth relaying.
    prompt(&env.orch, sa, "relay me")
        .await
        .expect("pi prompt should settle");

    // The fan-out persists asynchronously; wait until the turn's
    // `agent_settled` is in the log before referencing it.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let log = env.orch.read_events(sa).unwrap();
        if log.iter().any(|e| {
            matches!(
                &e.kind,
                EventKind::SessionUpdate(v) if v["type"] == "agent_settled"
            )
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "pi events never persisted");
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Prompt the mock session with a reference into the pi session; the
    // mock echoes the whole composed prompt back as a chunk.
    let mut rx = env.orch.subscribe();
    env.orch
        .prompt(
            sb,
            "absorb this".to_string(),
            vec![SessionRef {
                session_id: sa,
                event_seq: 0,
            }],
        )
        .await
        .expect("relay prompt should complete");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        chunk_text(e, sb).is_some_and(|t| t.contains("Context from session"))
    })
    .await;
    let text = events
        .iter()
        .find_map(|e| chunk_text(e, sb))
        .expect("expected the echoed prompt for session B");

    assert!(
        text.contains(&format!("Context from session {sa}")),
        "the ref block header should name the pi session, got: {text}"
    );
    // The pi turn's text (normalized `agent_message_chunk` on the wire,
    // plus the passthrough `message_end`) renders as `message:` lines.
    // The delta itself echoes sa's composed prompt, which includes the
    // shared-context preamble — so match the prefix, not the tail.
    assert!(
        text.contains("- message: fake pi reply:"),
        "the pi delta text must render in the relay block, got: {text}"
    );
    assert!(
        text.contains("- run settled"),
        "the passthrough agent_settled should summarize, got: {text}"
    );
    assert!(
        !text.contains("{\"type\""),
        "no raw pi JSON should leak into the block, got: {text}"
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
            EventKind::StateChanged {
                from: SessionState::Done,
                ..
            }
        )),
        "no transition may leave Done: {log:?}"
    );
}

/// The configured `prompt_timeout` unwedges a stuck turn end to end:
/// the conn's prompt resolves with a timeout error, the orchestrator's
/// existing error path moves the session to `Error`, and the conn's
/// "timed out" `Orchestrator` note is persisted into the event log.
/// `kill` still cleans the wedged session up.
#[tokio::test]
async fn prompt_timeout_marks_session_error() {
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
        agents: vec![mock_profile()],
        timeouts: ConnTimeouts {
            init: Duration::from_secs(10),
            prompt: Duration::from_secs(1),
        },
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
    let sid = orch
        .create_session(ws, &AgentId::new("mock"), None)
        .await
        .expect("create_session should succeed");

    // The mock's `hang` trigger never answers — the 1 s prompt bound is
    // the only way this resolves.
    let err = orch
        .prompt(sid, "hang".to_string(), vec![])
        .await
        .expect_err("a wedged turn must fail at the prompt bound");
    assert!(err.to_string().contains("timed out"), "{err}");

    let state = wait_state(&orch, sid, |s| matches!(s, SessionState::Error(_))).await;
    assert!(
        matches!(&state, SessionState::Error(m) if m.contains("timed out")),
        "the timeout error should land in the session state, got {state:?}"
    );

    // The conn emitted an Orchestrator note; the fan-out persists it.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let log = orch.read_events(sid).unwrap();
        if log
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Orchestrator(m) if m.contains("timed out")))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the 'timed out' note was never persisted: {log:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // The timed-out turn's agent is stopped after a short grace period,
    // not left running behind an `Error` state that lets workspace
    // removal proceed.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match orch.cancel(sid).await {
            Err(err) if err.to_string().contains("no live connection") => break,
            _ => {
                assert!(
                    Instant::now() < deadline,
                    "a live connection remained after the failed turn"
                );
                tokio::time::sleep(POLL_INTERVAL * 10).await;
            }
        }
    }

    orch.kill(sid).await.expect("kill should succeed");
}

/// Agent stderr end to end: a `noisy` turn kills the mock with 15
/// stderr lines; the drain writes `<data_dir>/sessions/<id>.stderr.log`
/// (all 15 lines — the ~12-line truncation is in-memory only), the
/// session lands in `Error`, and a persisted `Orchestrator` note carries
/// the tail. `delete_workspace` removes the stderr log alongside the
/// event log.
#[tokio::test]
async fn stderr_log_is_written_and_cleaned_on_workspace_delete() {
    let env = setup();
    let (ws, sid) = new_session(&env, "ws-stderr").await;

    let err = prompt(&env.orch, sid, "noisy")
        .await
        .expect_err("a crashing turn must fail the prompt");
    assert!(
        format!("{err:#}").contains("mock stderr line 15"),
        "orchestrator error should carry the stderr tail: {err:#}"
    );

    let state = wait_state(&env.orch, sid, |s| matches!(s, SessionState::Error(_))).await;
    assert!(matches!(state, SessionState::Error(_)), "{state:?}");

    // The session-scoped stderr log exists and holds every line.
    let log_path = env
        .data
        .path()
        .join("sessions")
        .join(format!("{sid}.stderr.log"));
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if log_path
            .exists()
            .then(|| std::fs::read_to_string(&log_path).unwrap())
            .is_some_and(|c| c.contains("mock stderr line 15\n"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stderr log never materialized at {}",
            log_path.display()
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        content.contains("mock stderr line 1\n") && content.contains("mock stderr line 15\n"),
        "log should hold all 15 lines: {content:?}"
    );

    // The tail note is persisted into the session's JSONL event log.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let log = env.orch.read_events(sid).unwrap();
        if log
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Orchestrator(m) if m.contains("stderr tail")))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the stderr-tail note was never persisted: {log:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Workspace removal removes the stderr log along with the event log
    // (the session is `Error`, i.e. non-live, so removal is allowed).
    assert!(env.orch.remove_workspace(ws).unwrap());
    assert!(
        !log_path.exists(),
        "stderr log should be removed on workspace delete"
    );
}

#[tokio::test]
async fn pi_commands_preserve_slash_prefix_and_expose_controls() {
    let env = setup_with_pi();
    let ws = env
        .orch
        .create_workspace(env.project_id, "slash", "main")
        .await
        .unwrap();
    let sid = env
        .orch
        .create_session(ws, &AgentId::new("pi"), None)
        .await
        .unwrap();
    let _other = env
        .orch
        .create_session(ws, &AgentId::new("pi"), None)
        .await
        .unwrap();
    let commands = env
        .orch
        .pi_command(sid, serde_json::json!({"type":"get_commands"}))
        .await
        .unwrap();
    assert_eq!(commands["commands"][0]["name"], "fix-tests");
    env.orch
        .pi_command(
            sid,
            serde_json::json!({"type":"set_thinking_level", "level":"high"}),
        )
        .await
        .unwrap();
    assert!(env
        .orch
        .pi_command(sid, serde_json::json!({"type":"bash", "command":"false"}))
        .await
        .is_err());
    let mut rx = env.orch.subscribe();
    env.orch
        .prompt(sid, "/fix-tests argument".into(), vec![])
        .await
        .unwrap();
    let events = recv_until(&mut rx, EVENT_TIMEOUT, |e| {
        chunk_text(e, sid).is_some_and(|t| t.contains("fake pi reply:"))
    })
    .await;
    let reply = events.iter().find_map(|e| chunk_text(e, sid)).unwrap();
    assert_eq!(reply, "fake pi reply: /fix-tests argument");
    assert_eq!(
        env.orch.get_session(sid).unwrap().unwrap().state,
        SessionState::Ready
    );
    env.orch.kill(sid).await.unwrap();
    assert!(env
        .orch
        .pi_command(sid, serde_json::json!({"type":"get_state"}))
        .await
        .is_err());
}

/// `session/prompt` on a native PTY session is refused up front: the
/// session must stay live instead of being marked `Error` under a
/// still-running terminal child.
#[tokio::test]
async fn prompt_on_native_session_is_rejected_without_killing_it() {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    let project_id = ProjectId::new();
    store
        .insert_project(&Project {
            id: project_id,
            root_path: repo.path().to_path_buf(),
            name: "native-project".into(),
        })
        .unwrap();
    let native = AgentProfile {
        id: AgentId::new("pty"),
        name: "PTY".into(),
        adapter: AdapterKind::Native {
            command: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "sleep 30".into()],
            session_backend: None,
            resume_args: vec![],
            history_args: vec![],
        },
        env: BTreeMap::new(),
        available: true,
    };
    let cfg = Config {
        agents: vec![native],
        ..Config::default()
    };
    let orch = Orchestrator::new(
        store,
        AgentRegistry::from_config(&cfg),
        data.path().to_path_buf(),
    );
    let ws = orch
        .create_workspace(project_id, "native", "main")
        .await
        .unwrap();
    let sid = orch
        .create_session(ws, &AgentId::new("pty"), None)
        .await
        .unwrap();
    let before = orch.get_session(sid).unwrap().unwrap().state;
    let err = orch
        .prompt(sid, "hello".into(), vec![])
        .await
        .expect_err("native sessions take input in their own terminal");
    assert!(err.to_string().contains("native terminal"), "{err:#}");
    let after = orch.get_session(sid).unwrap().unwrap().state;
    assert_eq!(after, before, "a refused prompt must not change state");
    assert!(!matches!(after, SessionState::Error(_)));
    orch.kill(sid).await.unwrap();
}
