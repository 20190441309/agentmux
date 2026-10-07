//! JSON-RPC 2.0 wire types shared by the agentmux daemon and its clients.
//!
//! Server (Task 11) and client SDK (Task 12) both use these method-name
//! constants and params/result types so the wire protocol cannot drift
//! between them. Pure data — no I/O.
//!
//! Methods are slash-separated (`project/register`) per spec §7. Every
//! request method `M_*` has a matching `XxxParams`/`XxxResult`; methods
//! taking no params or returning only an ack use `()`. `session/subscribe`
//! is answered with a `()` ack, after which the server pushes
//! [`N_SESSION_EVENT`] notifications whose `params` is a serialized
//! [`Event`] ([`SessionEventParams`]).

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{AgentId, ProjectId, SessionId, WorkspaceId};
use crate::model::{
    AgentProfile, Event, PermissionDecision, Project, Session, SessionRef, Workspace,
};

/// Value of the `jsonrpc` field on every envelope message.
pub const JSONRPC_VERSION: &str = "2.0";

// ---------------------------------------------------------------------------
// Method names (spec §7)
// ---------------------------------------------------------------------------

/// Register a repository root as a [`Project`].
pub const M_PROJECT_REGISTER: &str = "project/register";
/// List all registered projects.
pub const M_PROJECT_LIST: &str = "project/list";
/// Deregister a project.
pub const M_PROJECT_REMOVE: &str = "project/remove";

/// Create a worktree-backed [`Workspace`] under a project.
pub const M_WORKSPACE_CREATE: &str = "workspace/create";
pub const M_WORKSPACE_OPEN: &str = "workspace/open";
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOpenParams {
    pub project_id: ProjectId,
}
/// List a project's workspaces.
pub const M_WORKSPACE_LIST: &str = "workspace/list";
/// Remove a workspace and its worktree.
pub const M_WORKSPACE_REMOVE: &str = "workspace/remove";

