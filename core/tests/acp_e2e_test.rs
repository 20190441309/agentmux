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
//! - prompt containing `"perm"` → the agent calls
//!   `session/request_permission` (allow/always/deny options) mid-turn,
//!   then reports the answer as a `"permission outcome: …"` chunk

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use agentmux_core::{AcpConn, Event, EventKind, PermissionDecision};

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

/// The parked prompt future, boxed so it can leave this helper's frame.
type ParkedPrompt<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = agentmux_core::Result<()>> + 'a>>;

/// Run `conn.prompt` against the `perm` trigger and pull events until the
/// `PermissionRequest` lands; returns the still-parked prompt future (so
/// the caller can keep asserting "not done yet") and the request_id.
async fn park_perm_prompt<'a>(
    conn: &'a AcpConn,
    session_id: &'a str,
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
) -> (ParkedPrompt<'a>, String) {
    let mut prompt: ParkedPrompt<'_> =
        Box::pin(conn.prompt(session_id, "please perm now".to_string()));
    let request_id = loop {
        tokio::select! {
            r = &mut prompt => panic!("prompt finished before its permission was answered: {r:?}"),
            e = rx.recv() => {
                if let Ok(Event { kind: EventKind::PermissionRequest { request_id, request }, .. }) = e {
                    // The raw request is on the wire for the UI: three
                    // offered options, the tool call, the session id.
                    assert_eq!(
                        request["options"].as_array().map(|o| o.len()),
                        Some(3),
                        "expected allow/always/deny options: {request}"
                    );
                    assert_eq!(
                        request["toolCall"]["title"],
                        serde_json::json!("mock edit of src/lib.rs"),
                        "{request}"
                    );
                    break request_id;
                }
            }
        }
    };
    (prompt, request_id)
}

/// Whether `k` is the mock's `"permission outcome: <text>"` chunk.
fn is_perm_outcome(k: &EventKind, text: &str) -> bool {
    matches!(k, EventKind::SessionUpdate(v)
        if v["update"]["sessionUpdate"] == "agent_message_chunk"
            && v["update"]["content"]["text"] == format!("permission outcome: {text}"))
}

/// `perm` parks the turn on `session/request_permission`; answering
/// `allow_once` selects the mock's `allow` option — visible both in the
/// paired `PermissionResolved` event and the agent's own outcome chunk.
#[tokio::test]
async fn perm_prompt_parks_until_allowed() {
    let (mut conn, dir) = spawn_mock().await;
    let mut rx = conn.events();
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    let (prompt, request_id) = park_perm_prompt(&conn, &session_id, &mut rx).await;

    conn.respond_permission(&request_id, PermissionDecision::AllowOnce)
        .expect("respond_permission should accept a parked request");
    prompt.await.expect("the prompt completes once answered");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |k| {
        is_perm_outcome(k, "selected:allow")
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::PermissionResolved { request_id: r, outcome }
                if *r == request_id && outcome == "selected:allow"
        )),
        "expected a paired PermissionResolved: {events:?}"
    );
    conn.shutdown().await.unwrap();
}

/// `reject` maps onto the mock's `deny` option (kind `reject_once`).
#[tokio::test]
async fn perm_prompt_reject_selects_deny() {
    let (mut conn, dir) = spawn_mock().await;
    let mut rx = conn.events();
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    let (prompt, request_id) = park_perm_prompt(&conn, &session_id, &mut rx).await;

    conn.respond_permission(&request_id, PermissionDecision::Reject)
        .unwrap();
    prompt.await.expect("a rejected prompt still completes");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |k| {
        is_perm_outcome(k, "selected:deny")
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::PermissionResolved { outcome, .. } if outcome == "selected:deny"
        )),
        "expected PermissionResolved selected:deny: {events:?}"
    );
    conn.shutdown().await.unwrap();
}

/// `respond_permission` on an unknown request id errors cleanly; a
/// resolved id errors the same way (the entry was consumed).
#[tokio::test]
async fn respond_permission_unknown_or_consumed_id_errors() {
    let (mut conn, dir) = spawn_mock().await;
    let mut rx = conn.events();
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    let (prompt, request_id) = park_perm_prompt(&conn, &session_id, &mut rx).await;
    let err = conn
        .respond_permission("no-such-request", PermissionDecision::AllowOnce)
        .expect_err("unknown request_id must fail");
    assert!(err.to_string().contains("no-such-request"), "{err}");

    conn.respond_permission(&request_id, PermissionDecision::Cancel)
        .unwrap();
    assert!(
        conn.respond_permission(&request_id, PermissionDecision::AllowOnce)
            .is_err(),
        "an already-answered request_id must fail"
    );
    prompt.await.expect("cancel resolves the turn");

    let events = recv_until(&mut rx, EVENT_TIMEOUT, |k| is_perm_outcome(k, "cancelled")).await;
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::PermissionResolved { outcome, .. } if outcome == "cancelled"
        )),
        "cancel resolves as the cancelled outcome: {events:?}"
    );
    conn.shutdown().await.unwrap();
}

/// Closing the conn mid-parked-request resolves it `cancelled` — the
/// agent's turn unwinds instead of parking forever.
#[tokio::test]
async fn close_cancels_a_parked_permission() {
    let (conn, dir) = spawn_mock().await;
    let mut rx = conn.events();
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    let (prompt, _request_id) = park_perm_prompt(&conn, &session_id, &mut rx).await;
    conn.close();
    // The prompt unwinds one way or another — conn death fails it.
    let _ = tokio::time::timeout(EVENT_TIMEOUT, prompt)
        .await
        .expect("a killed conn must not leave the prompt hanging");
}
