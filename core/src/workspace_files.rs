//! Read-only, bounded Git inspection. Paths stay literal and repository-scoped.
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::Read,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

use anyhow::{ensure, Context, Result};

use crate::rpc::{
    DiffScope, WorkspaceChange, WorkspaceChangesResult, WorkspaceDiffParams, WorkspaceDiffResult,
};

const QUERY_LIMIT: usize = 1024 * 1024;
const DIFF_LIMIT: usize = 1024 * 1024;
const FILE_LIMIT: u64 = 4 * 1024 * 1024;

struct Capture {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(root)
        .stdin(Stdio::null());
    command
}

fn capture(mut command: Command, limit: usize) -> Result<Capture> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start git inspection")?;
    let mut stdout = child.stdout.take().context("git stdout unavailable")?;
    let mut stderr = child.stderr.take().context("git stderr unavailable")?;
    let errors = std::thread::spawn(move || {
        let mut bytes = vec![];
        (&mut stderr).take(64 * 1024).read_to_end(&mut bytes)?;
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok::<_, std::io::Error>(bytes)
    });
    let mut bytes = vec![];
    let read = (&mut stdout).take(limit as u64 + 1).read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    if truncated || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stderr = errors
        .join()
        .map_err(|_| anyhow::anyhow!("git stderr reader failed"))??;
    read?;
    bytes.truncate(limit);
    Ok(Capture {
        status,
        stdout: bytes,
        stderr,
        truncated,
    })
}

fn query(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = git(root);
    command.args(args);
    let result = capture(command, QUERY_LIMIT)?;
    ensure!(
        !result.truncated,
        "Git inspection exceeds the supported response size"
    );
    ensure!(
        result.status.success(),
        "Git inspection failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result.stdout)
}

fn raw_path(path: &str, bytes: Option<&Vec<u8>>) -> PathBuf {
    bytes
        .map(|bytes| PathBuf::from(OsString::from_vec(bytes.clone())))
        .unwrap_or_else(|| path.into())
}

pub(crate) fn safe_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        path.strip_prefix(root)
            .context("file is outside workspace")?
    } else {
        path
    };
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
        "invalid file path"
    );
    let file = root.join(relative);
    let mut ancestor = file.as_path();
    while !ancestor.exists() && fs::symlink_metadata(ancestor).is_err() {
        ancestor = ancestor.parent().context("file is outside workspace")?;
    }
    ensure!(
        ancestor.canonicalize()?.starts_with(root.canonicalize()?),
        "file is outside workspace"
    );
    Ok(relative.into())
}

fn encoded(bytes: &[u8]) -> (String, Option<Vec<u8>>) {
    match String::from_utf8(bytes.to_vec()) {
        Ok(path) => (path, None),
        Err(_) => (
            String::from_utf8_lossy(bytes).into_owned(),
            Some(bytes.to_vec()),
        ),
    }
}

fn status(bytes: &[u8]) -> Result<Vec<WorkspaceChange>> {
    let mut parts = bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty());
    let mut files = vec![];
    while let Some(record) = parts.next() {
        ensure!(
            record.len() >= 4 && record[2] == b' ',
            "invalid Git porcelain record"
        );
        let (path, path_bytes) = encoded(&record[3..]);
        let (old_path, old_path_bytes) = if [record[0], record[1]]
            .iter()
            .any(|byte| matches!(byte, b'R' | b'C'))
        {
            let old = parts.next().context("missing Git rename source")?;
            let (path, bytes) = encoded(old);
            (Some(path), bytes)
        } else {
            (None, None)
        };
        files.push(WorkspaceChange {
            path,
            path_bytes,
            old_path,
            old_path_bytes,
            index_status: (record[0] as char).to_string(),
            worktree_status: (record[1] as char).to_string(),
            added: None,
            deleted: None,
            binary: false,
            size_bytes: None,
            unavailable: None,
        });
    }
    Ok(files)
}