/// Create a [`Session`] in a workspace; `prompt` is sent as the first turn.
pub const M_SESSION_CREATE: &str = "session/create";
/// Send a prompt turn to a session.
pub const M_SESSION_PROMPT: &str = "session/prompt";
/// Cancel the in-flight prompt turn (ACP `session/cancel`).
pub const M_SESSION_CANCEL: &str = "session/cancel";
/// Answer a parked agent permission request (ACP `session/request_permission`).
pub const M_SESSION_PERMISSION: &str = "session/permission";
/// Kill the session's agent process.
pub const M_SESSION_KILL: &str = "session/kill";
/// List a workspace's sessions.
pub const M_SESSION_LIST: &str = "session/list";
/// Resume a `Done`/`Error` session.
pub const M_SESSION_RESUME: &str = "session/resume";
/// Pi RPC control commands (models, settings, command discovery).
pub const M_SESSION_PI: &str = "session/pi";
/// Rename a conversation in the workbench, independently of adapter names.
pub const M_SESSION_TITLE: &str = "session/title";
pub const M_NATIVE_LIST: &str = "session/native/list";
pub const M_NATIVE_OPEN: &str = "session/native/open";
pub const M_NATIVE_CHECK: &str = "session/native/check";
pub const M_NATIVE_REPORT: &str = "session/native/report";
pub const M_TERMINAL_ATTACH: &str = "session/terminal/attach";
pub const M_TERMINAL_READ: &str = "session/terminal/read";
pub const M_TERMINAL_INPUT: &str = "session/terminal/input";
pub const M_TERMINAL_RESIZE: &str = "session/terminal/resize";
pub const M_TERMINAL_DETACH: &str = "session/terminal/detach";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeListParams {
    pub session_id: SessionId,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeListResult {
    pub conversations: Vec<crate::pi_sessions::NativeConversation>,
    pub native_available: bool,
    pub structured_available: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeOpenParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub session_file: Option<PathBuf>,
    #[serde(default)]
    pub native: bool,
    #[serde(default)]
    pub history: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeControlParams {
    pub session_id: SessionId,
    pub token: String,
    pub session_file: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalAttachParams {
    pub session_id: SessionId,
    pub rows: u16,
    pub cols: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalReadParams {
    pub session_id: SessionId,
    pub token: String,
    pub after: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInputParams {
    pub session_id: SessionId,
    pub token: String,
    pub data: Vec<u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalResizeParams {
    pub session_id: SessionId,
    pub token: String,
    pub rows: u16,
    pub cols: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalDetachParams {
    pub session_id: SessionId,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTitleParams {
    pub session_id: SessionId,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPiParams {
    pub session_id: SessionId,
    pub command: Value,
}
/// Upgrade the connection to a [`N_SESSION_EVENT`] notification stream.
pub const M_SESSION_SUBSCRIBE: &str = "session/subscribe";

/// Read a page of persisted events, newest page when before_seq is absent.
pub const M_SESSION_HISTORY: &str = "session/history";
/// Read a bounded UTF-8 chunk of one serialized persisted event.
pub const M_SESSION_EVENT_READ: &str = "session/event/read";
/// Inspect a file's aggregate worktree diff against HEAD.
pub const M_WORKSPACE_DIFF: &str = "workspace/diff";
pub const M_WORKSPACE_CHANGES: &str = "workspace/changes";
pub const M_WORKSPACE_CONTEXT: &str = "workspace/context";
pub const M_WORKSPACE_CONTEXT_SAVE: &str = "workspace/context/save";

/// List configured agents with availability probing.
pub const M_AGENT_LIST: &str = "agent/list";
/// Register (or update) an [`AgentProfile`].
pub const M_AGENT_REGISTER: &str = "agent/register";

/// Daemon liveness/version info.
pub const M_SERVER_STATUS: &str = "server/status";
/// Ask the daemon to shut down.
pub const M_SERVER_SHUTDOWN: &str = "server/shutdown";

/// All request method names above — handy for dispatch and validation.
pub const ALL_METHODS: &[&str] = &[
    M_WORKSPACE_OPEN,
    M_NATIVE_LIST,
    M_NATIVE_OPEN,
    M_NATIVE_CHECK,
    M_NATIVE_REPORT,
    M_TERMINAL_ATTACH,
    M_TERMINAL_READ,
    M_TERMINAL_INPUT,
    M_TERMINAL_RESIZE,
    M_TERMINAL_DETACH,
    M_SESSION_TITLE,
    M_SESSION_PI,
    M_SESSION_HISTORY,
    M_SESSION_EVENT_READ,
    M_WORKSPACE_DIFF,
    M_WORKSPACE_CHANGES,
    M_WORKSPACE_CONTEXT,
    M_WORKSPACE_CONTEXT_SAVE,
    M_PROJECT_REGISTER,
    M_PROJECT_LIST,
    M_PROJECT_REMOVE,
    M_WORKSPACE_CREATE,
    M_WORKSPACE_LIST,
    M_WORKSPACE_REMOVE,
    M_SESSION_CREATE,
    M_SESSION_PROMPT,
    M_SESSION_CANCEL,
    M_SESSION_PERMISSION,
    M_SESSION_KILL,
    M_SESSION_LIST,
    M_SESSION_RESUME,
    M_SESSION_SUBSCRIBE,
    M_AGENT_LIST,
    M_AGENT_REGISTER,
    M_SERVER_STATUS,
    M_SERVER_SHUTDOWN,
];

/// Server → client notification carrying one normalized [`Event`]
/// ([`SessionEventParams`]) to every `session/subscribe`d connection.
pub const N_SESSION_EVENT: &str = "session/event";

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request (client → server).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    /// `XxxParams` serialized to a `Value`; absent for `()` params.
    #[serde(default)]
    pub params: Value,
    /// Echoed back in the response. Kept as raw `Value` because JSON-RPC
    /// permits string or number ids.
    pub id: Value,
}

impl RpcRequest {
    pub fn new(method: impl Into<String>, params: Value, id: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
            id,
        }
    }
}

/// A JSON-RPC 2.0 notification (no `id`, no response expected) — e.g. the
/// server pushing [`N_SESSION_EVENT`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response (server → client): exactly one of
/// `result`/`error` is `Some`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    /// Same id as the request; `null` when the request id is unknown
    /// (e.g. a parse error).
    pub id: Value,
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: Some(result),
            error: None,
            id,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }
}

/// The `error` member of a [`RpcResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    /// Invalid JSON was received by the server.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON is valid but the value is not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(Self::PARSE_ERROR, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_REQUEST, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            Self::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_PARAMS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Self::INTERNAL_ERROR, message)
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Any internal `anyhow` failure surfaces as [`RpcError::INTERNAL_ERROR`],
/// keeping the error chain in the message.
impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(format!("{e:#}"))
    }
}

// ---------------------------------------------------------------------------
// project/*
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRegisterParams {
    /// Absolute path of the repository root.
    pub root_path: PathBuf,
    /// Display name; the server derives it from `root_path` when absent.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRegisterResult {
    pub project: Project,
}

/// `project/list` takes no parameters.
pub type ProjectListParams = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectListResult {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRemoveParams {
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRemoveResult {
    /// `false` when no project with that id existed.
    pub removed: bool,
}

// ---------------------------------------------------------------------------
// workspace/*
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateParams {
    pub project_id: ProjectId,
    /// Workspace name — used for the worktree dir and branch.
    pub name: String,
    /// Git ref the worktree branches off; the server picks the repo's
    /// default (e.g. `HEAD`) when absent.
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreateResult {
    pub workspace: Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceListParams {
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRemoveParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRemoveResult {
    /// `false` when no workspace with that id existed.
    pub removed: bool,
}

// ---------------------------------------------------------------------------
// session/*
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateParams {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    /// Optional first prompt, sent once the session is `Ready`.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreateResult {
    pub session: Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPromptParams {
    pub session_id: SessionId,
    pub text: String,
    /// Cross-session context references handed to this turn.
    #[serde(default)]
    pub references: Vec<SessionRef>,
}

/// `session/prompt` responds when the turn has run to completion
/// (`TurnEnded`/`Cancelled`/an error event); the turn's output streams
/// as [`N_SESSION_EVENT`] notifications while it runs.
pub type SessionPromptResult = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCancelParams {
    pub session_id: SessionId,
}

pub type SessionCancelResult = ();

/// Params of `session/permission`: answer the permission request a
/// `PermissionRequest` event advertised. `outcome` is the caller's
/// generic decision ([`PermissionDecision`]'s snake_case wire values);
/// the daemon maps it onto the option ids the agent actually offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPermissionParams {
    pub session_id: SessionId,
    /// The `request_id` carried by the `PermissionRequest` event.
    pub request_id: String,
    pub outcome: PermissionDecision,
}

/// `session/permission` acks the answer — the turn resumes on the agent
/// side; `PermissionResolved` arrives on the event stream.
pub type SessionPermissionResult = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKillParams {
    pub session_id: SessionId,
}

pub type SessionKillResult = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListResult {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeParams {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub before_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHistoryResult {
    /// Latest fresh conversation boundary, even if outside this page.
    #[serde(default)]
    pub conversation_start: u64,
    pub events: Vec<Event>,
    pub has_more: bool,
    pub title: Option<String>,
    /// Sequence of the title source, for ordering live edits against history.
    #[serde(default)]
    pub title_seq: u64,
    pub pending_permissions: Vec<Event>,
    /// Events fetched losslessly in bounded chunks by the SDK.
    #[serde(default)]
    pub event_refs: Vec<u64>,
    #[serde(default)]
    pub pending_permission_refs: Vec<u64>,
    /// Latest ACP catalog, independent of transcript pagination.
    #[serde(default)]
    pub available_commands: Option<Event>,
    /// Oversized catalogs use the same lossless chunk transport as events.
    #[serde(default)]
    pub available_commands_ref: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventReadParams {
    pub session_id: SessionId,
    pub seq: u64,
    #[serde(default)]
    pub offset: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventReadResult {
    /// A UTF-8 aligned slice of the serialized event JSON.
    pub data: String,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDiffParams {
    pub workspace_id: WorkspaceId,
    pub path: String,
    #[serde(default)]
    pub path_bytes: Option<Vec<u8>>,
    #[serde(default)]
    pub old_path: Option<String>,
    #[serde(default)]
    pub old_path_bytes: Option<Vec<u8>>,
    #[serde(default)]
    pub scope: DiffScope,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffScope {
    #[default]
    Head,
    Staged,
    Unstaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceChange {
    pub path: String,
    pub path_bytes: Option<Vec<u8>>,
    pub old_path: Option<String>,
    pub old_path_bytes: Option<Vec<u8>>,
    pub index_status: String,
    pub worktree_status: String,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub binary: bool,
    pub size_bytes: Option<u64>,
    pub unavailable: Option<String>,
}

impl WorkspaceChange {
    pub fn key(&self) -> Vec<u8> {
        self.path_bytes
            .clone()
            .unwrap_or_else(|| self.path.as_bytes().to_vec())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceChangesParams {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceChangesResult {
    pub files: Vec<WorkspaceChange>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceContextResult {
    pub context: Option<String>,
    pub activity: Option<String>,
    pub context_truncated: bool,
    pub activity_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceContextSaveParams {
    pub workspace_id: WorkspaceId,
    pub expected: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDiffResult {
    pub text: String,
    #[serde(default)]
    pub scope: DiffScope,
    #[serde(default)]
    pub binary: bool,
    #[serde(default)]
    pub truncated: bool,
}

pub type SessionResumeResult = ();

/// `session/subscribe` takes no parameters — the subscription covers the
/// daemon's whole event bus.
pub type SessionSubscribeParams = ();

/// `session/subscribe` acks immediately; events then arrive as
/// [`N_SESSION_EVENT`] notifications.
pub type SessionSubscribeResult = ();

/// Params of a [`N_SESSION_EVENT`] notification: the normalized [`Event`]
/// itself.
pub type SessionEventParams = Event;

// ---------------------------------------------------------------------------
// agent/*
// ---------------------------------------------------------------------------

/// `agent/list` takes no parameters; profiles are availability-probed
/// server-side.
pub type AgentListParams = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListResult {
    pub agents: Vec<AgentProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegisterParams {
    /// The profile to register; the server fills in `available` by probing.
    pub profile: AgentProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegisterResult {
    pub agent: AgentProfile,
}

// ---------------------------------------------------------------------------
// server/*
// ---------------------------------------------------------------------------

/// `server/status` takes no parameters.
pub type ServerStatusParams = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusResult {
    /// Daemon version (`env!("CARGO_PKG_VERSION")` of the server).
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Sessions currently tracked by the orchestrator.
    pub sessions: usize,
    /// Effective absolute paths, absent on older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
}

/// `server/shutdown` takes no parameters.
pub type ServerShutdownParams = ();

/// `server/shutdown` acks before the daemon exits.
pub type ServerShutdownResult = ();

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// Serialize `value` to JSON and back, asserting the roundtrip is lossless.
    fn roundtrip<T>(value: &T)
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let v = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(v).unwrap();
        assert_eq!(*value, back);
    }

    #[test]
    fn method_constants_cover_spec_section_7() {
        assert_eq!(M_PROJECT_REGISTER, "project/register");
        assert_eq!(M_SESSION_SUBSCRIBE, "session/subscribe");
        assert_eq!(M_SERVER_STATUS, "server/status");
        assert_eq!(M_SERVER_SHUTDOWN, "server/shutdown");
        assert_eq!(N_SESSION_EVENT, "session/event");

        let mut sorted = ALL_METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ALL_METHODS.len(),
            "duplicate method constants"
        );
        assert!(ALL_METHODS.contains(&M_SESSION_HISTORY));
        assert!(ALL_METHODS.contains(&M_WORKSPACE_DIFF));
        assert!(ALL_METHODS.iter().all(|m| m.contains('/')));
    }

    #[test]
    fn session_create_and_prompt_params_roundtrip() {
        let create = SessionCreateParams {
            workspace_id: WorkspaceId::new(),
            agent_id: AgentId::new("claude-code"),
            prompt: Some("fix the bug".into()),
        };
        roundtrip(&create);

        let prompt = SessionPromptParams {
            session_id: SessionId::new(),
            text: "incorporate what session X produced".into(),
            references: vec![SessionRef {
                session_id: SessionId::new(),
                event_seq: 3,
            }],
        };
        roundtrip(&prompt);
    }

    /// `session/permission` params: `outcome` rides the wire as the
    /// decision's snake_case string — a bad value is a params error.
    #[test]
    fn session_permission_params_roundtrip() {
        roundtrip(&SessionPermissionParams {
            session_id: SessionId::new(),
            request_id: "req-1".into(),
            outcome: PermissionDecision::AllowAlways,
        });

        let parsed: SessionPermissionParams = serde_json::from_value(json!({
            "session_id": SessionId::new(),
            "request_id": "r",
            "outcome": "reject",
        }))
        .unwrap();
        assert_eq!(parsed.outcome, PermissionDecision::Reject);

        assert!(
            serde_json::from_value::<SessionPermissionParams>(json!({
                "session_id": SessionId::new(),
                "request_id": "r",
                "outcome": "maybe",
            }))
            .is_err(),
            "an unknown outcome must fail to parse"
        );
    }

    #[test]
    fn optional_param_fields_default_when_absent() {
        let create: SessionCreateParams = serde_json::from_value(json!({
            "workspace_id": WorkspaceId::new(),
            "agent_id": "claude-code",
        }))
        .unwrap();
        assert_eq!(create.prompt, None);

        let prompt: SessionPromptParams = serde_json::from_value(json!({
            "session_id": SessionId::new(),
            "text": "hi",
        }))
        .unwrap();
        assert!(prompt.references.is_empty());
    }

    #[test]
    fn results_carrying_domain_types_roundtrip() {
        roundtrip(&ProjectRegisterResult {
            project: Project {
                id: ProjectId::new(),
                root_path: PathBuf::from("/repos/agentmux"),
                name: "agentmux".into(),
            },
        });
        roundtrip(&SessionListResult { sessions: vec![] });
    }

    #[test]
    fn rpc_error_roundtrips_and_standard_codes_match_jsonrpc() {
        assert_eq!(RpcError::PARSE_ERROR, -32700);
        assert_eq!(RpcError::INVALID_REQUEST, -32600);
        assert_eq!(RpcError::METHOD_NOT_FOUND, -32601);
        assert_eq!(RpcError::INVALID_PARAMS, -32602);
        assert_eq!(RpcError::INTERNAL_ERROR, -32603);

        let e = RpcError::method_not_found("foo/bar");
        assert_eq!(e.code, -32601);
        assert!(e.message.contains("foo/bar"));
        roundtrip(&e);
        roundtrip(&RpcError::invalid_params("missing `session_id`"));
    }

    #[test]
    fn envelope_types_roundtrip() {
        roundtrip(&RpcRequest::new(
            M_SESSION_PROMPT,
            json!({"session_id": SessionId::new(), "text": "hi"}),
            json!(7),
        ));
        roundtrip(&RpcResponse::ok(json!(7), json!({"sessions": []})));
        roundtrip(&RpcResponse::err(
            json!("req-1"),
            RpcError::internal("boom"),
        ));
        roundtrip(&RpcNotification::new(N_SESSION_EVENT, json!({})));
    }
}
