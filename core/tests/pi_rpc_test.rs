//! Tests for [`PiConn`], the `pi --mode rpc` connection wrapper, and for
//! the [`PiTranslator`]/[`translate_line`] record normalizer.
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
//! - `crash`/`exit42`/`reject`/`handled`/`slow`/`hang`/`tool`/`toolfail`
//!   triggers as named (whole-word matches — see fake_pi.py)
//!
//! # Event normalization
//!
//! `PiTranslator` maps pi records onto ACP-shaped `sessionUpdate`s where
//! possible (`text_delta` → `agent_message_chunk`, `tool_execution_*` →
//! `tool_call`/`tool_call_update` + `FileEdited`) and passes the rest
//! through raw; `agent_settled` detection and `response` routing run on
//! the untranslated record and are unaffected.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use agentmux_core::pi_rpc::{translate_line, PiTranslator};
use agentmux_core::{ConnTimeouts, Event, EventKind, PiConn, SpawnOptions};

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
    spawn_fake_opts(&SpawnOptions::default())
}

/// [`spawn_fake`] with explicit spawn options (timeouts, stderr log).
fn spawn_fake_opts(opts: &SpawnOptions) -> (PiConn, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let conn = PiConn::spawn(
        Path::new("python3"),
        &fake_pi_args(),
        &BTreeMap::new(),
        dir.path(),
        opts,
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
// translate_line / PiTranslator — record translation (no process needed)
// =========================================================================

/// A pi `message_update` carrying a `text_delta` normalizes onto the
/// ACP-shaped `agent_message_chunk` — every downstream consumer already
/// knows how to render/summarize that shape — while the original record
/// survives under `"pi"`.
#[test]
fn translate_text_delta_normalizes_to_agent_message_chunk() {
    let line = r#"{"type":"message_update","usage":{"input":100},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}"#;
    let events = translate_line(line);
    assert_eq!(events.len(), 1, "one event per record");
    match &events[0].kind {
        EventKind::SessionUpdate(v) => {
            assert_eq!(v["sessionUpdate"], serde_json::json!("agent_message_chunk"));
            assert_eq!(v["content"]["text"], serde_json::json!("Hello"));
            assert_eq!(
                v["pi"]["type"],
                serde_json::json!("message_update"),
                "the raw record is preserved under \"pi\""
            );
        }
        other => panic!("expected SessionUpdate, got {other:?}"),
    }
    // The orchestrator assigns real seqs; the translator stamps 0.
    assert_eq!(events[0].seq, 0);
}

/// `thinking_delta` normalizes to `agent_thought_chunk` so reasoning
/// renders in the TUI's dimmer role instead of as JSON.
#[test]
fn translate_thinking_delta_normalizes_to_thought_chunk() {
    let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm"}}"#;
    let events = translate_line(line);
    assert_eq!(events.len(), 1);
    match &events[0].kind {
        EventKind::SessionUpdate(v) => {
            assert_eq!(v["sessionUpdate"], serde_json::json!("agent_thought_chunk"));
            assert_eq!(v["content"]["text"], serde_json::json!("hmm"));
        }
        other => panic!("expected SessionUpdate, got {other:?}"),
    }
}

/// A `message_update` whose `assistantMessageEvent` is not a text
/// delta (message plumbing like `text_start`/`toolcall_end`/`done`)
/// passes through raw.
#[test]
fn translate_non_delta_message_update_passes_through() {
    let line = r#"{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}"#;
    let events = translate_line(line);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0].kind, EventKind::SessionUpdate(v)
            if v["type"] == "message_update"
                && v["assistantMessageEvent"]["type"] == "text_start"));
}

/// Lifecycle and other unmappable pi records pass through as raw
/// `SessionUpdate`s — `agent_settled` in particular must stay intact:
/// consumers and the persisted log still see the real record.
#[test]
fn translate_unmapped_events_pass_through_raw() {
    for kind in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
        "agent_settled",
        "queue_update",
        "some_future_kind",
    ] {
        let line = format!(r#"{{"type":"{kind}"}}"#);
        let events = translate_line(&line);
        assert_eq!(events.len(), 1, "{kind}");
        assert!(
            matches!(&events[0].kind, EventKind::SessionUpdate(v) if v["type"] == kind),
            "{kind} should pass through raw"
        );
    }
}

