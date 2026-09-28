//! agentmux-core: shared domain model for the agentmux orchestrator.
//!
//! The model types are pure data — no I/O — so the same types can be used by
//! the server, the client/TUI and tests, and can cross the wire between them
//! unchanged. The exceptions: [`worktree`] shells out to `git` to manage the
//! worktrees backing each workspace, [`config`] reads the TOML config file,
//! and [`registry`] probes agent binaries on the filesystem.

pub mod acp_conn;
pub mod config;
pub mod id;
pub mod model;
pub mod pi_rpc;
pub mod registry;
pub mod store;
pub mod worktree;

pub use acp_conn::AcpConn;
pub use pi_rpc::PiConn;
pub use anyhow::Result;
pub use config::Config;
pub use id::{AgentId, ProjectId, SessionId, WorkspaceId};
pub use model::{
    AdapterKind, AgentProfile, Event, EventKind, Project, Session, SessionRef, SessionState,
    Workspace,
};
pub use registry::AgentRegistry;
pub use store::Store;
pub use worktree::WorktreeManager;
