//! agentmux-core: shared domain model for the agentmux orchestrator.
//!
//! The model types are pure data — no I/O — so the same types can be used by
//! the server, the client/TUI and tests, and can cross the wire between them
//! unchanged. The exceptions: [`worktree`] shells out to `git` to manage the
//! worktrees backing each workspace, [`config`] reads the TOML config file,
//! [`registry`] probes agent binaries on the filesystem, and [`collab`]
//! maintains the `.agentmux/` blackboard shared by sessions in a worktree.

pub mod acp_conn;
pub mod collab;
pub mod config;
pub mod id;
pub mod model;
pub mod orchestrator;
pub mod pi_rpc;
pub mod pi_sessions;
pub mod pi_shape;
pub mod registry;
pub mod rpc;
pub mod stderr;
pub mod store;
pub mod terminal;
pub mod workspace_context;
pub mod workspace_files;
pub mod worktree;

pub use acp_conn::AcpConn;
pub use anyhow::Result;
pub use config::{Config, ConnTimeouts, SpawnOptions};
pub use id::{AgentId, ProjectId, SessionId, WorkspaceId};
pub use model::{
    AdapterKind, AgentProfile, Event, EventKind, NativeSessionBackend, PermissionDecision, Project,
    Session, SessionRef, SessionState, Workspace,
};
pub use orchestrator::{Orchestrator, SpawnedConn};
pub use pi_rpc::PiConn;
pub use registry::AgentRegistry;
pub use rpc::RpcError;
pub use store::Store;
pub use worktree::WorktreeManager;
