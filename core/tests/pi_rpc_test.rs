//! Tests for [`PiConn`], the `pi --mode rpc` connection wrapper, and for
//! the pure [`translate_line`] record translator.
//!
//! # Fake pi
//!
//! `core/tests/fake_pi.py` is a tiny Python stub speaking pi's JSONL RPC
//! protocol — no real `pi` binary (or API key) required. See that file for
//! the full behaviour contract:
//!
//! - `get_state` → `data.sessionId == "pi-session-1"`
//! - `new_session` → `data.cancelled == false`
//! - `prompt` → `disposition:"started"` response, then an event run ending
//!   in `agent_settled`; the `text_delta` echoes the prompt text raw, so
//!   U+2028/U+2029 bytes come back verbatim (the documented framing trap)
//! - `crash`/`exit42`/`reject`/`handled`/`slow`/`hang` triggers as named

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use agentmux_core::pi_rpc::translate_line;
use agentmux_core::{Event, EventKind, PiConn};

/// Generous deadline for event collection; the stub replies instantly, so
/// this only bites on regression.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

fn fake_pi_args() -> Vec<String> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_pi.py");
    assert!(
        script.is_file(),
        "fake pi stub missing at {}",
        script.display()
    );
    vec![script.to_string_lossy().into_owned()]
}

/// Spawn a connection to the fake pi stub; the tempdir is the child's cwd.
fn spawn_fake() -> (PiConn, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let conn = PiConn::spawn(
        Path::new("python3"),
        &fake_pi_args(),
        &BTreeMap::new(),
        dir.path(),
    )
    .expect("fake pi should spawn");
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

// =========================================================================
// translate_line — the pure record translator (no process needed)
// =========================================================================

/// A pi event record (any JSON object that is not a command response)
/// becomes a [`EventKind::SessionUpdate`] carrying the raw JSON.
#[test]
fn translate_event_line_becomes_session_update() {
    let line = r#"{"type":"message_update","usage":{"input":100},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}"#;
    let event = translate_line(line).expect("event line should translate");
    match event.kind {
        EventKind::SessionUpdate(v) => {
            assert_eq!(v["type"], serde_json::json!("message_update"));
            assert_eq!(
                v["assistantMessageEvent"]["delta"],
                serde_json::json!("Hello")
            );
        }
        other => panic!("expected SessionUpdate, got {other:?}"),
    }
    // The orchestrator assigns real seqs; the translator stamps 0.
    assert_eq!(event.seq, 0);
}

/// A command response (`type:"response"` + `id`) is routed to the
/// pending-command map by the reader — it must never reach the event bus.
#[test]
fn translate_response_line_returns_none() {
    let line = r#"{"id":"agentmux-3","type":"response","command":"get_state","success":true,"data":{"sessionId":"pi-session-1"}}"#;
    assert!(
        translate_line(line).is_none(),
        "response records are not bus events"
    );
    // An error response is still a response, not an event.
    let err_line = r#"{"id":"agentmux-4","type":"response","command":"prompt","success":false,"error":"nope"}"#;
    assert!(translate_line(err_line).is_none());
}

/// The documented framing trap: U+2028/U+2029 may appear raw inside JSON
/// string payloads and must NOT be treated as record boundaries. One line
/// in → one event out, payload intact.
#[test]
fn translate_line_with_u2028_u2029_in_payload_does_not_split() {
    // Raw U+2028 and U+2029 characters inside the JSON string — exactly
    // what a generic line reader would wrongly split on.
    let line = "{\"type\":\"message_update\",\"delta\":\"a\u{2028}b\u{2029}c\"}";
    let event = translate_line(line).expect("line should translate as one record");
    match event.kind {
        EventKind::SessionUpdate(v) => {
            assert_eq!(v["delta"], serde_json::json!("a\u{2028}b\u{2029}c"));
        }
        other => panic!("expected SessionUpdate, got {other:?}"),
    }
}

/// `bash_execution_update` is the one event family that *does* carry an
/// `id` — it must still be classified as an event, not a response.
#[test]
fn translate_bash_execution_update_with_id_is_an_event() {
    let line = r#"{"type":"bash_execution_update","id":"agentmux-9","delta":"total 48\n"}"#;
    let event = translate_line(line).expect("bash update is an event");
    assert!(
        matches!(event.kind, EventKind::SessionUpdate(v) if v["type"] == "bash_execution_update")
    );
}

/// Blank, non-JSON, and non-object lines produce no event.
#[test]
fn translate_garbage_lines_return_none() {
    assert!(translate_line("").is_none());
    assert!(translate_line("   ").is_none());
    assert!(translate_line("not json at all").is_none());
    assert!(translate_line("[1,2,3]").is_none());
    assert!(translate_line("\"just a string\"").is_none());
    assert!(translate_line("42").is_none());
}

// =========================================================================
// PiConn process management + protocol flow against the fake stub
// =========================================================================

#[test]
fn spawn_fails_for_missing_command() {
    let dir = tempfile::tempdir().unwrap();
    let result = PiConn::spawn(
        Path::new("/nonexistent/agentmux-pi"),
        &[],
        &BTreeMap::new(),
        dir.path(),
    );
    assert!(result.is_err());
}

/// pi has no initialize handshake; [`PiConn::initialize`] issues a
/// `get_state` probe proving the subprocess speaks the protocol, and
/// resolves to the session state JSON.
#[tokio::test]
async fn initialize_get_state_handshake_returns_session_state() {
    let (mut conn, _dir) = spawn_fake();

    let state = conn.initialize().await.expect("initialize should succeed");
    assert_eq!(
        state["sessionId"],
        serde_json::json!("pi-session-1"),
        "unexpected get_state data: {state}"
    );
    assert_eq!(state["sessionName"], serde_json::json!("fake-pi"));

    conn.shutdown().await.unwrap();
}

/// `new_session` sends pi's `new_session` command, then returns the pi
/// session id reported by `get_state`.
#[tokio::test]
async fn new_session_returns_pi_session_id() {
    let (mut conn, dir) = spawn_fake();

    conn.initialize().await.unwrap();
    let session_id = conn
        .new_session(dir.path())
        .await
        .expect("new_session should succeed");
    assert_eq!(session_id, "pi-session-1");

    conn.shutdown().await.unwrap();
}

/// Full prompt turn: the response's `disposition:"started"` only means
/// *accepted* — `prompt` resolves when the run's `agent_settled` event
/// arrives, matching ACP's "prompt resolves when the turn ends" shape.
#[tokio::test]
async fn prompt_streams_events_and_resolves_on_agent_settled() {
    let (mut conn, dir) = spawn_fake();
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    conn.prompt(&session_id, "hello pi".to_string())
        .await
        .expect("prompt should resolve once the run settles");

    let events = recv_until(
        &mut rx,
        EVENT_TIMEOUT,
        |k| matches!(k, EventKind::SessionUpdate(v) if v["type"] == "agent_settled"),
    )
    .await;
    let kinds: Vec<&serde_json::Value> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SessionUpdate(v) => Some(v),
            _ => None,
        })
        .collect();

    // Every pi event arrives as an opaque SessionUpdate in send order.
    let types: Vec<&str> = kinds.iter().filter_map(|v| v["type"].as_str()).collect();
    assert_eq!(
        types,
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ],
        "pi events should stream in run order"
    );
    // The stub echoes the prompt text back in the text delta.
    assert_eq!(
        kinds[3]["assistantMessageEvent"]["delta"],
        serde_json::json!("fake pi reply: hello pi")
    );

    conn.shutdown().await.unwrap();
}

