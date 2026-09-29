//! Pi-native event record shapes — the `{"type": "<kind>", ...}` objects
//! a `pi --mode rpc` process streams (docs: pi `rpc.md` / `json.md`).
//!
//! [`PiConn`](crate::PiConn)'s [`PiTranslator`](crate::pi_rpc::PiTranslator)
//! normalizes what maps cleanly onto the ACP-shaped `sessionUpdate`
//! vocabulary (`text_delta` → `agent_message_chunk`,
//! `tool_execution_*` → `tool_call`/`tool_call_update`, completed file
//! edits → [`EventKind::FileEdited`](crate::EventKind::FileEdited)) and
//! passes everything else through as an opaque
//! [`EventKind::SessionUpdate`](crate::EventKind::SessionUpdate) holding
//! the raw record. These helpers give every downstream consumer —
//! [`collab::summarize_event`](crate::collab::summarize_event), the
//! orchestrator's relay `describe_event`, and the TUI's
//! renderers/`touched_files` — one shared place to read the pi shapes,
//! so a record that survived translation unnormalized (or predates it in
//! a persisted log) still yields text, paths and summaries instead of a
//! JSON dump.
//!
//! All helpers are pure digs into `serde_json::Value`; none allocate or
//! panic on unexpected shapes — pi reserves the right to add fields.

use serde_json::Value;

/// The pi event kind — `{"type": "<kind>"}` — or `None` for values that
/// aren't pi records. An ACP-shaped update (one carrying `sessionUpdate`)
/// is deliberately excluded: normalized pi events keep their provenance
/// under a `"pi"` key, and this dig must not reclassify them as raw.
pub fn kind(value: &Value) -> Option<&str> {
    if value.get("sessionUpdate").is_some() {
        return None;
    }
    value.get("type").and_then(|t| t.as_str())
}

/// A streaming text delta out of a `message_update` record.
///
/// pi carries `{"type":"message_update","assistantMessageEvent":
/// {"type":"text_delta"|"thinking_delta","delta": "…"}}`; `text_delta`
/// is the visible reply, `thinking_delta` the reasoning stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    /// `text_delta` — assistant reply text.
    Message,
    /// `thinking_delta` — reasoning text (the "thought" role).
    Thought,
}

/// Extract a `message_update`'s text/thinking delta, if it carries one.
pub fn delta(value: &Value) -> Option<(Delta, &str)> {
    if kind(value) != Some("message_update") {
        return None;
    }
    let ev = value.get("assistantMessageEvent")?;
    let text = ev.get("delta").and_then(|d| d.as_str())?;
    match ev.get("type").and_then(|t| t.as_str()) {
        Some("text_delta") => Some((Delta::Message, text)),
        Some("thinking_delta") => Some((Delta::Thought, text)),
        _ => None,
    }
}

