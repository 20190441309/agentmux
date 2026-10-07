//! Bounded workspace blackboard reads and conflict-checked context writes.
use crate::rpc::WorkspaceContextResult;
use anyhow::{ensure, Context, Result};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

const LIMIT: u64 = 1024 * 1024;

fn read(root: &Path, relative: &str, limit: u64) -> Result<(Option<String>, bool)> {
    let path = root.join(crate::workspace_files::safe_path(
        root,
        Path::new(relative),
    )?);
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, false)),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = vec![];
    file.take(limit + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > limit;
    if truncated {
        bytes.truncate(limit as usize);
        if let Err(error) = std::str::from_utf8(&bytes) {
            ensure!(error.error_len().is_none(), "Shared context is not UTF-8");
            bytes.truncate(error.valid_up_to());
        }
    }
    Ok((
        Some(String::from_utf8(bytes).context("Shared context is not UTF-8")?),
        truncated,
    ))
}

pub fn load(root: &Path) -> Result<WorkspaceContextResult> {
    let (context, context_truncated) = read(root, ".agentmux/context.md", LIMIT)?;
    let (activity, activity_truncated) = read(root, ".agentmux/activity.md", LIMIT)?;
    Ok(WorkspaceContextResult {
        context,
        activity,
        context_truncated,
        activity_truncated,
    })
}

pub fn save(root: &Path, expected: Option<&str>, text: &str) -> Result<WorkspaceContextResult> {
    ensure!(
        text.len() as u64 <= LIMIT,
        "Shared context exceeds 1 MiB; edit kept locally"
    );
    let directory = root.join(crate::workspace_files::safe_path(
        root,
        Path::new(".agentmux"),
    )?);
    fs::create_dir_all(&directory)?;
    let relative = crate::workspace_files::safe_path(root, Path::new(".agentmux/context.md"))?;
    let current = load(root)?;
    ensure!(
        !current.context_truncated && current.context.as_deref() == expected,
        "Shared context changed while editing; edit kept locally. Reload and merge before saving."
    );
    let temporary = directory.join(format!(".context-{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        // Check again after preparing the replacement to avoid overwriting a known newer edit.
        ensure!(
            load(root)?.context.as_deref() == expected,
            "Shared context changed; edit kept locally"
        );
        fs::rename(&temporary, root.join(relative))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written?;
    load(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_context_roundtrips_and_concurrent_edit_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        assert!(load(root.path()).unwrap().context.is_none());
        let first = save(root.path(), None, "中文 context\n").unwrap();
        assert_eq!(first.context.as_deref(), Some("中文 context\n"));
        fs::write(root.path().join(".agentmux/context.md"), "other edit").unwrap();
        assert!(save(root.path(), first.context.as_deref(), "my draft").is_err());
        assert_eq!(
            load(root.path()).unwrap().context.as_deref(),
            Some("other edit")
        );
    }
    #[test]
    fn outside_symlink_and_oversized_writes_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("context.md"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".agentmux")).unwrap();
        assert!(load(root.path()).is_err());
        assert!(save(root.path(), None, "replace").is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("context.md")).unwrap(),
            "secret"
        );
        let root = tempfile::tempdir().unwrap();
        assert!(save(root.path(), None, &"x".repeat(LIMIT as usize + 1)).is_err());
    }
}
