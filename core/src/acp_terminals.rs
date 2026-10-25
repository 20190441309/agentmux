//! Registry behind ACP `terminal/*` requests.
//!
//! An agent that advertises-and-uses the client `terminal` capability asks
//! the client to run commands for it: `terminal/create` starts one,
//! `terminal/output` polls its (bounded) output, `terminal/wait_for_exit`
//! blocks until it ends, `terminal/kill` stops it while keeping the id
//! valid, and `terminal/release` kills it if needed and forgets the id.
//!
//! Commands run as plain piped child processes (stdout and stderr merged
//! into one buffer) — ACP terminals are not interactive. The buffer keeps
//! the *tail* of the output within `output_byte_limit` and flags
//! `truncated`, as the protocol specifies. Every child is killed when the
//! registry is dropped, so a torn-down connection never leaks processes.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use agent_client_protocol as acp;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{watch, Notify},
};

/// Output kept per terminal when the agent sets no `output_byte_limit`.
const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;
/// Live (unreleased) terminals allowed per connection.
const MAX_TERMINALS: usize = 32;

#[derive(Default)]
struct OutputBuf {
    bytes: Vec<u8>,
    truncated: bool,
}

impl OutputBuf {
    fn push(&mut self, chunk: &[u8], limit: usize) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > limit {
            let mut cut = self.bytes.len() - limit;
            // Never start mid-character: skip UTF-8 continuation bytes.
            while cut < self.bytes.len() && (self.bytes[cut] & 0xC0) == 0x80 {
                cut += 1;
            }
            self.bytes.drain(..cut);
            self.truncated = true;
        }
    }
}

struct Terminal {
    output: Arc<Mutex<OutputBuf>>,
    exit: watch::Receiver<Option<acp::TerminalExitStatus>>,
    kill: Arc<Notify>,
}

impl Terminal {
    fn exit_status(&self) -> Option<acp::TerminalExitStatus> {
        self.exit.borrow().clone()
    }
}

/// Terminals created by one ACP connection.
#[derive(Default)]
pub struct TerminalRegistry {
    terminals: Mutex<HashMap<String, Arc<Terminal>>>,
    next: AtomicU64,
}

impl Drop for TerminalRegistry {
    fn drop(&mut self) {
        if let Ok(map) = self.terminals.lock() {
            for t in map.values() {
                t.kill.notify_one();
            }
        }
    }
}

impl TerminalRegistry {
    fn get(&self, id: &acp::TerminalId) -> acp::Result<Arc<Terminal>> {
        self.terminals
            .lock()
            .unwrap()
            .get(&*id.0)
            .cloned()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data(serde_json::json!(format!("unknown terminal {}", id.0)))
            })
    }

    /// Start `command` in `cwd` (already confined by the caller).
    pub fn create(
        &self,
        req: &acp::CreateTerminalRequest,
        cwd: PathBuf,
    ) -> acp::Result<acp::TerminalId> {
        if self.terminals.lock().unwrap().len() >= MAX_TERMINALS {
            return Err(acp::Error::invalid_params()
                .data(serde_json::json!("too many open terminals; release some")));
        }
        let limit = req
            .output_byte_limit
            .map(|l| l.min(usize::MAX as u64) as usize)
            .unwrap_or(DEFAULT_OUTPUT_LIMIT);

        let mut cmd = Command::new(&req.command);
        cmd.args(&req.args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for var in &req.env {
            cmd.env(&var.name, &var.value);
        }
        let mut child = cmd.spawn().map_err(|e| {
            acp::Error::invalid_params().data(serde_json::json!(format!(
                "failed to start `{}`: {e}",
                req.command
            )))
        })?;

        let output = Arc::new(Mutex::new(OutputBuf::default()));
        let readers: Vec<_> = [
            child.stdout.take().map(|s| spawn_reader(s, &output, limit)),
            child.stderr.take().map(|s| spawn_reader(s, &output, limit)),
        ]
        .into_iter()
        .flatten()
        .collect();

        let (exit_tx, exit_rx) = watch::channel(None);
        let kill = Arc::new(Notify::new());
        let kill_wait = kill.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = kill_wait.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            // Let the readers drain so `exit_status` implies complete output.
            for r in readers {
                let _ = r.await;
            }
            let _ = exit_tx.send(Some(to_exit_status(status.ok())));
        });

        let id = format!("term-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        self.terminals.lock().unwrap().insert(
            id.clone(),
            Arc::new(Terminal {
                output,
                exit: exit_rx,
                kill,
            }),
        );
        Ok(acp::TerminalId::new(id))
    }

    pub fn output(&self, id: &acp::TerminalId) -> acp::Result<acp::TerminalOutputResponse> {
        let t = self.get(id)?;
        let buf = t.output.lock().unwrap();
        Ok(acp::TerminalOutputResponse::new(
            String::from_utf8_lossy(&buf.bytes).into_owned(),
            buf.truncated,
        )
        .exit_status(t.exit_status()))
    }

    pub async fn wait_for_exit(
        &self,
        id: &acp::TerminalId,
    ) -> acp::Result<acp::TerminalExitStatus> {
        let t = self.get(id)?;
        let mut rx = t.exit.clone();
        let status = rx
            .wait_for(|s| s.is_some())
            .await
            .map_err(acp::Error::into_internal_error)?
            .clone();
        Ok(status.unwrap_or_default())
    }

    /// Stop the command but keep the id valid for `output`/`wait_for_exit`.
    pub fn kill(&self, id: &acp::TerminalId) -> acp::Result<()> {
        self.get(id)?.kill.notify_one();
        Ok(())
    }

    /// Kill if still running and forget the id.
    pub fn release(&self, id: &acp::TerminalId) -> acp::Result<()> {
        let t = self
            .terminals
            .lock()
            .unwrap()
            .remove(&*id.0)
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data(serde_json::json!(format!("unknown terminal {}", id.0)))
            })?;
        t.kill.notify_one();
        Ok(())
    }
}

