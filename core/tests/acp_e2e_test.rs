//! End-to-end tests for [`AcpConn`] against the real mock ACP agent
//! (`agentmux-mock-agent`, see `mock-agent/` in the workspace root).
//!
//! # Locating the mock binary
//!
//! `env!("CARGO_BIN_EXE_<name>")` only works for binaries of the *same*
//! package, so cross-package we cannot rely on it. Instead each test
//! process builds the agent once (`$CARGO build -p agentmux-mock-agent`,
//! incremental after the first run) and resolves the artifact at
//! `$CARGO_TARGET_DIR/debug/agentmux-mock-agent`, falling back to
//! `<workspace>/target/debug/agentmux-mock-agent` — the same directory
//! cargo just linked it into.
//!
//! # Mock agent contract (see `mock-agent/src/main.rs`)
//!
//! - `initialize` → normal response (`agentInfo.name == "agentmux-mock-agent"`)
//! - `session/new` → fixed session id `mock-session-1`
//! - `session/prompt` → `agent_message_chunk` echoing
//!   `"mock reply: <prompt text>"`, then a `tool_call`
//!   (kind `edit`, location `src/lib.rs`), then `stop_reason = "end_turn"`
//! - prompt containing `"crash"` → process exits with code 1
//! - prompt containing `"exit42"` → process exits with code 42
//! - prompt containing `"hang"` → never responds (for timeout tests)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use agentmux_core::{AcpConn, Event, EventKind};

/// Generous deadline for event collection; the mock replies instantly, so
/// this only bites on regression.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build `agentmux-mock-agent` once per test process and return the path
/// of the produced debug binary.
fn mock_agent_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("core/ sits directly under the workspace root")
                .to_path_buf();
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "agentmux-mock-agent"])
                .current_dir(&workspace_root)
                .status()
                .expect("failed to invoke `cargo build -p agentmux-mock-agent`");
            assert!(
                status.success(),
                "`cargo build -p agentmux-mock-agent` failed"
            );

            // Mirror cargo's target-dir resolution: honour CARGO_TARGET_DIR
            // (relative paths are anchored at the workspace root, matching
            // cargo's behaviour for a workspace invocation), else
            // <workspace>/target.
            let target_dir = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .map(|p| {
                    if p.is_absolute() {
                        p
                    } else {
                        workspace_root.join(p)
                    }
                })
                .unwrap_or_else(|| workspace_root.join("target"));
            let binary = target_dir.join("debug").join("agentmux-mock-agent");
            assert!(
                binary.is_file(),
                "mock agent binary missing at {}",
                binary.display()
            );
            binary
        })
        .clone()
}

/// Spawn a connection to the mock agent; the tempdir is the child's cwd.
async fn spawn_mock() -> (AcpConn, tempfile::TempDir) {
    let binary = mock_agent_binary();
    let dir = tempfile::tempdir().unwrap();
    let conn = AcpConn::spawn(&binary, &[], &BTreeMap::new(), dir.path())
        .expect("mock agent should spawn");
    (conn, dir)
}

/// Collect events until `pred` matches or `timeout` elapses; returns every
/// event seen up to and including the match.
async fn recv_until<F>(
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
    timeout: Duration,
    pred: F,
) -> Vec<Event>
where
    F: Fn(&EventKind) -> bool,
{
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return seen;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                let hit = pred(&event.kind);
                seen.push(event);
                if hit {
                    return seen;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return seen,
        }
    }
}