type Counts = (Option<u64>, Option<u64>);

fn numstat(bytes: &[u8]) -> Result<HashMap<Vec<u8>, Counts>> {
    let mut parts = bytes.split(|byte| *byte == 0).peekable();
    let mut stats = HashMap::new();
    while let Some(record) = parts.next().filter(|part| !part.is_empty()) {
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let added = fields.next().context("missing added count")?;
        let deleted = fields.next().context("missing deleted count")?;
        let path = fields.next().context("missing numstat path")?;
        let path = if path.is_empty() {
            parts.next().context("missing numstat rename source")?;
            parts.next().context("missing numstat rename target")?
        } else {
            path
        };
        let count = |bytes: &[u8]| -> Result<Option<u64>> {
            if bytes == b"-" {
                Ok(None)
            } else {
                Ok(Some(std::str::from_utf8(bytes)?.parse()?))
            }
        };
        stats.insert(path.to_vec(), (count(added)?, count(deleted)?));
    }
    Ok(stats)
}

fn file_stats(root: &Path, file: &mut WorkspaceChange) -> Result<()> {
    let relative = safe_path(root, &raw_path(&file.path, file.path_bytes.as_ref()))?;
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            file.added = Some(0);
            file.deleted = Some(0);
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    file.size_bytes = Some(metadata.len());
    if metadata.len() > FILE_LIMIT {
        return Ok(());
    }
    let bytes = if metadata.file_type().is_symlink() {
        fs::read_link(&path)?.as_os_str().as_bytes().to_vec()
    } else {
        fs::read(&path)?
    };
    file.binary = bytes.contains(&0);
    if !file.binary {
        file.added = Some(
            bytes.iter().filter(|byte| **byte == b'\n').count() as u64
                + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n")),
        );
        file.deleted = Some(0);
    }
    Ok(())
}

pub fn changes(root: &Path) -> Result<WorkspaceChangesResult> {
    let mut files = status(&query(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--renames",
        ],
    )?)?;
    let head = capture(
        {
            let mut command = git(root);
            command.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
            command
        },
        256,
    )?
    .status
    .success();
    let stats = if head {
        numstat(&query(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                "-M",
                "HEAD",
                "--",
            ],
        )?)?
    } else {
        HashMap::new()
    };
    for file in &mut files {
        if let Some((added, deleted)) = stats.get(&file.key()) {
            file.added = *added;
            file.deleted = *deleted;
            file.binary = added.is_none();
        }
        let path = raw_path(&file.path, file.path_bytes.as_ref());
        match safe_path(root, &path) {
            Ok(relative) => {
                file.size_bytes = fs::metadata(root.join(relative))
                    .ok()
                    .map(|metadata| metadata.len())
            }
            Err(error) => {
                file.unavailable = Some(error.to_string());
                continue;
            }
        }
        if file.index_status == "?" || !head {
            if let Err(error) = file_stats(root, file) {
                file.unavailable = Some(error.to_string());
            }
        } else if file.added.is_none() && !file.binary {
            file.added = Some(0);
            file.deleted = Some(0);
        }
    }
    let result = WorkspaceChangesResult { files };
    ensure!(
        serde_json::to_vec(&result)?.len() <= 4 * QUERY_LIMIT,
        "Too many workspace changes for one response"
    );
    Ok(result)
}