/// A `tool_execution_start` → `tool_call` SessionUpdate carrying the
/// call id, a `name path` title, `in_progress` status and the extracted
/// `locations` — the fields TUI/collab/`touched_files` already read.
#[test]
fn translate_tool_execution_start_becomes_tool_call() {
    let events = translate_line(
        r#"{"type":"tool_execution_start","toolCallId":"tc-1","toolName":"edit","args":{"path":"src/x.rs","oldText":"a","newText":"b"}}"#,
    );
    assert_eq!(events.len(), 1);
    match &events[0].kind {
        EventKind::SessionUpdate(v) => {
            assert_eq!(v["sessionUpdate"], serde_json::json!("tool_call"));
            assert_eq!(v["toolCallId"], serde_json::json!("tc-1"));
            assert_eq!(v["status"], serde_json::json!("in_progress"));
            assert_eq!(v["title"], serde_json::json!("edit src/x.rs"));
            assert_eq!(v["locations"][0]["path"], serde_json::json!("src/x.rs"));
            assert_eq!(v["pi"]["type"], serde_json::json!("tool_execution_start"));
        }
        other => panic!("expected SessionUpdate, got {other:?}"),
    }
}

/// The full `tool_execution_*` lifecycle for a file-editing tool:
/// start → `tool_call`, update → `tool_call_update`, end →
/// `tool_call_update` *plus* a `FileEdited` for the tracked path. The
/// `end` record carries no `args` — the path comes from the translator's
/// toolCallId bookkeeping, so one [`PiTranslator`] must span the run.
#[test]
fn translator_maps_completed_edit_to_file_edited() {
    let mut t = PiTranslator::new();
    let start = t.translate_line(
        r#"{"type":"tool_execution_start","toolCallId":"tc-9","toolName":"edit","args":{"path":"src/edited.rs"}}"#,
    );
    assert_eq!(start.len(), 1);

    let end = t.translate_line(
        r#"{"type":"tool_execution_end","toolCallId":"tc-9","toolName":"edit","result":{"content":[{"type":"text","text":"ok"}]},"isError":false}"#,
    );
    assert_eq!(
        end.len(),
        2,
        "a completed file edit yields the update AND FileEdited: {end:?}"
    );
    match &end[0].kind {
        EventKind::SessionUpdate(v) => {
            assert_eq!(v["sessionUpdate"], serde_json::json!("tool_call_update"));
            assert_eq!(v["status"], serde_json::json!("completed"));
            assert_eq!(v["toolCallId"], serde_json::json!("tc-9"));
        }
        other => panic!("expected SessionUpdate, got {other:?}"),
    }
    assert!(
        matches!(&end[1].kind, EventKind::FileEdited { path } if path.as_os_str() == "src/edited.rs"),
        "expected FileEdited for the tracked path: {:?}",
        end[1].kind
    );
}

/// A failed or non-editing tool end must NOT emit `FileEdited`.
#[test]
fn translator_skips_file_edited_for_failed_and_readonly_tools() {
    // Failed edit.
    let mut t = PiTranslator::new();
    t.translate_line(
        r#"{"type":"tool_execution_start","toolCallId":"a","toolName":"edit","args":{"path":"x.rs"}}"#,
    );
    let end = t.translate_line(
        r#"{"type":"tool_execution_end","toolCallId":"a","toolName":"edit","isError":true}"#,
    );
    assert!(
        end.iter()
            .all(|e| !matches!(e.kind, EventKind::FileEdited { .. })),
        "a failed edit is not a FileEdited: {end:?}"
    );
    assert!(matches!(&end[0].kind, EventKind::SessionUpdate(v) if v["status"] == "failed"));

    // `read` has a path arg but doesn't edit.
    let mut t = PiTranslator::new();
    t.translate_line(
        r#"{"type":"tool_execution_start","toolCallId":"b","toolName":"read","args":{"path":"x.rs"}}"#,
    );
    let end = t.translate_line(
        r#"{"type":"tool_execution_end","toolCallId":"b","toolName":"read","isError":false}"#,
    );
    assert!(
        end.iter()
            .all(|e| !matches!(e.kind, EventKind::FileEdited { .. })),
        "a read is not a FileEdited: {end:?}"
    );
}

