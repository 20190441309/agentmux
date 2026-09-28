//! Git-worktree management for agentmux workspaces.
//!
//! Each [`Workspace`](crate::model::Workspace) is backed by a real git
//! worktree at `<repo_root>/.agentmux/worktrees/<name>`, checked out on a
//! branch named `agentmux/<name>`. [`WorktreeManager`] shells out to `git`;
//! it is the one place in this crate that performs I/O.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

use crate::Result;

/// Prefix of every branch managed by [`WorktreeManager`].
const BRANCH_PREFIX: &str = "agentmux/";

/// Directory (relative to the repo root) holding all agentmux worktrees.
const WORKTREE_DIR: &str = ".agentmux/worktrees";

/// Creates and removes the git worktrees backing agentmux workspaces.
pub struct WorktreeManager;

impl WorktreeManager {
    /// Create a worktree named `name` in `repo_root`, branched off `base`.
    ///
    /// Runs `git worktree add -b agentmux/<name>
    /// <repo_root>/.agentmux/worktrees/<name> <base>` and returns the
    /// worktree path plus the new branch name. On failure, any half-created
    /// worktree is cleaned up before the error is returned.
    pub fn create(repo_root: &Path, name: &str, base: &str) -> Result<(PathBuf, String)> {
        // `name` must be a single path component: it is used verbatim as both
        // the branch suffix and the worktree dir name, and `remove` maps the
        // path back to the branch via `file_name` — `..`, `a/b` or an empty
        // name would escape `WORKTREE_DIR`, produce an invalid refname, or
        // fail to round-trip.
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            bail!("invalid worktree name: {name:?}");
        }

        let branch = format!("{BRANCH_PREFIX}{name}");
        let worktree_path = repo_root.join(WORKTREE_DIR).join(name);

        std::fs::create_dir_all(worktree_path.parent().unwrap())
            .context("failed to create .agentmux/worktrees")?;

        // Snapshot what already exists so that failure cleanup below only
        // removes what *this* call may have created — e.g. a duplicate `name`
        // must leave the original worktree and branch untouched.
        let path_existed = worktree_path.exists();
        let branch_existed = branch_exists(repo_root, &branch);

        let output = Command::new("git")
            .args(["worktree", "add", "-b", &branch])
            .arg(&worktree_path)
            .arg(base)
            .current_dir(repo_root)
            .output()
            .context("failed to spawn git")?;

        if !output.status.success() {
            // Best-effort cleanup of whatever `git worktree add` left behind.
            if !path_existed {
                let _ = Command::new("git")
                    .args(["worktree", "remove", "--force"])
                    .arg(&worktree_path)
                    .current_dir(repo_root)
                    .output();
                if worktree_path.exists() {
                    let _ = std::fs::remove_dir_all(&worktree_path);
                }
            }
            if !branch_existed {
                let _ = Command::new("git")
                    .args(["branch", "-D", &branch])
                    .current_dir(repo_root)
                    .output();
            }
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok((worktree_path, branch))
    }

    /// Remove `worktree_path` and delete its corresponding `agentmux/` branch.
    ///
    /// Runs `git worktree remove --force <path>` (discarding uncommitted
    /// changes) followed by `git branch -D agentmux/<name>`, where `<name>`
    /// is the worktree directory's final component — the same convention
    /// [`create`](Self::create) uses.
    pub fn remove(repo_root: &Path, worktree_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(worktree_path)
            .current_dir(repo_root)
            .output()
            .context("failed to spawn git")?;

        if !output.status.success() {
            bail!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let name = worktree_path
            .file_name()
            .context("worktree path has no final component")?
            .to_string_lossy();
        let branch = format!("{BRANCH_PREFIX}{name}");

        if branch_exists(repo_root, &branch) {
            let output = Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(repo_root)
                .output()
                .context("failed to spawn git")?;

            if !output.status.success() {
                bail!(
                    "git branch -D {branch} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }

        Ok(())
    }
}

/// Whether `refs/heads/<branch>` exists in `repo_root`.
fn branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
