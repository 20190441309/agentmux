//! Full-screen native terminal attachment. Original bytes and coordinates pass
//! through untouched; Ctrl+] m detaches and Ctrl+] Ctrl+] sends a literal prefix.
use agentmux_client::DaemonClient;
use agentmux_core::{terminal::TerminalFrame, SessionId};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;

const MAX_SEED: usize = 1024 * 1024;
pub async fn launch(socket: &Path, id: SessionId, seed: Option<String>) -> Result<bool, String> {
    if seed.as_ref().is_some_and(|s| s.len() > MAX_SEED) {
        return Err("Native draft exceeds the launch limit; draft kept. Paste it in the native interface instead.".into());
    }
    let socket = socket.to_owned();
    tokio::task::spawn_blocking(move || {
        let _tty = SavedTty::capture();
        let mut command =
            std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command
            .arg("--attach")
            .arg(id.to_string())
            .env("AGENTMUX_SOCK", socket);
        let mut seed_pipe = None;
        if let Some(text) = seed {
            let (reader, writer) = UnixStream::pair().map_err(|e| e.to_string())?;
            let fd = reader.as_raw_fd();
            // The anonymous descriptor avoids putting drafts in argv, env, or
            // temporary files. It is the only extra descriptor inherited.
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(fd, 3) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.args(["--seed-fd", "3"]);
            seed_pipe = Some((reader, writer, text));
        }
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        if let Some((reader, mut writer, text)) = seed_pipe {
            drop(reader);
            if let Err(e) = writer.write_all(text.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
            let _ = writer.shutdown(std::net::Shutdown::Write);
        }
        Ok(child.wait().map_err(|e| e.to_string())?.success())
    })
    .await
    .map_err(|e| e.to_string())?
}

struct SavedTty(Option<libc::termios>);
impl SavedTty {
    fn capture() -> Self {
        // Restore the parent's tty even if the attachment process is killed.
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        let ok = unsafe { libc::tcgetattr(0, saved.as_mut_ptr()) } == 0;
        Self(ok.then(|| unsafe { saved.assume_init() }))
    }
}
impl Drop for SavedTty {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, saved);
            }
        }
    }
}
struct DisplayGuard {
    depth: usize,
}
impl Drop for DisplayGuard {
    fn drop(&mut self) {
        let _ = write!(std::io::stdout(), "\x1b[<{}u\x1b[>4;0m\x1b[?2026l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[0m\x1b[?25h\x1b[?1049l", self.depth + 1);
        let _ = std::io::stdout().flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
fn display(frame: &TerminalFrame, guard: &mut DisplayGuard) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    if frame.reset {
        write!(out, "\x1b[<{}u\x1b[>0u\x1b[2J\x1b[H", guard.depth + 1)
            .map_err(|e| e.to_string())?;
    }
    out.write_all(&frame.data).map_err(|e| e.to_string())?;
    if frame.reset {
        out.write_all(b"\x1b[?2026l").map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;
    guard.depth = frame.keyboard_depth;
    Ok(())
}
#[derive(Default)]
pub(crate) struct Escape {
    pending: bool,
}
impl Escape {
    pub(crate) fn consume(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        for &byte in bytes {
            if self.pending {
                self.pending = false;
                if byte == b'm' {
                    return (out, true);
                }
                out.push(0x1d);
                if byte != 0x1d {
                    out.push(byte);
                }
            } else if byte == 0x1d {
                self.pending = true;
            } else {
                out.push(byte);
            }
        }
        (out, false)
    }
}
pub async fn attach(socket: &Path, id: SessionId, seed_fd: Option<i32>) -> Result<(), String> {
    let seed = if let Some(fd) = seed_fd {
        let mut data = Vec::new();
        unsafe { std::fs::File::from_raw_fd(fd) }
            .take(MAX_SEED as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() > MAX_SEED {
            return Err("Native launch draft is too large".into());
        }
        Some(data)
    } else {
        None
    };
    let mut reader = DaemonClient::connect_existing(socket)
        .await
        .map_err(|e| e.to_string())?;
    let mut writer = DaemonClient::connect_existing(socket)
        .await
        .map_err(|e| e.to_string())?;
    let (cols, rows) = crossterm::terminal::size().map_err(|e| e.to_string())?;
    let attached = reader
        .terminal_attach(id, rows, cols)
        .await
        .map_err(|e| e.to_string())?;
    let token = attached.token;
    crossterm::terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let mut guard = DisplayGuard { depth: 0 };
    write!(std::io::stdout(), "\x1b[?1049h\x1b[>0u\x1b[2J\x1b[H").map_err(|e| e.to_string())?;
    display(&attached.frame, &mut guard)?;
    let (frame_tx, mut frame_rx) = mpsc::channel(16);
    let read_token = token.clone();
    let mut after = attached.frame.seq;
    let pump = tokio::spawn(async move {
        loop {
            let frame = reader
                .terminal_read(id, &read_token, after)
                .await
                .map_err(|e| e.to_string());
            let ended = frame.as_ref().map_or(true, |f| f.exit_code.is_some());
            if let Ok(frame) = &frame {
                after = frame.seq;
            }
            if frame_tx.send(frame).await.is_err() || ended {
                break;
            }
        }
    });
    let (input_tx, mut input_rx) = mpsc::channel(64);
    let stopped = Arc::new(AtomicBool::new(false));
    let input_stopped = stopped.clone();
    let input = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut bytes = [0u8; 8192];
        while !input_stopped.load(Ordering::SeqCst) {
            let mut fd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
                continue;
            }
            match stdin.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if input_tx.blocking_send(bytes[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let result = async {
        if let Some(seed) = seed {
            writer.terminal_input(id, &token, b"\x1b[200~".to_vec()).await.map_err(|e| e.to_string())?;
            for chunk in seed.chunks(16384) { writer.terminal_input(id, &token, chunk.to_vec()).await.map_err(|e| e.to_string())?; }
            writer.terminal_input(id, &token, b"\x1b[201~\r".to_vec()).await.map_err(|e| e.to_string())?;
        }
        if attached.frame.exit_code.is_some() { return Ok(()); }
        let mut escape = Escape::default(); let mut size = (cols, rows);
        let mut resize = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                frame = frame_rx.recv() => match frame {
                    Some(Ok(frame)) => { display(&frame, &mut guard)?; if frame.exit_code.is_some() { break; } },
                    Some(Err(error)) => return Err(error), None => break,
                },
                bytes = input_rx.recv() => match bytes {
                    Some(bytes) => {
                        let (data, detach) = escape.consume(&bytes);
                        if !data.is_empty() { writer.terminal_input(id, &token, data).await.map_err(|e| e.to_string())?; }
                        if detach { break; }
                    }, None => break,
                },
                _ = resize.tick() => {
                    if let Ok(next) = crossterm::terminal::size() { if next != size { writer.terminal_resize(id, &token, next.1, next.0).await.map_err(|e| e.to_string())?; size = next; } }
                }
            }
        }
        Ok(())
    }.await;
    stopped.store(true, Ordering::SeqCst);
    drop(input_rx);
    let _ = input.join();
    pump.abort();
    let _ = pump.await;
    let _ = writer.terminal_detach(id, &token).await;
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prefix_keeps_unicode_and_native_escape_sequences() {
        let mut escape = Escape::default();
        let bytes = "中文\x1b[<0;5;6M".as_bytes();
        assert_eq!(escape.consume(bytes), (bytes.to_vec(), false));
        let native_keys = b"\x03\x1bOQ\x1b[200~paste\x1b[201~";
        assert_eq!(escape.consume(native_keys), (native_keys.to_vec(), false));
        assert_eq!(escape.consume(&[0x1d]), (vec![], false));
        assert_eq!(escape.consume(&[0x1d]), (vec![0x1d], false));
        assert_eq!(escape.consume(&[0x1d, b'x']), (vec![0x1d, b'x'], false));
        assert_eq!(escape.consume(&[0x1d, b'm']), (vec![], true));
    }
}