/// A command response (`type:"response"` + `id`) is routed to the
/// pending-command map by the reader — it must never reach the event bus.
#[test]
fn translate_response_line_returns_nothing() {
    let line = r#"{"id":"agentmux-3","type":"response","command":"get_state","success":true,"data":{"sessionId":"pi-session-1"}}"#;
    assert!(
        translate_line(line).is_empty(),
        "response records are not bus events"
    );
    // An error response is still a response, not an event.
    let err_line = r#"{"id":"agentmux-4","type":"response","command":"prompt","success":false,"error":"nope"}"#;
    assert!(translate_line(err_line).is_empty());
}

/// The documented framing trap: U+2028/U+2029 may appear raw inside JSON
/// string payloads and must NOT be treated as record boundaries. One line
/// in → one event out, payload intact. (A `delta` outside
/// `assistantMessageEvent` isn't a known delta shape → passthrough.)
#[test]
fn translate_line_with_u2028_u2029_in_payload_does_not_split() {
    // Raw U+2028 and U+2029 characters inside the JSON string — exactly
    // what a generic line reader would wrongly split on.
    let line = "{\"type\":\"message_update\",\"delta\":\"a\u{2028}b\u{2029}c\"}";
    let events = translate_line(line);
    assert_eq!(events.len(), 1, "the record must translate unsplit");
    match &events[0].kind {
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
    let events = translate_line(line);
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0].kind, EventKind::SessionUpdate(v) if v["type"] == "bash_execution_update")
    );
}

/// Blank, non-JSON, and non-object lines produce no event.
#[test]
fn translate_garbage_lines_return_nothing() {
    assert!(translate_line("").is_empty());
    assert!(translate_line("   ").is_empty());
    assert!(translate_line("not json at all").is_empty());
    assert!(translate_line("[1,2,3]").is_empty());
    assert!(translate_line("\"just a string\"").is_empty());
    assert!(translate_line("42").is_empty());
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
        &SpawnOptions::default(),
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

    // Events arrive in send order; text deltas are normalized onto the
    // ACP `sessionUpdate` vocabulary, everything else passes through
    // with its pi `type` intact.
    let types: Vec<&str> = kinds
        .iter()
        .map(|v| {
            v["type"]
                .as_str()
                .or_else(|| v["sessionUpdate"].as_str())
                .unwrap_or("?")
        })
        .collect();
    assert_eq!(
        types,
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "agent_message_chunk",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ],
        "pi events should stream in run order"
    );
    // The stub echoes the prompt text back; the normalized chunk carries
    // it under `content.text` and preserves the raw record under `pi`.
    assert_eq!(
        kinds[3]["content"]["text"],
        serde_json::json!("fake pi reply: hello pi")
    );
    assert_eq!(
        kinds[3]["pi"]["assistantMessageEvent"]["delta"],
        serde_json::json!("fake pi reply: hello pi")
    );

    conn.shutdown().await.unwrap();
}

