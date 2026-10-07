//! Native Pi session discovery uses bounded, structured JSON reads, never CLI
//! screen scraping. Only sessions for the exact workspace are eligible.
use crate::{AgentProfile, Result};
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeConversation {
    pub session_id: String,
    pub session_file: PathBuf,
    pub title: String,
    pub modified: u64,
}
fn line(reader: &mut BufReader<fs::File>, max: usize) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let count = reader
        .by_ref()
        .take(max as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    ensure!(bytes.len() <= max, "native session record is oversized");
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Ok(Some(bytes))
}
pub fn identity(file: &Path) -> Result<(String, PathBuf)> {
    let file = file
        .canonicalize()
        .context("native session file is unavailable")?;
    let mut reader = BufReader::new(fs::File::open(&file)?);
    let header: Value = serde_json::from_slice(
        &line(&mut reader, 64 * 1024)?.context("native session file is empty")?,
    )?;
    ensure!(
        header["type"] == "session"
            && header["version"]
                .as_u64()
                .is_some_and(|v| (1..=3).contains(&v)),
        "unsupported native Pi session format"
    );
    let id = header["id"]
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() < 256
                && !id.starts_with('-')
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .context("invalid native session identity")?
        .to_owned();
    Ok((id, file))
}
pub fn session_dir(profile: &AgentProfile, cwd: &Path) -> Result<PathBuf> {
    let args = match &profile.adapter {
        crate::AdapterKind::PiRpc { args, .. } | crate::AdapterKind::Native { args, .. } => args,
        _ => anyhow::bail!("native Pi history is unavailable for this adapter"),
    };
    let home = profile
        .env
        .get("HOME")
        .cloned()
        .or_else(|| std::env::var("HOME").ok())
        .context("HOME is not configured")?;
    let expand = |text: &str| {
        let p = if let Some(rest) = text.strip_prefix("~/") {
            PathBuf::from(&home).join(rest)
        } else {
            PathBuf::from(text)
        };
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    if let Some((_, path)) = args
        .windows(2)
        .find(|a| a[0] == "--session-dir")
        .map(|a| (&a[0], &a[1]))
    {
        return Ok(expand(path));
    }
    if let Some(path) = profile
        .env
        .get("PI_CODING_AGENT_SESSION_DIR")
        .cloned()
        .or_else(|| std::env::var("PI_CODING_AGENT_SESSION_DIR").ok())
    {
        return Ok(expand(&path));
    }
    let agent_dir = profile
        .env
        .get("PI_CODING_AGENT_DIR")
        .cloned()
        .or_else(|| std::env::var("PI_CODING_AGENT_DIR").ok())
        .map(|p| expand(&p))
        .unwrap_or_else(|| PathBuf::from(&home).join(".pi/agent"));
    let cwd = cwd
        .canonicalize()
        .context("workspace path is unavailable")?;
    let encoded = cwd
        .to_str()
        .context("Pi workspace path is not UTF-8")?
        .trim_start_matches(['/', '\\'])
        .replace(['/', '\\', ':'], "-");
    Ok(agent_dir.join("sessions").join(format!("--{encoded}--")))
}
pub fn inspect(file: &Path, cwd: &Path) -> Result<NativeConversation> {
    let file = file
        .canonicalize()
        .context("native session file is unavailable")?;
    let mut reader = BufReader::new(fs::File::open(&file)?);
    let header = line(&mut reader, 64 * 1024)?.context("native session file is empty")?;
    let header: Value = serde_json::from_slice(&header)?;
    ensure!(
        header["type"] == "session"
            && header["version"]
                .as_u64()
                .is_some_and(|v| (1..=3).contains(&v)),
        "unsupported native Pi session format"
    );
    let stored_cwd = Path::new(
        header["cwd"]
            .as_str()
            .context("native session has no workspace")?,
    )
    .canonicalize()?;
    ensure!(
        stored_cwd == cwd.canonicalize()?,
        "native conversation belongs to a different workspace"
    );
    let id = header["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("native session has no identity")?
        .to_owned();
    let mut title = String::new();
    let mut named = false;
    // Discovery is bounded; native Pi remains responsible for loading the
    // complete session, branches, extensions and provider context.
    let mut scanned = 0;
    for _ in 0..1024 {
        let bytes = match line(&mut reader, 1024 * 1024) {
            Ok(Some(bytes)) => bytes,
            _ => break,
        };
        scanned += bytes.len();
        if scanned > 2 * 1024 * 1024 {
            break;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if entry["type"] == "session_info" {
            if let Some(name) = entry["name"].as_str() {
                title = name.chars().take(80).collect();
                named = true;
            }
        } else if title.is_empty()
            && !named
            && entry["type"] == "message"
            && entry["message"]["role"] == "user"
        {
            let content = &entry["message"]["content"];
            let text = content
                .as_str()
                .or_else(|| content.as_array()?.iter().find_map(|v| v["text"].as_str()));
            if let Some(text) = text {
                title = text
                    .lines()
                    .find(|s| !s.trim().is_empty())
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
            }
        }
    }
    if title.is_empty() {
        title = format!("Pi {}", id.chars().take(12).collect::<String>());
    }
    title = title
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{2028}' && *c != '\u{2029}')
        .take(80)
        .collect();
    let modified = fs::metadata(&file)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(NativeConversation {
        session_id: id,
        session_file: file,
        title,
        modified,
    })
}
pub fn list(profile: &AgentProfile, cwd: &Path) -> Result<Vec<NativeConversation>> {
    let dir = session_dir(profile, cwd)?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(dir)?.take(5000) {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            if let Ok(session) = inspect(&path, cwd) {
                result.push(session);
            }
        }
    }
    result.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then(a.session_file.cmp(&b.session_file))
    });
    Ok(result)
}
pub fn create(profile: &AgentProfile, cwd: &Path) -> Result<NativeConversation> {
    let dir = session_dir(profile, cwd)?;
    fs::create_dir_all(&dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now();
    let file = dir.join(format!(
        "{}_{}.jsonl",
        timestamp.format("%Y-%m-%dT%H-%M-%S-%3fZ"),
        id
    ));
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)?;
    serde_json::to_writer(
        &mut out,
        &serde_json::json!({"type":"session", "version":3, "id":id, "timestamp":timestamp.to_rfc3339(), "cwd":cwd.canonicalize()?}),
    )?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    inspect(&file, cwd)
}
