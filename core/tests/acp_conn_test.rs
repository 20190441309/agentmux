//! Tests for [`AcpConn`], the ACP client connection wrapper.
//!
//! These tests exercise process management and the parts of the protocol
//! surface that do not require a real agent: spawn semantics (env, cwd),
//! the `initialize` timeout, and the event stream. A mock ACP agent for
//! full request/response integration testing lands in Task 6.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use agentmux_core::{AcpConn, EventKind};

/// Collect events until `pred` matches or `timeout` elapses; returns the
/// matching event.
async fn recv_until<F>(
    rx: &mut tokio::sync::broadcast::Receiver<agentmux_core::Event>,
    timeout: Duration,
    pred: F,
) -> Option<agentmux_core::Event>
where
    F: Fn(&EventKind) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) if pred(&event.kind) => return Some(event),
            Ok(Ok(_)) => continue,
            // A lagging receiver skips ahead rather than giving up.
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        }
    }
}

#[test]
fn spawn_fails_for_missing_command() {
    let dir = tempfile::tempdir().unwrap();
    let result = AcpConn::spawn(
        Path::new("/nonexistent/agentmux-agent"),
        &[],
        &BTreeMap::new(),
        dir.path(),
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn spawn_applies_env_and_cwd_and_reports_child_exit() {
    let dir = tempfile::tempdir().unwrap();
    let env = BTreeMap::from([("AGENTMUX_ENV_PROBE".to_string(), "hello-acp".to_string())]);
    let args = vec![
        "-c".to_string(),
        "printf %s \"$AGENTMUX_ENV_PROBE\" > probe.txt".to_string(),
    ];

    let mut conn = AcpConn::spawn(Path::new("/bin/sh"), &args, &env, dir.path()).unwrap();

    // The child may exit before we subscribe: the *first* `events()` call
    // returns the receiver that has been buffering since spawn, so the early
    // `AgentExited` is replayed rather than dropped.
    let mut rx = conn.events();

    // The child writes a file in its cwd using the env we injected, then
    // exits — the connection must report `AgentExited`.
    let exit = recv_until(&mut rx, Duration::from_secs(5), |k| {
        matches!(k, EventKind::AgentExited { .. })
    })
    .await;
    let exit = exit.expect("expected an AgentExited event");
    assert!(
        matches!(exit.kind, EventKind::AgentExited { code: Some(0) }),
        "expected clean exit, got {:?}",
        exit.kind
    );
    // ACP-derived events carry a placeholder seq assigned later by the
    // orchestrator.
    assert_eq!(exit.seq, 0);
    assert_eq!(exit.session_id, conn.session_id());

    // cwd + env were applied: the file exists inside the tempdir.
    let probe = std::fs::read_to_string(dir.path().join("probe.txt")).unwrap();
    assert_eq!(probe, "hello-acp");

    conn.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn initialize_times_out_when_agent_never_responds() {
    let dir = tempfile::tempdir().unwrap();
    // `sleep` never writes to stdout, so there is no way for `initialize` to
    // resolve except via the internal timeout (10s of *virtual* time, which
    // elapses instantly under the paused clock). `cat` would be wrong here:
    // it echoes the request back, and the parsed echo can resolve the pending
    // request with a protocol error instead of a timeout.
    let mut conn = AcpConn::spawn(
        Path::new("/bin/sleep"),
        &["60".to_string()],
        &BTreeMap::new(),
        dir.path(),
    )
    .unwrap();

    let err = conn.initialize().await.unwrap_err();
    assert!(
        err.to_string().contains("timed out"),
        "expected a timeout error, got: {err}"
    );

    conn.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_survive_without_consumers_and_shutdown_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let args = vec!["-c".to_string(), "sleep 0.05".to_string()];
    let mut conn =
        AcpConn::spawn(Path::new("/bin/sh"), &args, &BTreeMap::new(), dir.path()).unwrap();

    // Let the child exit before anyone subscribes; broadcast must not
    // deadlock the connection.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut rx = conn.events();
    let _ = rx.try_recv(); // lagging events may have been missed; that's fine

    conn.shutdown().await.unwrap();
    // Second shutdown is a no-op, not an error.
    conn.shutdown().await.unwrap();
}