/// The assistant-visible text of a `message_start`/`message_end` (or
/// `turn_end`) record: `message.content[]` items with `type:"text"`
/// joined in order. `message_update` is delta-only and yields `None` —
/// use [`delta`] for it.
pub fn message_text(value: &Value) -> Option<String> {
    match kind(value) {
        Some("message_start" | "message_end") => {}
        _ => return None,
    }
    let content = value.pointer("/message/content")?.as_array()?;
    let text: String = content
        .iter()
        .filter(|c| c.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
        .collect();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// `toolName` of a `tool_execution_*` record.
pub fn tool_name(value: &Value) -> Option<&str> {
    matches!(kind(value), Some(k) if k.starts_with("tool_execution_"))
        .then(|| value.get("toolName").and_then(|t| t.as_str()))
        .flatten()
}

/// `toolCallId` of a `tool_execution_*` record.
pub fn tool_call_id(value: &Value) -> Option<&str> {
    matches!(kind(value), Some(k) if k.starts_with("tool_execution_"))
        .then(|| value.get("toolCallId").and_then(|t| t.as_str()))
        .flatten()
}

/// The file path a `tool_execution_*` record operates on, dug out of
/// `args`: pi's file tools take `path`, but agents/extensions emit
/// `file_path`, `filePath`, `filename` and `file` too — the first string
/// wins. A top-level `path` is accepted as a last resort.
pub fn tool_path(value: &Value) -> Option<&str> {
    if !matches!(kind(value), Some(k) if k.starts_with("tool_execution_")) {
        return None;
    }
    let args = value.get("args")?;
    for key in ["path", "file_path", "filePath", "filename", "file"] {
        if let Some(p) = args.get(key).and_then(|p| p.as_str()) {
            if !p.is_empty() {
                return Some(p);
            }
        }
    }
    None
}

/// Whether a tool *name* looks like it mutates files — pi's own `edit`
/// and `write`, plus the usual extension aliases. Name-based heuristic:
/// `bash` edits are invisible to it, which is fine — a missed edit is
/// better than claiming a `read` rewrote the file.
pub fn tool_edits_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const EDITISH: [&str; 7] = [
        "edit", "write", "patch", "create", "delete", "move", "rename",
    ];
    EDITISH.iter().any(|k| name.contains(k))
}

/// Whether a `tool_execution_end` reports failure (`isError: true`).
pub fn tool_end_failed(value: &Value) -> bool {
    kind(value) == Some("tool_execution_end")
        && value.get("isError").and_then(|e| e.as_bool()) == Some(true)
}

/// Kinds that carry no consumer-meaningful payload — turn/message
/// scaffolding a summary should skip entirely rather than tag.
const SCAFFOLDING: [&str; 4] = ["turn_start", "turn_end", "message_start", "agent_start"];

/// Reduce one raw pi record to a one-line human summary, or `None` for
/// pure scaffolding (`turn_start`, `message_start`, delta-less
/// `message_update`, …) that a relay/context block should skip rather
/// than spell out.
///
/// This is the shared fallback for records that reached a consumer
/// unnormalized; the translator handles the common cases upstream, so
/// this mostly sees lifecycle records — and provides the defensive path
/// for persisted pre-normalization logs.
pub fn summary(value: &Value) -> Option<String> {
    let kind = kind(value)?;
    // Streaming text wins over the kind label wherever it appears.
    if let Some((_, text)) = delta(value) {
        return Some(format!("message: {text}"));
    }
    if let Some(text) = message_text(value) {
        return Some(format!("message: {text}"));
    }
    if let Some(name) = tool_name(value) {
        let path = tool_path(value);
        return Some(match kind {
            "tool_execution_start" => match path {
                Some(p) => format!("tool {name} started ({p})"),
                None => format!("tool {name} started"),
            },
            "tool_execution_end" if tool_end_failed(value) => {
                format!("tool {name} failed")
            }
            "tool_execution_end" => format!("tool {name} finished"),
            _ => format!("tool {name} update"),
        });
    }
    match kind {
        k if SCAFFOLDING.contains(&k) => None,
        "message_update" => None, // non-delta assistant events are plumbing
        "message_end" => None,    // text-less end marker
        "agent_end" => Some("agent run finished".into()),
        "agent_settled" => Some("run settled".into()),
        "bash_execution_update" => value
            .get("delta")
            .and_then(|d| d.as_str())
            .map(|d| format!("bash output: {}", d.lines().next().unwrap_or(""))),
        "extension_ui_request" => Some("extension requested UI".into()),
        "extension_error" => Some(
            value
                .get("error")
                .and_then(|e| e.as_str())
                .map(|e| format!("extension error: {e}"))
                .unwrap_or_else(|| "extension error".into()),
        ),
        "queue_update" => Some("steering queue updated".into()),
        "auto_retry_start" => Some("auto-retry started".into()),
        "auto_retry_end" => Some("auto-retry ended".into()),
        "compaction_start" | "auto_compaction_start" => Some("compaction started".into()),
        "compaction_end" | "auto_compaction_end" => Some("compaction finished".into()),
        other => Some(format!("pi event: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kind_excludes_acp_shaped_updates() {
        assert_eq!(kind(&json!({"type": "agent_start"})), Some("agent_start"));
        assert_eq!(kind(&json!({"sessionUpdate": "agent_message_chunk"})), None);
        assert_eq!(kind(&json!({})), None);
        assert_eq!(kind(&json!("nope")), None);
    }

    #[test]
    fn delta_reads_text_and_thinking() {
        let text = json!({"type":"message_update","assistantMessageEvent":
            {"type":"text_delta","contentIndex":0,"delta":"hello"}});
        assert_eq!(delta(&text), Some((Delta::Message, "hello")));
        let think = json!({"type":"message_update","assistantMessageEvent":
            {"type":"thinking_delta","contentIndex":0,"delta":"hmm"}});
        assert_eq!(delta(&think), Some((Delta::Thought, "hmm")));
        // Scaffolding subtypes and other records yield nothing.
        let start = json!({"type":"message_update","assistantMessageEvent":
            {"type":"text_start","contentIndex":0}});
        assert_eq!(delta(&start), None);
        assert_eq!(delta(&json!({"type":"agent_start"})), None);
    }

    #[test]
    fn message_text_joins_content_blocks() {
        let end = json!({"type":"message_end","message":{"role":"assistant",
            "content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}});
        assert_eq!(message_text(&end).as_deref(), Some("ab"));
        let empty = json!({"type":"message_end","message":{"role":"assistant","content":[]}});
        assert_eq!(message_text(&empty), None);
        assert_eq!(message_text(&json!({"type":"agent_start"})), None);
    }

    #[test]
    fn tool_fields_dig_args_and_names() {
        let v = json!({"type":"tool_execution_start","toolCallId":"tc-1",
            "toolName":"edit","args":{"path":"src/x.rs","oldText":"a","newText":"b"}});
        assert_eq!(tool_name(&v), Some("edit"));
        assert_eq!(tool_call_id(&v), Some("tc-1"));
        assert_eq!(tool_path(&v), Some("src/x.rs"));
        let alt = json!({"type":"tool_execution_update","toolCallId":"t",
            "toolName":"write","args":{"file_path":"/abs/y.md"}});
        assert_eq!(tool_path(&alt), Some("/abs/y.md"));
        let noargs = json!({"type":"tool_execution_end","toolCallId":"t",
            "toolName":"edit","isError":false});
        assert_eq!(tool_path(&noargs), None, "end carries no args");
        assert!(tool_edits_file("edit"));
        assert!(tool_edits_file("write_file"));
        assert!(!tool_edits_file("read"));
        assert!(!tool_edits_file("bash"));
        assert!(tool_end_failed(
            &json!({"type":"tool_execution_end","isError":true})
        ));
        assert!(!tool_end_failed(&noargs));
    }

    #[test]
    fn summary_covers_lifecycle_and_skips_scaffolding() {
        let delta_v = json!({"type":"message_update","assistantMessageEvent":
            {"type":"text_delta","delta":"hi there"}});
        assert_eq!(summary(&delta_v).as_deref(), Some("message: hi there"));

        let end = json!({"type":"message_end","message":{"role":"assistant",
            "content":[{"type":"text","text":"done"}]}});
        assert_eq!(summary(&end).as_deref(), Some("message: done"));

        for k in ["turn_start", "turn_end", "message_start", "agent_start"] {
            assert_eq!(summary(&json!({"type": k})), None, "{k} is scaffolding");
        }
        assert_eq!(
            summary(&json!({"type": "agent_settled"})).as_deref(),
            Some("run settled")
        );
        assert_eq!(
            summary(&json!({"type":"tool_execution_start","toolCallId":"t",
                "toolName":"edit","args":{"path":"src/x.rs"}}))
            .as_deref(),
            Some("tool edit started (src/x.rs)")
        );
        assert_eq!(
            summary(&json!({"type":"tool_execution_end","toolName":"edit","isError":true}))
                .as_deref(),
            Some("tool edit failed")
        );
        assert_eq!(
            summary(&json!({"type":"some_future_kind"})).as_deref(),
            Some("pi event: some_future_kind")
        );
        // ACP-shaped and non-pi values are not summarized here.
        assert_eq!(summary(&json!({"sessionUpdate":"tool_call"})), None);
        assert_eq!(summary(&json!({})), None);
    }
}
