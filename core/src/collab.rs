//! Shared-workspace collaboration layer.
//!
//! When several [`Session`](crate::model::Session)s share one worktree, a
//! `.agentmux/` directory at the worktree root acts as their blackboard:
//!
//! - `context.md` — free-form notes agents write about who owns which files
//!   and what they are doing;
//! - `activity.md` — an append-only log of one-line, timestamped entries
//!   recording what each agent changed.
//!
//! [`shared_context_preamble`] folds both files into a plain-text preamble
//! that the orchestrator can prepend to a sibling session's prompt so the
//! agents can coordinate instead of stepping on each other's files.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use chrono::Utc;

use crate::model::EventKind;
use crate::Result;

/// Directory (relative to the worktree root) holding the shared blackboard.
const SHARED_DIR: &str = ".agentmux";

/// How many recent activity lines the preamble includes.
const ACTIVITY_TAIL: usize = 20;

/// Template written to a fresh `context.md`: a short header telling agents
/// what the file is for.
const CONTEXT_TEMPLATE: &str = "\
# Shared blackboard
#
# This file is shared by every agentmux agent working in this worktree.
# Write down which files/areas you own and anything collaborators must
# know; read it before editing files another agent may be working on.
";

/// Create `<worktree_path>/.agentmux/` with a `context.md` template and an
/// empty `activity.md`.
///
/// Idempotent: existing files are left untouched, so calling this again on
/// an already-initialized worktree never wipes recorded content.
pub fn init_shared_dir(worktree_path: &Path) -> Result<()> {
    let shared = worktree_path.join(SHARED_DIR);
    std::fs::create_dir_all(&shared).context("failed to create .agentmux directory")?;

    let context_path = shared.join("context.md");
    if !context_path.exists() {
        std::fs::write(&context_path, CONTEXT_TEMPLATE)
            .context("failed to write context.md template")?;
    }

    let activity_path = shared.join("activity.md");
    if !activity_path.exists() {
        std::fs::write(&activity_path, "").context("failed to create activity.md")?;
    }

    Ok(())
}

/// Append one timestamped line to `activity.md`:
/// `- [YYYY-MM-DD HH:MM:SS UTC] <agent_name>: <summary>`.
///
/// The `.agentmux/` directory and file are created if missing, so this is
/// safe to call without [`init_shared_dir`].
pub fn append_activity(worktree_path: &Path, agent_name: &str, summary: &str) -> Result<()> {
    let shared = worktree_path.join(SHARED_DIR);
    std::fs::create_dir_all(&shared).context("failed to create .agentmux directory")?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(shared.join("activity.md"))
        .context("failed to open activity.md for appending")?;

    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    writeln!(file, "- [{ts}] {agent_name}: {summary}").context("failed to write activity line")?;
    Ok(())
}

/// Reduce an [`EventKind`] to a one-line activity summary, or `None` when
/// the event is not worth recording on the blackboard (state changes,
/// exits, chatter, ...).
///
/// `FileEdited` maps to `"edited <path>"`. Raw `SessionUpdate` payloads map
/// only when they clearly describe a tool call — e.g. an ACP `tool_call`
/// update carrying a `locations[].path` or a title.
pub fn summarize_event(kind: &EventKind) -> Option<String> {
    match kind {
        EventKind::FileEdited { path } => Some(format!("edited {}", path.display())),
        EventKind::SessionUpdate(update) => summarize_session_update(update),
        _ => None,
    }
}

/// Best-effort summary of a raw `session/update` JSON payload. Only
/// `tool_call` updates carry an obvious action; everything else is `None`.
///
/// Accepts both the bare update object (`{"sessionUpdate": "tool_call",
/// ...}`) and the full notification envelope adapters actually emit
/// (`{"sessionId": ..., "update": {...}}`).
fn summarize_session_update(update: &serde_json::Value) -> Option<String> {
    // Unwrap the notification envelope when present; a bare update object
    // has no `"update"` key of its own, so this is unambiguous.
    let update = update.get("update").unwrap_or(update);
    if update.get("sessionUpdate").and_then(|u| u.as_str()) != Some("tool_call") {
        return None;
    }

    let kind = update.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let path = update
        .get("locations")
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .and_then(|loc| loc.get("path"))
        .and_then(|p| p.as_str());
    let title = update.get("title").and_then(|t| t.as_str());

    Some(match (kind, path, title) {
        ("edit", Some(p), _) => format!("edited {p}"),
        ("delete", Some(p), _) => format!("deleted {p}"),
        ("move", Some(p), _) => format!("moved {p}"),
        (k, Some(p), _) if !k.is_empty() => format!("{k} {p}"),
        (_, Some(p), _) => format!("touched {p}"),
        (_, None, Some(t)) => format!("tool call: {t}"),
        (_, None, None) => "tool call".to_string(),
    })
}

/// Build the shared-context preamble injected into a sibling session's
/// prompt, or `None` when there is nothing to share.
///
/// Returns `Some` only when `session_count >= 2` *and* at least one of
/// `context.md` / `activity.md` under `<worktree_path>/.agentmux/` has
/// non-blank content. The preamble is plain text: an intro line, the full
/// `context.md`, the last [`ACTIVITY_TAIL`] activity lines, and a short
/// coordination instruction.
pub fn shared_context_preamble(
    worktree_path: &Path,
    session_count: usize,
) -> Result<Option<String>> {
    if session_count < 2 {
        return Ok(None);
    }

    let shared = worktree_path.join(SHARED_DIR);
    let context = read_or_empty(&shared.join("context.md"))?;
    let activity = read_or_empty(&shared.join("activity.md"))?;

    if context.trim().is_empty() && activity.trim().is_empty() {
        return Ok(None);
    }

    let activity_lines: Vec<&str> = activity.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail_start = activity_lines.len().saturating_sub(ACTIVITY_TAIL);
    let tail = activity_lines[tail_start..].join("\n");

    let context_section = if context.trim().is_empty() {
        "(empty)".to_string()
    } else {
        context.trim().to_string()
    };
    let activity_section = if tail.is_empty() {
        "(no activity yet)".to_string()
    } else {
        tail
    };

    Ok(Some(format!(
        "You are working in a shared workspace with other agents. \
         The .agentmux/ directory is the shared blackboard: context.md \
         records who is working on what, and activity.md logs what each \
         agent changed.\n\
         \n\
         ## .agentmux/context.md\n\
         {context_section}\n\
         \n\
         ## .agentmux/activity.md (last {ACTIVITY_TAIL} entries)\n\
         {activity_section}\n\
         \n\
         Coordinate through this blackboard: keep context.md updated with \
         the files you are working on, append your changes to activity.md, \
         and do not edit files the other agents are working on without \
         checking first."
    )))
}

/// Read a file to string, treating a missing file as empty.
fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}