/// A `tool` prompt exercises the `tool_execution_*` → `tool_call` /
/// `FileEdited` translation end to end: the stub emits an `edit` tool
/// lifecycle, so the bus must yield a normalized `tool_call` update and
/// a `FileEdited` for `src/edited.rs` — and `prompt` still settles.
#[tokio::test]
async fn prompt_with_tool_edit_emits_file_edited_and_settles() {
    let (mut conn, dir) = spawn_fake();
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    conn.prompt(&session_id, "use a tool".to_string())
        .await
        .expect("tool prompt should settle");

    let events = recv_until(
        &mut rx,
        EVENT_TIMEOUT,
        |k| matches!(k, EventKind::SessionUpdate(v) if v["type"] == "agent_settled"),
    )
    .await;

    let tool_call = events.iter().any(|e| {
        matches!(&e.kind, EventKind::SessionUpdate(v)
            if v["sessionUpdate"] == "tool_call"
                && v["toolCallId"] == "toolcall-1"
                && v["locations"][0]["path"] == "src/edited.rs")
    });
    assert!(tool_call, "tool_execution_start → tool_call: {events:?}");

    let edited = events.iter().find_map(|e| match &e.kind {
        EventKind::FileEdited { path } => Some(path.clone()),
        _ => None,
    });
    assert_eq!(
        edited,
        Some(std::path::PathBuf::from("src/edited.rs")),
        "the completed edit must emit FileEdited: {events:?}"
    );

    conn.shutdown().await.unwrap();
}

/// `toolfail` runs the same tool lifecycle but with `isError:true` —
/// a failed edit must not claim a file was edited.
#[tokio::test]
async fn failed_tool_edit_does_not_emit_file_edited() {
    let (mut conn, dir) = spawn_fake();
    let mut rx = conn.events();

    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();
    conn.prompt(&session_id, "do a toolfail".to_string())
        .await
        .expect("toolfail prompt should settle");

    let events = recv_until(
        &mut rx,
        EVENT_TIMEOUT,
        |k| matches!(k, EventKind::SessionUpdate(v) if v["type"] == "agent_settled"),
    )
    .await;

    assert!(
        events
            .iter()
            .all(|e| !matches!(e.kind, EventKind::FileEdited { .. })),
        "a failed edit emits no FileEdited: {events:?}"
    );
    let failed_update = events.iter().any(|e| {
        matches!(&e.kind, EventKind::SessionUpdate(v)
            if v["sessionUpdate"] == "tool_call_update" && v["status"] == "failed")
    });
    assert!(failed_update, "the tool end surfaces as failed: {events:?}");

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
            EventKind::SessionUpdate(v) if v["sessionUpdate"] == "agent_message_chunk" => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 1, "the U+2028 record must arrive unsplit");
    assert_eq!(
        deltas[0]["content"]["text"],
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
        &SpawnOptions::default(),
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
    let mut conn = PiConn::spawn(
        Path::new("/bin/sh"),
        &args,
        &BTreeMap::new(),
        dir.path(),
        &SpawnOptions::default(),
    )
    .unwrap();

    // Let the child exit before anyone subscribes; broadcast must not
    // deadlock the connection.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut rx = conn.events();
    let _ = rx.try_recv(); // lagging events may have been missed; that's fine

    conn.shutdown().await.unwrap();
    // Second shutdown is a no-op, not an error.
    conn.shutdown().await.unwrap();
}

/// The configured `prompt` bound: `hang` leaves the request pending on
/// the worker forever; under the paused clock the 30 s virtual deadline
/// fires the instant the caller parks on the reply (external replies
/// always lose to auto-advance — this pins the timeout path itself).
/// An `Orchestrator` "timed out" note lands on the bus, and `cancel`
/// (its own code path, no timeout) still resolves.
#[tokio::test(start_paused = true)]
async fn prompt_times_out_on_the_configured_bound_virtual() {
    let opts = SpawnOptions {
        timeouts: ConnTimeouts {
            init: Duration::from_secs(10),
            prompt: Duration::from_secs(30),
        },
        ..SpawnOptions::default()
    };
    let (mut conn, _dir) = spawn_fake_opts(&opts);
    let mut rx = conn.events();

    let err = conn
        .prompt("pi-session-1", "hang".to_string())
        .await
        .expect_err("a wedged turn must error at the prompt bound");
    assert!(
        err.to_string().contains("timed out"),
        "expected a timeout error, got: {err}"
    );

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

    conn.cancel("pi-session-1")
        .await
        .expect("cancel must resolve immediately");
    conn.shutdown().await.unwrap();
}

