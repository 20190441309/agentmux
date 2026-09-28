//! agentmux-core: shared domain model for the agentmux orchestrator.
//!
//! The model types are pure data — no I/O — so the same types can be used by
//! the server, the client/TUI and tests, and can cross the wire between them
//! unchanged. The exception is [`worktree`], which shells out to `git` to
//! manage the worktrees backing each workspace.

pub mod id;
pub mod model;
pub mod store;
pub mod worktree;

pub use anyhow::Result;
pub use id::{AgentId, ProjectId, SessionId, WorkspaceId};
pub use model::{
    AdapterKind, AgentProfile, Event, EventKind, Project, Session, SessionRef, SessionState,
    Workspace,
};
pub use store::Store;
pub use worktree::WorktreeManager;
