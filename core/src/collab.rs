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

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::Utc;

use crate::id::SessionId;
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

/// Pattern added to the repository's `info/exclude`. Anchored, so it
/// matches `.agentmux/` at the root of the main checkout and of every
/// linked worktree (they share the common `info/exclude`).
const EXCLUDE_PATTERN: &str = "/.agentmux/";

/// Keep agentmux's own files out of `git status`, `git add -A` and the
/// workspace change list: append [`EXCLUDE_PATTERN`] to the repository's
/// `info/exclude` unless already present. Best-effort — a directory that
/// is not inside a git work tree is left alone, and failures are ignored
/// so collaboration never breaks over bookkeeping.
pub fn exclude_shared_dir(root: &Path) {
    let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(root)
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    // Relative output is relative to `root` (the command's cwd).
    let path = root.join(String::from_utf8_lossy(&output.stdout).trim());
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|line| line.trim() == EXCLUDE_PATTERN) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let separator = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let _ = write!(
            file,
            "{separator}# agentmux shared blackboard and worktrees\n{EXCLUDE_PATTERN}\n"
        );
    }
}

/// Create `<worktree_path>/.agentmux/` with a `context.md` template and an
/// empty `activity.md`.
///
/// Idempotent: existing files are left untouched, so calling this again on
/// an already-initialized worktree never wipes recorded content.
pub fn init_shared_dir(worktree_path: &Path) -> Result<()> {
    let shared = worktree_path.join(SHARED_DIR);
    std::fs::create_dir_all(&shared).context("failed to create .agentmux directory")?;
    exclude_shared_dir(worktree_path);

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
    if !shared.is_dir() {
        std::fs::create_dir_all(&shared).context("failed to create .agentmux directory")?;
        exclude_shared_dir(worktree_path);
    }

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
/// (`{"sessionId": ..., "update": {...}}`). A pi-native record
/// (`{"type": "tool_execution_*", ...}`) summarizes too — normally the
/// pi translator emits `tool_call` + `FileEdited` upstream, so this is
/// the defensive path for records that arrived unnormalized (e.g. a
/// persisted pre-translation log).
fn summarize_session_update(update: &serde_json::Value) -> Option<String> {
    // Unwrap the notification envelope when present; a bare update object
    // has no `"update"` key of its own, so this is unambiguous.
    let update = update.get("update").unwrap_or(update);
    if update.get("sessionUpdate").and_then(|u| u.as_str()) != Some("tool_call") {
        return summarize_pi_tool_update(update);
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

/// Summary for a pi-native `tool_execution_*` record, or `None` for
/// other pi kinds (deltas and lifecycle records aren't activity).
/// `tool_execution_update` progress pings are skipped — start/end mark
/// the meaningful moments; an errored `end` reports the failure rather
/// than claiming the file was edited.
fn summarize_pi_tool_update(update: &serde_json::Value) -> Option<String> {
    use crate::pi_shape as ps;
    if ps::kind(update) == Some("tool_execution_update") {
        return None; // progress pings aren't activity
    }
    let name = ps::tool_name(update)?;
    let path = ps::tool_path(update);
    if ps::tool_end_failed(update) {
        return Some(format!("tool {name} failed"));
    }
    Some(match (name, path) {
        (n, Some(p)) if ps::tool_edits_file(n) => format!("edited {p}"),
        (_, Some(p)) => format!("touched {p}"),
        (n, None) => format!("tool call: {n}"),
    })
}

/// The file an event says was changed, as written by the agent: a
/// `FileEdited`, an ACP `tool_call` of kind edit/delete/move with a
/// location, or a pi file-editing tool record that did not fail.
pub fn edited_path(kind: &EventKind) -> Option<PathBuf> {
    match kind {
        EventKind::FileEdited { path } => Some(path.clone()),
        EventKind::SessionUpdate(update) => {
            let update = update.get("update").unwrap_or(update);
            if update.get("sessionUpdate").and_then(|u| u.as_str()) == Some("tool_call") {
                let kind = update.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                if !matches!(kind, "edit" | "delete" | "move") {
                    return None;
                }
                return update
                    .get("locations")
                    .and_then(|l| l.as_array())
                    .and_then(|a| a.first())
                    .and_then(|loc| loc.get("path"))
                    .and_then(|p| p.as_str())
                    .map(PathBuf::from);
            }
            use crate::pi_shape as ps;
            let name = ps::tool_name(update)?;
            (ps::tool_edits_file(name) && !ps::tool_end_failed(update))
                .then(|| ps::tool_path(update).map(PathBuf::from))
                .flatten()
        }
        _ => None,
    }
}

/// How long an edit counts as "recent" for overlap notices.
pub const OVERLAP_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Recent file edits per session within one shared worktree, to notice
/// when two agents change the same file close together. This is a soft
/// activity signal — not a lock, and not proof of a conflict (sequential
/// hand-offs overlap too). In memory only: it covers this daemon's run.
#[derive(Default)]
pub struct OverlapTracker {
    /// path → (session, agent name, last edit) for each editor.
    edits: HashMap<PathBuf, Vec<(SessionId, String, Instant)>>,
    /// (editor, path, other) → when the editor was last told about other.
    notified: HashMap<(SessionId, PathBuf, SessionId), Instant>,
}

impl OverlapTracker {
    /// Record that `session` (`agent`) edited `path` at `now`. Returns the
    /// other sessions that also edited it within [`OVERLAP_WINDOW`] and
    /// that `session` has not yet been told about since their last edit.
    pub fn record(
        &mut self,
        session: SessionId,
        agent: &str,
        path: PathBuf,
        now: Instant,
    ) -> Vec<SessionId> {
        let editors = self.edits.entry(path.clone()).or_default();
        editors.retain(|(_, _, at)| now.duration_since(*at) < OVERLAP_WINDOW);
        let mut fresh = vec![];
        for (other, _, at) in editors.iter() {
            if *other == session {
                continue;
            }
            let key = (session, path.clone(), *other);
            if self.notified.get(&key).is_none_or(|told| told < at) {
                self.notified.insert(key, now);
                fresh.push(*other);
            }
        }
        editors.retain(|(id, _, _)| *id != session);
        editors.push((session, agent.to_string(), now));
        fresh
    }

    /// Files edited by two or more sessions within [`OVERLAP_WINDOW`]:
    /// `path — agent, agent` lines, sorted by path.
    pub fn summary(&self, now: Instant) -> Vec<String> {
        let mut lines: Vec<String> = self
            .edits
            .iter()
            .filter_map(|(path, editors)| {
                let recent: Vec<&str> = editors
                    .iter()
                    .filter(|(_, _, at)| now.duration_since(*at) < OVERLAP_WINDOW)
                    .map(|(_, agent, _)| agent.as_str())
                    .collect();
                (recent.len() >= 2).then(|| format!("- {} — {}", path.display(), recent.join(", ")))
            })
            .collect();
        lines.sort();
        lines
    }

    /// Drop a session's edits (it was removed).
    pub fn forget(&mut self, session: SessionId) {
        for editors in self.edits.values_mut() {
            editors.retain(|(id, _, _)| *id != session);
        }
        self.edits.retain(|_, editors| !editors.is_empty());
        self.notified
            .retain(|(editor, _, other), _| *editor != session && *other != session);
    }
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
    overlaps: &[String],
) -> Result<Option<String>> {
    if session_count < 2 {
        return Ok(None);
    }

    let shared = worktree_path.join(SHARED_DIR);
    let context = read_or_empty(&shared.join("context.md"))?;
    let activity = read_or_empty(&shared.join("activity.md"))?;

    if context.trim().is_empty() && activity.trim().is_empty() && overlaps.is_empty() {
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
    let overlap_section = if overlaps.is_empty() {
        String::new()
    } else {
        format!(
            "\n## Files several agents edited in the last {} minutes\n{}\n\
             Re-read these files before changing them; another agent's edits \
             may be newer than what you last saw.\n",
            OVERLAP_WINDOW.as_secs() / 60,
            overlaps.join("\n")
        )
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
         {overlap_section}\
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