/// End-to-end framing coverage: a prompt containing a raw U+2028 is echoed
/// back raw by the stub. A reader splitting on U+2028/U+2029 would break
/// the record into unparseable fragments; the LF-only reader must deliver
/// one intact `SessionUpdate`.
#[tokio::test]
async fn u2028_in_payload_is_not_split() {
    let (mut conn, dir) = spawn_fake();
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    conn.prompt(&session_id, "line\u{2028}break".to_string())
        .await
        .expect("prompt should settle");

    let events = recv_until(
        &mut rx,
        EVENT_TIMEOUT,
        |k| matches!(k, EventKind::SessionUpdate(v) if v["type"] == "agent_settled"),
    )
    .await;
    let deltas: Vec<&serde_json::Value> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SessionUpdate(v) if v["type"] == "message_update" => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 1, "the U+2028 record must arrive unsplit");
    assert_eq!(
        deltas[0]["assistantMessageEvent"]["delta"],
        serde_json::json!("fake pi reply: line\u{2028}break"),
        "payload must be intact across U+2028"
    );

    conn.shutdown().await.unwrap();
}

/// Regression: a turn emitting more records than the 256-cap broadcast
/// ring can hold must not hang `prompt`. Settle detection rides a
/// dedicated signal, not the lossy bus — even if a consumer-facing
/// receiver lags, `prompt` resolves once `agent_settled` is read.
#[tokio::test]
async fn prompt_resolves_after_high_event_count_turn() {
    let (mut conn, dir) = spawn_fake();
    // A consumer that subscribes but never drains: its receiver lags over
    // the whole burst. The prompt's own wait must not depend on that ring.
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    tokio::time::timeout(EVENT_TIMEOUT, conn.prompt(&session_id, "burst".to_string()))
        .await
        .expect("prompt hung: settle was lost with the broadcast overflow")
        .expect("burst prompt should settle successfully");

    // The lagging consumer receiver skipped events — expected — but the
    // retained tail (which includes the final agent_settled) still shows
    // the burst happened.
    let mut saw = 0usize;
    loop {
        match rx.try_recv() {
            Ok(ev) => {
                if matches!(ev.kind, EventKind::SessionUpdate(_)) {
                    saw += 1;
                }
            }
            // Skip the lag marker and continue into the retained tail.
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            _ => break,
        }
    }
    assert!(
        saw > 0,
        "consumer receiver should retain the tail of the burst"
    );

    conn.shutdown().await.unwrap();
}