pub fn diff(root: &Path, params: &WorkspaceDiffParams) -> Result<WorkspaceDiffResult> {
    let relative = safe_path(root, &raw_path(&params.path, params.path_bytes.as_ref()))?;
    let old = params
        .old_path
        .as_deref()
        .map(|path| safe_path(root, &raw_path(path, params.old_path_bytes.as_ref())))
        .transpose()?;
    let mut tracked = git(root);
    tracked
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(&relative);
    let index = capture(tracked, QUERY_LIMIT)?.status.success();
    let mut in_head = git(root);
    in_head
        .args(["ls-tree", "-z", "--name-only", "HEAD", "--"])
        .arg(&relative);
    if let Some(old) = &old {
        in_head.arg(old);
    }
    let tree = capture(in_head, QUERY_LIMIT)?;
    let head = tree.status.success();
    let tracked = index || (head && !tree.stdout.is_empty());
    let mut command = git(root);
    command.args([
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--find-renames",
    ]);
    let no_index = !tracked || (!head && params.scope == DiffScope::Head);
    if no_index {
        ensure!(
            params.scope != DiffScope::Staged,
            "Untracked files have no staged diff"
        );
        let file = root.join(&relative);
        ensure!(file.is_file(), "file no longer exists");
        if fs::metadata(&file)?.len() > FILE_LIMIT {
            return Ok(WorkspaceDiffResult {
                text: format!(
                    "File exceeds {} MiB; full diff is not loaded.",
                    FILE_LIMIT / 1024 / 1024
                ),
                binary: false,
                truncated: true,
                scope: params.scope,
            });
        }
        command.args(["--no-index", "--", "/dev/null"]).arg(file);
    } else {
        match params.scope {
            DiffScope::Head => {
                command.arg("HEAD");
            }
            DiffScope::Staged => {
                command.arg("--cached");
            }
            DiffScope::Unstaged => {}
        }
        command.arg("--").arg(&relative);
        if let Some(old) = &old {
            command.arg(old);
        }
    }
    let output = capture(command, DIFF_LIMIT)?;
    ensure!(
        output.truncated
            || output.status.success()
            || (no_index && output.status.code() == Some(1)),
        "Git diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let binary = output
        .stdout
        .windows(b"Binary files".len())
        .any(|bytes| bytes == b"Binary files");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.truncated {
        text.push_str("\n[Diff output truncated at 1 MiB]\n");
    }
    if text.is_empty() {
        text = "No changes in this diff scope.".into();
    }
    Ok(WorkspaceDiffResult {
        text,
        scope: params.scope,
        binary,
        truncated: output.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceId;
    use std::io::Write;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        query(dir.path(), &["init", "-b", "main"]).unwrap();
        query(dir.path(), &["config", "user.email", "test@example.com"]).unwrap();
        query(dir.path(), &["config", "user.name", "Workspace Test"]).unwrap();
        dir
    }

    fn commit(root: &Path) {
        query(root, &["add", "."]).unwrap();
        query(root, &["commit", "-m", "fixture"]).unwrap();
    }

    fn params(path: &str, scope: DiffScope) -> WorkspaceDiffParams {
        WorkspaceDiffParams {
            workspace_id: WorkspaceId::new(),
            path: path.into(),
            path_bytes: None,
            old_path: None,
            old_path_bytes: None,
            scope,
        }
    }

    #[test]
    fn status_and_numstat_preserve_rename_and_special_path_bytes() {
        let files = status(b"R  new\tname\0old\nname\0?? \xff\0").unwrap();
        assert_eq!(files[0].path, "new\tname");
        assert_eq!(files[0].old_path.as_deref(), Some("old\nname"));
        assert_eq!(files[1].key(), vec![0xff]);
        let stats = numstat(b"2\t1\t\0old\nname\0new\tname\0-\t-\tbinary\0").unwrap();
        assert_eq!(stats[b"new\tname".as_slice()], (Some(2), Some(1)));
        assert_eq!(stats[b"binary".as_slice()], (None, None));
    }

    #[test]
    fn reads_real_git_changes_and_all_diff_scopes_without_modifying_files() {
        let repo = repo();
        let root = repo.path();
        fs::write(root.join("中文 file.txt"), "original\n").unwrap();
        fs::write(root.join("old.txt"), "rename me\n").unwrap();
        fs::write(root.join("delete.txt"), "deleted\n").unwrap();
        commit(root);
        fs::write(root.join("中文 file.txt"), "staged\n").unwrap();
        query(root, &["add", "中文 file.txt"]).unwrap();
        fs::write(root.join("中文 file.txt"), "working\n").unwrap();
        fs::rename(root.join("old.txt"), root.join("new name.txt")).unwrap();
        query(root, &["add", "old.txt", "new name.txt"]).unwrap();
        fs::remove_file(root.join("delete.txt")).unwrap();
        fs::write(root.join("new\nfile\t.txt"), "first\nsecond").unwrap();
        fs::write(root.join("binary.bin"), [0, 1, 2]).unwrap();
        let before = query(root, &["status", "--porcelain=v1", "-z"]).unwrap();
        let result = changes(root).unwrap();
        let changed = result
            .files
            .iter()
            .find(|file| file.path == "中文 file.txt")
            .unwrap();
        assert_eq!(
            (&changed.index_status, &changed.worktree_status),
            (&"M".into(), &"M".into())
        );
        let renamed = result
            .files
            .iter()
            .find(|file| file.path == "new name.txt")
            .unwrap();
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
        assert!(result
            .files
            .iter()
            .any(|file| file.path == "delete.txt" && file.worktree_status == "D"));
        assert_eq!(
            result
                .files
                .iter()
                .find(|file| file.path == "new\nfile\t.txt")
                .unwrap()
                .added,
            Some(2)
        );
        assert!(
            result
                .files
                .iter()
                .find(|file| file.path == "binary.bin")
                .unwrap()
                .binary
        );
        assert!(diff(root, &params("中文 file.txt", DiffScope::Head))
            .unwrap()
            .text
            .contains("-original"));
        assert!(diff(root, &params("中文 file.txt", DiffScope::Staged))
            .unwrap()
            .text
            .contains("+staged"));
        assert!(diff(root, &params("中文 file.txt", DiffScope::Unstaged))
            .unwrap()
            .text
            .contains("-staged"));
        let mut rename = params("new name.txt", DiffScope::Head);
        rename.old_path = Some("old.txt".into());
        assert!(diff(root, &rename).unwrap().text.contains("rename from"));
        assert!(
            diff(root, &params("binary.bin", DiffScope::Head))
                .unwrap()
                .binary
        );
        assert_eq!(
            query(root, &["status", "--porcelain=v1", "-z"]).unwrap(),
            before
        );
    }

    #[test]
    fn unborn_raw_paths_large_files_and_escape_failures_are_explicit() {
        let repo = repo();
        let root = repo.path();
        let raw = OsString::from_vec(b"invalid-\xff.txt".to_vec());
        fs::write(root.join(&raw), "raw path\n").unwrap();
        let mut large = fs::File::create(root.join("large.txt")).unwrap();
        large.write_all(b"head\n").unwrap();
        large.set_len(FILE_LIMIT + 1).unwrap();
        let result = changes(root).unwrap();
        let file = result
            .files
            .iter()
            .find(|file| file.path_bytes.is_some())
            .unwrap();
        let mut request = params(&file.path, DiffScope::Head);
        request.path_bytes = file.path_bytes.clone();
        assert!(diff(root, &request).unwrap().text.contains("+raw path"));
        assert!(
            diff(root, &params("large.txt", DiffScope::Head))
                .unwrap()
                .truncated
        );
        fs::write(root.join("staged.txt"), "new staged\n").unwrap();
        query(root, &["add", "staged.txt"]).unwrap();
        assert!(diff(root, &params("staged.txt", DiffScope::Staged))
            .unwrap()
            .text
            .contains("+new staged"));
        assert!(diff(root, &params("../outside", DiffScope::Head)).is_err());
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape.txt")).unwrap();
        assert!(diff(root, &params("escape.txt", DiffScope::Head)).is_err());
        assert!(changes(root)
            .unwrap()
            .files
            .iter()
            .find(|file| file.path == "escape.txt")
            .unwrap()
            .unavailable
            .is_some());
        let no_git = tempfile::tempdir().unwrap();
        assert!(changes(no_git.path()).is_err());
    }
}
