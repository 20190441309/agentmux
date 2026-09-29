//! Domain model shared by every agentmux crate (server, client, TUI).

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::{AgentId, ProjectId, SessionId, WorkspaceId};

/// A project registered with agentmux: a repository root that workspaces
/// are created under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    /// Absolute path of the project (repository) root.
    pub root_path: PathBuf,
    pub name: String,
}

/// A git-worktree-backed working directory belonging to a [`Project`].
///
/// Multiple [`Session`]s may share one workspace; the worktree isolates
/// their changes from the main checkout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub project_id: ProjectId,
    pub name: String,
    /// Absolute path of the worktree checkout.
    pub worktree_path: PathBuf,
    /// Branch checked out in the worktree.
    pub branch: String,
    pub created_at: DateTime<Utc>,
}

/// How the orchestrator spawns and talks to an agent process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdapterKind {
    /// ACP (Agent Client Protocol) agent: spawned as `command` + `args`
    /// and spoken to over stdio JSON-RPC.
    Acp { command: PathBuf, args: Vec<String> },
    /// Pi RPC agent variant.
    PiRpc { command: PathBuf, args: Vec<String> },
}

/// A configured agent that the orchestrator can run inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: AgentId,
    pub name: String,
    pub adapter: AdapterKind,
    /// Extra environment variables for the spawned process.
    pub env: BTreeMap<String, String>,
    /// Whether the agent binary was found and usable at last probe.
    pub available: bool,
}

/// Lifecycle state of a [`Session`].
///
/// `Created → Connecting → Ready → Prompting → WaitingPermission → Done |
/// Error` — a session may resume after `Done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Created,
    Connecting,
    Ready,
    Prompting,
    WaitingPermission,
    Done,
    /// Terminal failure; carries a human-readable reason.
    Error(String),
}

/// A pointer into another session's event stream.
///
/// Used to hand context between sessions: "incorporate what session X had
/// produced up to `event_seq`".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    pub session_id: SessionId,
    pub event_seq: u64,
}

/// A generic answer to an agent's permission request — what
/// `session/permission` carries on the wire.
///
/// The daemon maps the decision onto the agent's offered ACP
/// `PermissionOption`s: `AllowOnce`/`AllowAlways` pick the option of the
/// matching kind (unknown kind → an error, the request stays parked),
/// `Reject` prefers `reject_once` then `reject_always` and degrades to
/// ACP `cancelled` when the agent offered no rejection at all, `Cancel`
/// is the protocol's `cancelled` outcome outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this operation only this time.
    AllowOnce,
    /// Allow this operation and remember the choice.
    AllowAlways,
    /// Deny the operation (any `reject_*` option; `cancelled` as the
    /// deny-equivalent fallback).
    Reject,
    /// Abort the pending tool call (ACP `cancelled`).
    Cancel,
}

/// One orchestrated agent session inside a [`Workspace`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub state: SessionState,
    /// Adapter-level session id (e.g. the ACP `sessionId`), populated once
    /// the agent has created or loaded its session.
    pub acp_session_id: Option<String>,
    /// Cross-session references handed to this session as context.
    pub references: Vec<SessionRef>,
    pub created_at: DateTime<Utc>,
}

/// The payload of an [`Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    /// A raw ACP `session/update` notification, kept as opaque JSON for now;
    /// converted to a strong type internally once the ACP crate lands
    /// (Task 5).
    SessionUpdate(serde_json::Value),
    /// The session's [`SessionState`] changed.
    StateChanged {
        from: SessionState,
        to: SessionState,
    },
    /// The agent edited a file (path relative to the worktree where
    /// possible).
    FileEdited { path: PathBuf },
    /// The agent process exited.
    AgentExited { code: Option<i32> },
    /// The agent asked `session/request_permission`; the request is
    /// parked awaiting a `session/permission` answer (or the
    /// timeout/teardown fallback). `request` is the raw ACP
    /// `RequestPermissionRequest` JSON — the offered options included —
    /// kept opaque for the same reason as [`EventKind::SessionUpdate`].
    PermissionRequest {
        /// Daemon-minted correlation id — the key `session/permission`
        /// resolves the request under.
        request_id: String,
        request: serde_json::Value,
    },
    /// A parked permission request concluded (user answer, timeout,
    /// session cancel, or conn teardown).
    ///
    /// `outcome` is `"cancelled"` or `"selected:<option_id>"` — enough
    /// for UIs to dismiss the matching dialog and log the decision
    /// without re-parsing the ACP response shape.
    PermissionResolved { request_id: String, outcome: String },
    /// A message produced by the orchestrator itself (lifecycle notes,
    /// internal errors, ...).
    Orchestrator(String),
}