/// `crash` in the prompt kills the stub mid-turn: the pending prompt
/// resolves with an error and the bus reports `AgentExited`.
#[tokio::test]
async fn crash_prompt_exits_agent_and_fails_prompt() {
    let (mut conn, dir) = spawn_fake();
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    let result = conn.prompt(&session_id, "please crash".to_string()).await;
    assert!(result.is_err(), "a crashed agent must fail the prompt");

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
    assert_eq!(exit.session_id, conn.session_id());

    conn.shutdown().await.unwrap();
}

/// A `success:false` response surfaces its `error` string as the command's
/// Err — it is not silently dropped or misrouted as an event.
#[tokio::test]
async fn rejected_prompt_returns_the_error_string() {
    let (mut conn, dir) = spawn_fake();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    let err = conn
        .prompt(&session_id, "reject me".to_string())
        .await
        .expect_err("rejected prompt must fail");
    assert!(
        err.to_string().contains("rejected"),
        "expected the response's error text, got: {err}"
    );

    conn.shutdown().await.unwrap();
}

/// `disposition:"handled"` means an extension consumed the prompt and no
/// run starts — waiting for `agent_settled` would hang, so `prompt` must
/// resolve on the response alone.
#[tokio::test]
async fn handled_disposition_resolves_without_a_run() {
    let (mut conn, dir) = spawn_fake();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        conn.prompt(&session_id, "handled by extension".to_string()),
    )
    .await
    .expect("handled prompt must resolve promptly")
    .expect("handled prompt should succeed");

    conn.shutdown().await.unwrap();
}

/// `slow` leaves a run in flight until `abort` arrives; `cancel` must
/// unblock the connection and leave it usable for the next turn.
#[tokio::test]
async fn cancel_aborts_in_flight_run_and_connection_survives() {
    let (mut conn, dir) = spawn_fake();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    {
        let prompt = conn.prompt(&session_id, "slow".to_string());
        tokio::pin!(prompt);
        let elapsed = tokio::time::timeout(Duration::from_secs(2), &mut prompt).await;
        assert!(elapsed.is_err(), "the slow run must still be pending");
        // Dropping the future releases the &mut borrow; the request stays
        // pending on the worker, which is the point.
    }

    // pi's `abort` waits for the session to go idle, then responds.
    conn.cancel(&session_id)
        .await
        .expect("abort should succeed");

    // The connection is healthy for the next turn.
    conn.prompt(&session_id, "after abort".to_string())
        .await
        .expect("prompt after abort should complete");

    conn.shutdown().await.unwrap();
}

/// A prompt that is never answered keeps the request pending without
/// wedging the connection: `shutdown` still works.
#[tokio::test]
async fn hang_prompt_never_resolves_but_shutdown_still_works() {
    let (mut conn, dir) = spawn_fake();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    {
        let prompt = conn.prompt(&session_id, "hang".to_string());
        tokio::pin!(prompt);
        let elapsed = tokio::time::timeout(Duration::from_secs(2), &mut prompt).await;
        assert!(elapsed.is_err(), "a hanging prompt must not resolve");
    }

    conn.shutdown().await.unwrap();
}

/// `initialize` is bounded by the internal 10s timeout when the agent
/// never speaks (virtual time elapses instantly under the paused clock).
#[tokio::test(start_paused = true)]
async fn initialize_times_out_when_agent_never_responds() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = PiConn::spawn(
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

/// Events emitted before anyone subscribes are replayed to the first
/// `events()` receiver; shutdown is idempotent.
#[tokio::test]
async fn events_survive_without_consumers_and_shutdown_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let args = vec!["-c".to_string(), "sleep 0.05".to_string()];
    let mut conn =
        PiConn::spawn(Path::new("/bin/sh"), &args, &BTreeMap::new(), dir.path()).unwrap();

    // Let the child exit before anyone subscribes; broadcast must not
    // deadlock the connection.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut rx = conn.events();
    let _ = rx.try_recv(); // lagging events may have been missed; that's fine

    conn.shutdown().await.unwrap();
    // Second shutdown is a no-op, not an error.
    conn.shutdown().await.unwrap();
}