fn spawn_reader<R>(
    mut reader: R,
    output: &Arc<Mutex<OutputBuf>>,
    limit: usize,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let output = output.clone();
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = reader.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            output.lock().unwrap().push(&chunk[..n], limit);
        }
    })
}

fn to_exit_status(status: Option<std::process::ExitStatus>) -> acp::TerminalExitStatus {
    use std::os::unix::process::ExitStatusExt;
    let Some(status) = status else {
        return acp::TerminalExitStatus::new();
    };
    acp::TerminalExitStatus::new()
        .exit_code(status.code().map(|c| c as u32))
        .signal(status.signal().map(signal_name))
}

fn signal_name(n: i32) -> String {
    match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return format!("SIG{n}"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(command: &str, args: &[&str]) -> acp::CreateTerminalRequest {
        acp::CreateTerminalRequest::new(acp::SessionId::new("s"), command)
            .args(args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn registry() -> (TerminalRegistry, PathBuf) {
        (TerminalRegistry::default(), std::env::temp_dir())
    }

    #[test]
    fn output_buffer_keeps_the_tail_on_a_char_boundary() {
        let mut b = OutputBuf::default();
        b.push("héllo wörld".as_bytes(), 6);
        assert!(b.truncated);
        assert!(std::str::from_utf8(&b.bytes).is_ok());
        assert!(b.bytes.len() <= 6);
        assert!(String::from_utf8_lossy(&b.bytes).ends_with("rld"));
    }

    #[tokio::test]
    async fn runs_a_command_and_reports_output_and_exit() {
        let (reg, cwd) = registry();
        let id = reg
            .create(&req("sh", &["-c", "echo out; echo err >&2; exit 3"]), cwd)
            .unwrap();
        let status = reg.wait_for_exit(&id).await.unwrap();
        assert_eq!(status.exit_code, Some(3));
        let out = reg.output(&id).unwrap();
        assert!(out.output.contains("out") && out.output.contains("err"));
        assert!(!out.truncated);
        assert_eq!(out.exit_status.unwrap().exit_code, Some(3));
    }

    #[tokio::test]
    async fn output_limit_truncates_from_the_front() {
        let (reg, cwd) = registry();
        let mut r = req("sh", &["-c", "printf 0123456789abcdef"]);
        r.output_byte_limit = Some(4);
        let id = reg.create(&r, cwd).unwrap();
        reg.wait_for_exit(&id).await.unwrap();
        let out = reg.output(&id).unwrap();
        assert_eq!(out.output, "cdef");
        assert!(out.truncated);
    }

    #[tokio::test]
    async fn kill_ends_the_command_but_keeps_the_id_until_release() {
        let (reg, cwd) = registry();
        let id = reg.create(&req("sleep", &["30"]), cwd).unwrap();
        assert!(reg.output(&id).unwrap().exit_status.is_none());
        reg.kill(&id).unwrap();
        let status = reg.wait_for_exit(&id).await.unwrap();
        assert_eq!(status.signal.as_deref(), Some("SIGKILL"));
        assert!(reg.output(&id).is_ok());
        reg.release(&id).unwrap();
        assert!(reg.output(&id).is_err());
        assert!(reg.release(&id).is_err());
    }

    #[tokio::test]
    async fn spawn_failure_and_unknown_ids_are_errors() {
        let (reg, cwd) = registry();
        assert!(reg.create(&req("/no/such/binary", &[]), cwd).is_err());
        assert!(reg.output(&acp::TerminalId::new("nope")).is_err());
    }

    #[tokio::test]
    async fn env_is_passed_and_terminal_count_is_capped() {
        let (reg, cwd) = registry();
        let mut r = req("sh", &["-c", "printf $AMUX_T"]);
        r.env = vec![acp::EnvVariable::new("AMUX_T", "hello")];
        let id = reg.create(&r, cwd.clone()).unwrap();
        reg.wait_for_exit(&id).await.unwrap();
        assert_eq!(reg.output(&id).unwrap().output, "hello");

        for _ in 0..MAX_TERMINALS - 1 {
            reg.create(&req("sleep", &["30"]), cwd.clone()).unwrap();
        }
        assert!(reg.create(&req("sleep", &["30"]), cwd).is_err());
    }
}
