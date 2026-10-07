//! External editing and clipboard commands use argv parsing, never an implicit shell.
use agentmux_core::SessionId;
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

pub const TEXT_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
pub enum EditRequest {
    Draft {
        session: SessionId,
        text: String,
    },
    Context {
        workspace: agentmux_core::WorkspaceId,
        original: Option<String>,
        text: String,
    },
}

pub fn edit_text(text: &str) -> Result<String, String> {
    let command = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".into());
    edit_with(&command, text)
}

pub fn edit_with(command: &str, text: &str) -> Result<String, String> {
    let args = shell_words::split(command).map_err(|error| error.to_string())?;
    let (program, args) = args.split_first().ok_or("No editor command configured")?;
    let mut file = tempfile::Builder::new()
        .prefix("agentmux-edit-")
        .suffix(".md")
        .tempfile()
        .map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())?;
    let status = Command::new(program)
        .args(args)
        .arg(file.path())
        .status()
        .map_err(|error| format!("Editor could not start: {error}"))?;
    if !status.success() {
        return Err(format!(
            "Editor cancelled or failed ({status}); original text kept"
        ));
    }
    let mut bytes = vec![];
    std::fs::File::open(file.path())
        .map_err(|error| error.to_string())?
        .take(TEXT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > TEXT_LIMIT {
        return Err("Editor output exceeds 8 MiB; original text kept".into());
    }
    String::from_utf8(bytes).map_err(|_| "Editor output is not UTF-8; original text kept".into())
}

pub fn clipboard(text: &str) -> Result<(), String> {
    if let Ok(command) = std::env::var("AGENTMUX_CLIPBOARD_COMMAND") {
        let args = shell_words::split(&command).map_err(|error| error.to_string())?;
        return copy_with(&args, text);
    }
    let choices: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    } else {
        &[
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };
    let mut errors = vec![];
    for args in choices {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).into()).collect();
        match copy_with(&owned, text) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(error),
        }
    }
    Err(format!("Clipboard unavailable: {}", errors.join("; ")))
}

fn copy_with(args: &[String], text: &str) -> Result<(), String> {
    let (program, args) = args
        .split_first()
        .ok_or("No clipboard command configured")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let written = child
        .stdin
        .take()
        .ok_or("Clipboard input unavailable")?
        .write_all(text.as_bytes());
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.to_string());
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Clipboard command failed: {status}"))
    }
}

pub fn retain_copy(text: &str) -> Result<tempfile::TempPath, String> {
    let mut file = tempfile::Builder::new()
        .prefix("agentmux-copy-")
        .suffix(".txt")
        .tempfile()
        .map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .map_err(|error| error.to_string())?;
    Ok(file.into_temp_path())
}

pub fn display_path(path: &tempfile::TempPath) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_preserves_literal_content_arguments_and_failure() {
        let text = "中文\n    indentation\n`literal`\n";
        assert_eq!(edit_with("true", text).unwrap(), text);
        assert!(edit_with("false", text).is_err());
        assert!(edit_with("/agentmux-missing-editor", text).is_err());
        assert!(edit_with("'unterminated", text).is_err());
    }
    #[test]
    fn clipboard_failure_is_reported_and_fallback_preserves_raw_text() {
        let text = "```rust\n  中文();\n```\n";
        assert!(copy_with(&["false".into()], text).is_err());
        let path = retain_copy(text).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