/// Full initialize → new_session → prompt turn, asserting the two
/// `session/update` notifications from the contract arrive in order.
#[tokio::test]
async fn initialize_session_and_prompt_roundtrip() {
    let (mut conn, dir) = spawn_mock().await;
    let mut rx = conn.events();

    // initialize → normal response advertising the mock's identity.
    let init = conn.initialize().await.expect("initialize should succeed");
    assert_eq!(
        init["agentInfo"]["name"],
        serde_json::json!("agentmux-mock-agent"),
        "unexpected initialize response: {init}"
    );

    // session/new → fixed session id.
    let session_id = conn
        .new_session(dir.path())
        .await
        .expect("session/new should succeed");
    assert_eq!(session_id, "mock-session-1");

    // session/prompt → resolves once the mock finishes its turn.
    conn.prompt(&session_id, "hello agent".to_string())
        .await
        .expect("prompt should complete with end_turn");

    // The mock emits its updates before answering the prompt, so both are
    // already in the pipe; collect until the tool_call update shows up.
    let events = recv_until(
        &mut rx,
        EVENT_TIMEOUT,
        |k| matches!(k, EventKind::SessionUpdate(v) if v["update"]["sessionUpdate"] == "tool_call"),
    )
    .await;
    let updates: Vec<&serde_json::Value> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SessionUpdate(v) => Some(v),
            _ => None,
        })
        .collect();
    assert!(
        updates.len() >= 2,
        "expected at least 2 session updates, got {updates:?}"
    );

    // 1) agent_message_chunk echoing the prompt text.
    assert_eq!(
        updates[0]["update"]["sessionUpdate"],
        serde_json::json!("agent_message_chunk")
    );
    assert_eq!(updates[0]["sessionId"], serde_json::json!("mock-session-1"));
    assert_eq!(
        updates[0]["update"]["content"]["text"],
        serde_json::json!("mock reply: hello agent"),
        "chunk should echo the prompt text: {}",
        updates[0]
    );

    // 2) tool_call for an edit on src/lib.rs.
    assert_eq!(
        updates[1]["update"]["sessionUpdate"],
        serde_json::json!("tool_call")
    );
    assert_eq!(updates[1]["update"]["kind"], serde_json::json!("edit"));
    assert_eq!(
        updates[1]["update"]["locations"][0]["path"],
        serde_json::json!("src/lib.rs")
    );

    conn.shutdown().await.unwrap();
}

/// `crash` in the prompt text kills the agent; the connection must surface
/// `AgentExited` with exit code 1.
#[tokio::test]
async fn crash_prompt_exits_agent_with_code_1() {
    let (mut conn, dir) = spawn_mock().await;
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    // The process dies mid-turn; the prompt resolves with an error rather
    // than a response — the exit event is what we assert on.
    let _ = conn
        .prompt(&session_id, "please crash now".to_string())
        .await;

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |k| {
        matches!(k, EventKind::AgentExited { .. })
    })
    .await;
    let exit = events.last().expect("expected an AgentExited event");
    assert!(
        matches!(exit.kind, EventKind::AgentExited { code: Some(1) }),
        "expected exit code 1, got {:?}",
        exit.kind
    );

    conn.shutdown().await.unwrap();
}

/// `exit42` produces a distinctive exit code so AgentExited coverage can
/// tell codes apart.
#[tokio::test]
async fn exit42_prompt_reports_exit_code_42() {
    let (mut conn, dir) = spawn_mock().await;
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    let _ = conn.prompt(&session_id, "exit42".to_string()).await;

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |k| {
        matches!(k, EventKind::AgentExited { .. })
    })
    .await;
    let exit = events.last().expect("expected an AgentExited event");
    assert!(
        matches!(exit.kind, EventKind::AgentExited { code: Some(42) }),
        "expected exit code 42, got {:?}",
        exit.kind
    );

    conn.shutdown().await.unwrap();
}

/// `hang` never answers the prompt; the connection must stay usable —
/// `shutdown` still works while the prompt future is pending.
#[tokio::test]
async fn hang_prompt_never_resolves_but_connection_stays_alive() {
    let (mut conn, dir) = spawn_mock().await;

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    {
        let prompt = conn.prompt(&session_id, "hang".to_string());
        tokio::pin!(prompt);
        let elapsed = tokio::time::timeout(Duration::from_secs(2), &mut prompt).await;
        assert!(elapsed.is_err(), "a hanging prompt must not resolve");
        // Dropping the prompt future here releases the `&mut conn` borrow;
        // the request stays pending on the worker, which is the point.
    }

    // The worker dispatches each command on its own task, so a stuck
    // prompt does not wedge the connection.
    conn.shutdown().await.unwrap();
}
