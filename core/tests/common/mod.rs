//! Shared helpers for agentmux-core integration tests.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Build `agentmux-mock-agent` once per test process and return the path
/// of the produced debug binary.
///
/// `env!("CARGO_BIN_EXE_<name>")` only works for binaries of the *same*
/// package, so cross-package we build the agent once
/// (`$CARGO build -p agentmux-mock-agent`, incremental after the first
/// run) and resolve the artifact at
/// `$CARGO_TARGET_DIR/debug/agentmux-mock-agent`, falling back to
/// `<workspace>/target/debug/agentmux-mock-agent` — the same directory
/// cargo just linked it into.
pub fn mock_agent_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("core/ sits directly under the workspace root")
                .to_path_buf();
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "agentmux-mock-agent"])
                .current_dir(&workspace_root)
                .status()
                .expect("failed to invoke `cargo build -p agentmux-mock-agent`");
            assert!(
                status.success(),
                "`cargo build -p agentmux-mock-agent` failed"
            );

            // Mirror cargo's target-dir resolution: honour
            // CARGO_TARGET_DIR (relative paths are anchored at the
            // workspace root, matching cargo's behaviour for a workspace
            // invocation), else <workspace>/target.
            let target_dir = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .map(|p| {
                    if p.is_absolute() {
                        p
                    } else {
                        workspace_root.join(p)
                    }
                })
                .unwrap_or_else(|| workspace_root.join("target"));
            let binary = target_dir.join("debug").join("agentmux-mock-agent");
            assert!(
                binary.is_file(),
                "mock agent binary missing at {}",
                binary.display()
            );
            binary
        })
        .clone()
}