/// One entry in a session's ordered, persistent event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub session_id: SessionId,
    /// Monotonic per-session sequence number.
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: EventKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize `value` to JSON and back, asserting the roundtrip is lossless.
    fn roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let v = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(v).unwrap();
        assert_eq!(*value, back);
    }

    #[test]
    fn session_state_roundtrips_and_error_carries_message() {
        let s = SessionState::Error("agent exited".into());
        let v = serde_json::to_value(&s).unwrap();
        let back: SessionState = serde_json::from_value(v).unwrap();
        assert!(matches!(back, SessionState::Error(m) if m == "agent exited"));
    }

    #[test]
    fn generated_ids_are_unique_and_roundtrip() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);

        roundtrip(&a);
        roundtrip(&ProjectId::new());
        roundtrip(&WorkspaceId::new());

        let agent = AgentId::new("claude-code");
        let v = serde_json::to_value(&agent).unwrap();
        assert_eq!(v, serde_json::json!("claude-code"));
        let back: AgentId = serde_json::from_value(v).unwrap();
        assert_eq!(agent, back);
    }

    #[test]
    fn adapter_kind_variants_roundtrip() {
        for kind in [
            AdapterKind::Acp {
                command: PathBuf::from("/usr/bin/claude-agent-acp"),
                args: vec!["--verbose".into()],
            },
            AdapterKind::PiRpc {
                command: PathBuf::from("/usr/bin/pi"),
                args: vec![],
            },
        ] {
            roundtrip(&kind);
        }
    }

    #[test]
    fn project_workspace_and_profile_roundtrip() {
        let project = Project {
            id: ProjectId::new(),
            root_path: PathBuf::from("/repos/agentmux"),
            name: "agentmux".into(),
        };
        let workspace = Workspace {
            id: WorkspaceId::new(),
            project_id: project.id,
            name: "feature-x".into(),
            worktree_path: PathBuf::from("/repos/agentmux-wt/feature-x"),
            branch: "feature-x".into(),
            created_at: Utc::now(),
        };
        let profile = AgentProfile {
            id: AgentId::new("claude-code"),
            name: "Claude Code".into(),
            adapter: AdapterKind::Acp {
                command: PathBuf::from("/usr/bin/claude-agent-acp"),
                args: vec![],
            },
            env: BTreeMap::from([("NO_COLOR".to_string(), "1".to_string())]),
            available: true,
        };

        roundtrip(&project);
        roundtrip(&workspace);
        roundtrip(&profile);
    }

    #[test]
    fn session_roundtrips_with_references() {
        let session = Session {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            agent_id: AgentId::new("claude-code"),
            state: SessionState::WaitingPermission,
            acp_session_id: Some("acp-session-42".into()),
            references: vec![SessionRef {
                session_id: SessionId::new(),
                event_seq: 7,
            }],
            created_at: Utc::now(),
        };

        roundtrip(&session);
    }

    #[test]
    fn every_event_kind_roundtrips() {
        let session_id = SessionId::new();
        let kinds = [
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hello"}
            })),
            EventKind::StateChanged {
                from: SessionState::Prompting,
                to: SessionState::Done,
            },
            EventKind::FileEdited {
                path: PathBuf::from("src/main.rs"),
            },
            EventKind::AgentExited { code: Some(0) },
            EventKind::AgentExited { code: None },
            EventKind::PermissionRequest {
                request_id: "req-1".into(),
                request: serde_json::json!({
                    "sessionId": "s",
                    "toolCall": {"title": "Write src/x.rs"},
                    "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
                }),
            },
            EventKind::PermissionResolved {
                request_id: "req-1".into(),
                outcome: "selected:allow".into(),
            },
            EventKind::Orchestrator("workspace cleaned".into()),
        ];

        for (seq, kind) in kinds.into_iter().enumerate() {
            let event = Event {
                session_id,
                seq: seq as u64,
                ts: Utc::now(),
                kind,
            };
            roundtrip(&event);
        }
    }

    /// `session/permission` outcomes ride the wire as snake_case strings.
    #[test]
    fn permission_decision_roundtrips_as_snake_case() {
        for (decision, wire) in [
            (PermissionDecision::AllowOnce, "allow_once"),
            (PermissionDecision::AllowAlways, "allow_always"),
            (PermissionDecision::Reject, "reject"),
            (PermissionDecision::Cancel, "cancel"),
        ] {
            let v = serde_json::to_value(decision).unwrap();
            assert_eq!(v, serde_json::json!(wire));
            let back: PermissionDecision = serde_json::from_value(v).unwrap();
            assert_eq!(decision, back);
        }
        assert!(serde_json::from_value::<PermissionDecision>(serde_json::json!("y")).is_err());
    }
}
