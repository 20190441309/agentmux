//! `agentmux-server` — the agentmux daemon.
//!
//! Thin crate boundary: [`rpc_server`] holds the listener, connection
//! handling and method dispatch; `main.rs` is only CLI plumbing.

pub mod rpc_server;

pub use rpc_server::{
    bind_unix_listener, build_daemon, default_config_path, default_data_dir, dispatch,
    lock_data_dir, Daemon, ServerPaths,
};
