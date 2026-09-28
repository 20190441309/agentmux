//! Integration tests for `WorktreeManager` against real git repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentmux_core::worktree::WorktreeManager;
use tempfile::TempDir;

/// Run `git <args>` in `repo`, panicking if the command cannot be spawned.
fn git(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("failed to spawn git")
}

/// Create a throwaway git repo with one commit on `main`.
fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();

    let out = git(repo, &["init", "-b", "main"]);
    assert!(out.status.success(), "git init failed: {out:?}");
    git(repo, &["config", "user.email", "agentmux-test@example.com"]);
    git(repo, &["config", "user.name", "agentmux test"]);

    std::fs::write(repo.join("README.md"), "# test repo\n").unwrap();
    assert!(git(repo, &["add", "."]).status.success());
    assert!(git(repo, &["commit", "-m", "init"]).status.success());
    dir
}

/// Paths of all worktrees registered in `repo` (`git worktree list --porcelain`).
fn worktree_paths(repo: &Path) -> Vec<PathBuf> {
    let out = git(repo, &["worktree", "list", "--porcelain"]);
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("worktree ").map(PathBuf::from))
        .collect()
}

/// Names of all local branches matching `pattern`.
fn branches(repo: &Path, pattern: &str) -> Vec<String> {
    let out = git(
        repo,
        &["branch", "--list", pattern, "--format=%(refname:short)"],
    );
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[test]
fn create_adds_worktree_on_agentmux_branch() {
    let repo = init_repo();

    let (path, branch) = WorktreeManager::create(repo.path(), "demo", "main")
        .expect("create should succeed on a fresh name");

    assert_eq!(branch, "agentmux/demo");
    assert_eq!(path, repo.path().join(".agentmux/worktrees/demo"));
    assert!(path.is_dir(), "worktree path should exist on disk");
    // The worktree is a real checkout of `base`.
    assert!(path.join("README.md").is_file());

    let worktrees = worktree_paths(repo.path());
    assert!(
        worktrees.contains(&path),
        "git worktree list should contain {path:?}, got {worktrees:?}"
    );
    assert_eq!(
        branches(repo.path(), "agentmux/demo"),
        vec!["agentmux/demo"]
    );
}

#[test]
fn create_with_duplicate_name_errors_and_leaves_no_residue() {
    let repo = init_repo();
    WorktreeManager::create(repo.path(), "demo", "main").unwrap();

    let err =
        WorktreeManager::create(repo.path(), "demo", "main").expect_err("duplicate name must fail");
    assert!(
        !err.to_string().is_empty(),
        "error should carry git's stderr"
    );

    // No half-created directory survives under worktrees/.
    let wt_parent = repo.path().join(".agentmux/worktrees");
    let entries: Vec<_> = std::fs::read_dir(&wt_parent).unwrap().collect();
    assert_eq!(
        entries.len(),
        1,
        "only the original worktree dir may remain"
    );
    assert_eq!(
        worktree_paths(repo.path()).len(),
        2,
        "repo should still have exactly main + demo worktrees"
    );
}

#[test]
fn create_rejects_names_that_are_not_single_path_components() {
    let repo = init_repo();

    for bad in ["", ".", "..", "a/b", "trail/", "/abs", "back\\slash"] {
        assert!(
            WorktreeManager::create(repo.path(), bad, "main").is_err(),
            "name {bad:?} should be rejected"
        );
    }
    // Nothing was created under .agentmux for the rejected names.
    let wt_parent = repo.path().join(".agentmux/worktrees");
    assert!(!wt_parent.exists() || std::fs::read_dir(&wt_parent).unwrap().next().is_none());
}

#[test]
fn remove_deletes_worktree_and_agentmux_branch() {
    let repo = init_repo();
    let (path, branch) = WorktreeManager::create(repo.path(), "demo", "main").unwrap();

    // Dirty the worktree so removal genuinely needs --force.
    std::fs::write(path.join("dirty.txt"), "uncommitted work\n").unwrap();

    WorktreeManager::remove(repo.path(), &path).expect("remove should succeed");

    assert!(!path.exists(), "worktree dir should be gone");
    assert_eq!(
        worktree_paths(repo.path()),
        vec![repo.path().canonicalize().unwrap()],
        "only the main worktree should remain"
    );
    assert!(
        branches(repo.path(), "agentmux/demo").is_empty(),
        "branch {branch} should be deleted"
    );
}
