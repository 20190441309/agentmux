//! agentmux-core: shared domain model for the agentmux orchestrator.
//!
//! This crate is pure data — no I/O, no process management — so the same
//! types can be used by the server, the client/TUI and tests, and can cross
//! the wire between them unchanged.

pub mod id;
pub mod model;

pub use anyhow::Result;
pub use id::{AgentId, ProjectId, SessionId, WorkspaceId};
pub use model::{
    AdapterKind, AgentProfile, Event, EventKind, Project, Session, SessionRef, SessionState,
    Workspace,
};