/// Real-clock counterpart: `prompt_timeout = 1s` frees the caller from
/// the stub's `hang` (~1 s, not forever), and a healthy turn under the
/// same conn resolves well inside the bound.
#[tokio::test]
async fn prompt_timeout_frees_the_caller_and_cancel_still_works() {
    let opts = SpawnOptions {
        timeouts: ConnTimeouts {
            init: Duration::from_secs(10),
            prompt: Duration::from_secs(1),
        },
        ..SpawnOptions::default()
    };
    let (mut conn, dir) = spawn_fake_opts(&opts);
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(dir.path()).await.unwrap();

    // Healthy turn first: the bound must not bite a fast agent.
    conn.prompt(&session_id, "healthy".to_string())
        .await
        .expect("a healthy prompt resolves inside the bound");

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

    conn.cancel(&session_id)
        .await
        .expect("abort should resolve");
    conn.shutdown().await.unwrap();
}

/// Agent stderr is piped, drained into the configured log file, and the
/// retained tail surfaces twice: appended to the failed prompt's error
/// and as an `Orchestrator` note emitted before `AgentExited`. fake_pi's
/// `noisy` trigger writes 15 lines then exits 3 — one more than the
/// 12-line tail, so truncation is exercised too.
#[tokio::test]
async fn stderr_is_logged_and_tail_surfaces_on_crash() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("agent.stderr.log");
    let opts = SpawnOptions {
        stderr_log: Some(log_path.clone()),
        ..SpawnOptions::default()
    };
    let (mut conn, work) = spawn_fake_opts(&opts);
    conn.initialize().await.unwrap();
    let session_id = conn.new_session(work.path()).await.unwrap();
    let mut rx = conn.events();

    let err = conn
        .prompt(&session_id, "noisy".to_string())
        .await
        .expect_err("a crashing prompt must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("fake-pi stderr line 15"),
        "error should carry the stderr tail, got: {msg}"
    );
    assert!(
        !msg.contains("fake-pi stderr line 3"),
        "tail should keep only the last ~12 lines, got: {msg}"
    );

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
    assert!(note.contains("fake-pi stderr line 15"), "{note}");
    assert!(!note.contains("fake-pi stderr line 3"), "{note}");
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
    assert!(content.contains("fake-pi stderr line 1\n"), "{content:?}");
    assert!(content.contains("fake-pi stderr line 15\n"), "{content:?}");

    conn.shutdown().await.unwrap();
}

/// A child that dies during the `get_state` handshake fails initialize
/// with its last stderr line on the error.
#[tokio::test]
async fn init_failure_error_carries_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let conn = PiConn::spawn(
        Path::new("sh"),
        &[
            "-c".to_string(),
            "echo 'pi init boom on stderr' >&2; exit 7".to_string(),
        ],
        &BTreeMap::new(),
        dir.path(),
        &SpawnOptions::default(),
    )
    .expect("sh should spawn");

    let err = conn
        .initialize()
        .await
        .expect_err("a dead stub must fail initialize");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("pi init boom on stderr"),
        "init failure should carry the stderr tail, got: {msg}"
    );
}

#[tokio::test]
async fn pi_control_requests_query_and_change_existing_adapter() {
    let (mut conn, _dir) = spawn_fake();
    conn.initialize().await.unwrap();
    let commands = conn
        .request(serde_json::json!({"type":"get_commands"}))
        .await
        .unwrap();
    assert_eq!(commands["commands"][0]["name"], "fix-tests");
    conn.request(serde_json::json!({"type":"set_model", "provider":"fake", "modelId":"large"}))
        .await
        .unwrap();
    conn.request(serde_json::json!({"type":"set_thinking_level", "level":"high"}))
        .await
        .unwrap();
    let state = conn
        .request(serde_json::json!({"type":"get_state"}))
        .await
        .unwrap();
    assert_eq!(state["model"]["id"], "large");
    assert_eq!(state["thinkingLevel"], "high");
    assert!(conn
        .request(serde_json::json!({"type":"set_thinking_level", "level":"invalid"}))
        .await
        .is_err());
    conn.shutdown().await.unwrap();
}
