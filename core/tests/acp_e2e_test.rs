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

use agentmux_core::{AcpConn, ConnTimeouts, Event, EventKind, PermissionDecision, SpawnOptions};

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
    spawn_mock_opts(&SpawnOptions::default()).await
}

/// [`spawn_mock`] with explicit spawn options (timeouts, stderr log).
async fn spawn_mock_opts(opts: &SpawnOptions) -> (AcpConn, tempfile::TempDir) {
    let binary = mock_agent_binary();
    let dir = tempfile::tempdir().unwrap();
    let conn = AcpConn::spawn(&binary, &[], &BTreeMap::new(), dir.path(), opts)
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

/// The configured `prompt` bound: a wedged turn resolves with a timeout
/// error exactly at the bound — under the paused clock the 30 s virtual
/// deadline fires the instant the caller parks on the reply (external
/// replies always lose to auto-advance, so this pins the timeout path
/// itself). `cancel` takes no timeout and still resolves.
#[tokio::test(start_paused = true)]
async fn prompt_times_out_on_the_configured_bound_virtual() {
    let opts = SpawnOptions {
        timeouts: ConnTimeouts {
            init: Duration::from_secs(10),
            prompt: Duration::from_secs(30),
        },
        ..SpawnOptions::default()
    };
    let (mut conn, _dir) = spawn_mock_opts(&opts).await;
    let mut rx = conn.events();

    let err = conn
        .prompt("mock-session-1", "hang".to_string())
        .await
        .expect_err("a wedged turn must error at the prompt bound");
    assert!(
        err.to_string().contains("timed out"),
        "expected a timeout error, got: {err}"
    );

    // The conn emitted an Orchestrator note naming the timeout — the
    // orchestrator's fan-out persists these into the session log.
    let mut notes = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        notes.push(ev);
    }
    assert!(
        notes.iter().any(|e| matches!(
            &e.kind,
            EventKind::Orchestrator(m) if m.contains("timed out")
        )),
        "expected a 'timed out' Orchestrator note, got {notes:?}"
    );

    // CANCEL is not subject to the prompt bound — it answers right away
    // even with the turn still wedged agent-side.
    conn.cancel("mock-session-1")
        .await
        .expect("cancel must resolve immediately");
    conn.shutdown().await.unwrap();
}

/// Real-clock counterpart: the 1 s bound frees the caller from the mock's
/// `hang` turn (~1 s, not forever), while a healthy turn under the same
/// conn completes well inside the bound.
#[tokio::test]
async fn prompt_timeout_frees_the_caller_and_cancel_still_works() {
    let opts = SpawnOptions {
        timeouts: ConnTimeouts {
            init: Duration::from_secs(10),
            prompt: Duration::from_secs(1),
        },
        ..SpawnOptions::default()
    };
    let (mut conn, dir) = spawn_mock_opts(&opts).await;
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    let start = std::time::Instant::now();
    let err = conn
        .prompt(&session_id, "hang".to_string())
        .await
        .expect_err("a wedged turn must fail at the prompt bound");
    let elapsed = start.elapsed();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(5),
        "bound should fire near 1s, took {elapsed:?}"
    );

    // The wedged worker-side request does not wedge the conn: cancel
    // resolves, and the next prompt works once the turn unwinds.
    conn.cancel(&session_id)
        .await
        .expect("cancel should resolve");
    conn.shutdown().await.unwrap();
}

/// Agent stderr is piped, drained into the configured log file, and the
/// retained tail surfaces twice: appended to the failed prompt's error
/// and as an `Orchestrator` note emitted before `AgentExited`. The
/// mock's `noisy` trigger writes 15 lines then exits 3 — one more than
/// the 12-line tail, so truncation is exercised too.
#[tokio::test]
async fn stderr_is_logged_and_tail_surfaces_on_crash() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("agent.stderr.log");
    let opts = SpawnOptions {
        stderr_log: Some(log_path.clone()),
        ..SpawnOptions::default()
    };
    let (mut conn, work) = spawn_mock_opts(&opts).await;
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(work.path()).await.unwrap();
    let mut rx = conn.events();

    let err = conn
        .prompt(&session_id, "noisy".to_string())
        .await
        .expect_err("a crashing prompt must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mock stderr line 15"),
        "error should carry the stderr tail, got: {msg}"
    );
    assert!(
        !msg.contains("mock stderr line 3"),
        "tail should keep only the last ~12 lines, got: {msg}"
    );

    // The exit watcher emits the tail as an Orchestrator note, then
    // AgentExited{Some(3)} — order asserted by position.
    let events = recv_until(&mut rx, EVENT_TIMEOUT, |k| {
        matches!(k, EventKind::AgentExited { .. })
    })
    .await;
    let note_pos = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::Orchestrator(m) if m.contains("stderr tail")))
        .expect("a stderr-tail Orchestrator note should precede AgentExited");
    let note = match &events[note_pos].kind {
        EventKind::Orchestrator(m) => m,
        _ => unreachable!(),
    };
    assert!(note.contains("mock stderr line 15"), "{note}");
    assert!(!note.contains("mock stderr line 3"), "{note}");
    assert!(
        note_pos < events.len() - 1
            && matches!(
                events.last().unwrap().kind,
                EventKind::AgentExited { code: Some(3) }
            ),
        "expected note then AgentExited{{3}}, got {events:?}"
    );

    // The on-disk log keeps ALL 15 lines — tail truncation is in-memory
    // only.
    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(content.contains("mock stderr line 1\n"), "{content:?}");
    assert!(content.contains("mock stderr line 15\n"), "{content:?}");

    conn.shutdown().await.unwrap();
}

/// An agent that dies during the initialize handshake fails the call
/// with its last stderr line on the error — no `noisy` trigger needed,
/// `sh` exits before answering.
#[tokio::test]
async fn init_failure_error_carries_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let conn = AcpConn::spawn(
        Path::new("sh"),
        &[
            "-c".to_string(),
            "echo 'init boom on stderr' >&2; exit 7".to_string(),
        ],
        &BTreeMap::new(),
        dir.path(),
        &SpawnOptions::default(),
    )
    .expect("sh should spawn");

    let err = conn
        .initialize()
        .await
        .expect_err("a dead agent must fail initialize");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("init boom on stderr"),
        "init failure should carry the stderr tail, got: {msg}"
    );
}
