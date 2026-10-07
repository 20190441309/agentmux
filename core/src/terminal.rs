//! Daemon-owned native terminals. Output and input are never persisted to logs.
use crate::{AgentProfile, Event, EventKind, Result, SessionId};
use anyhow::{anyhow, ensure, Context};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, Notify};
use uuid::Uuid;

const BUFFER_BYTES: usize = 4 * 1024 * 1024;
const READ_BYTES: usize = 64 * 1024;
const LEASE_TIME: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalFrame {
    pub seq: u64,
    #[serde(with = "byte_wire")]
    pub data: Vec<u8>,
    pub reset: bool,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub keyboard_depth: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalAttached {
    pub token: String,
    pub frame: TerminalFrame,
}
struct State {
    parser: vt100::Parser,
    chunks: VecDeque<(u64, Vec<u8>, usize)>,
    bytes: usize,
    seq: u64,
    exit_code: Option<i32>,
    owner: Option<(String, Instant)>,
    ever_attached: bool,
    modes_parser: vte::Parser,
    modes: KeyboardModes,
}
impl State {
    fn authorize(&mut self, token: &str) -> Result<()> {
        ensure!(
            self.owner
                .as_ref()
                .is_some_and(|(id, deadline)| id == token && *deadline > Instant::now()),
            "native terminal control lease expired or belongs to another client"
        );
        self.owner.as_mut().unwrap().1 = Instant::now() + LEASE_TIME;
        Ok(())
    }
    fn frame(&self, after: Option<u64>) -> TerminalFrame {
        let replay = after.is_some_and(|seq| {
            seq <= self.seq
                && self
                    .chunks
                    .front()
                    .is_none_or(|(first, _, _)| seq.saturating_add(1) >= *first)
        });
        if replay {
            let mut data = Vec::new();
            let mut seq = after.unwrap();
            let mut keyboard_depth = self.modes.stack.len();
            for (id, chunk, depth) in &self.chunks {
                if *id > seq {
                    if !data.is_empty() && data.len() + chunk.len() > READ_BYTES {
                        break;
                    }
                    data.extend_from_slice(chunk);
                    seq = *id;
                    keyboard_depth = *depth;
                }
            }
            TerminalFrame {
                seq,
                data,
                reset: false,
                exit_code: (seq == self.seq).then_some(self.exit_code).flatten(),
                keyboard_depth,
            }
        } else {
            let mut data = self.parser.screen().state_formatted();
            data.extend(self.modes.formatted());
            TerminalFrame {
                seq: self.seq,
                data,
                reset: true,
                exit_code: self.exit_code,
                keyboard_depth: self.modes.stack.len(),
            }
        }
    }
}
pub struct NativeTerminal {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    events: broadcast::Sender<Event>,
    first_events: Mutex<Option<broadcast::Receiver<Event>>>,
    closed: AtomicBool,
    pid: Option<u32>,
    session_id: SessionId,
}
impl std::fmt::Debug for NativeTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTerminal")
            .field("pid", &self.pid)
            .finish()
    }
}
impl NativeTerminal {
    pub fn spawn(profile: &AgentProfile, cwd: &Path, args: &[String]) -> Result<Self> {
        let crate::AdapterKind::Native { command, .. } = &profile.adapter else {
            return Err(anyhow!("not a native agent"));
        };
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .context("failed to allocate native terminal")?;
        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env(
            "TERM",
            profile
                .env
                .get("TERM")
                .map(String::as_str)
                .unwrap_or("xterm-256color"),
        );
        for (key, value) in &profile.env {
            cmd.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to launch native agent")?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let state = Arc::new(Mutex::new(State {
            parser: vt100::Parser::new(24, 80, 2000),
            chunks: VecDeque::new(),
            bytes: 0,
            seq: 0,
            exit_code: None,
            owner: None,
            ever_attached: false,
            modes_parser: vte::Parser::new(),
            modes: KeyboardModes::default(),
        }));
        let notify = Arc::new(Notify::new());
        let (events, first_events) = broadcast::channel(16);
        let session_id = SessionId::new();
        let output_state = state.clone();
        let output_notify = notify.clone();
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(format!("native-output-{session_id}"))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut state = output_state.lock().unwrap();
                            state.parser.process(&buf[..n]);
                            let State {
                                modes_parser,
                                modes,
                                ..
                            } = &mut *state;
                            modes_parser.advance(modes, &buf[..n]);
                            state.seq += 1;
                            let seq = state.seq;
                            let depth = state.modes.stack.len();
                            state.chunks.push_back((seq, buf[..n].to_vec(), depth));
                            state.bytes += n;
                            while state.bytes > BUFFER_BYTES {
                                if let Some((_, data, _)) = state.chunks.pop_front() {
                                    state.bytes -= data.len();
                                }
                            }
                            drop(state);
                            output_notify.notify_waiters();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = drained_tx.send(());
            })?;
        let exit_state = state.clone();
        let exit_notify = notify.clone();
        let exit_events = events.clone();
        std::thread::Builder::new()
            .name(format!("native-exit-{session_id}"))
            .spawn(move || {
                let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(1);
                // Drain final output before announcing exit to an attached client.
                let _ = drained_rx.recv_timeout(Duration::from_secs(1));
                exit_state.lock().unwrap().exit_code = Some(code);
                exit_notify.notify_waiters();
                let _ = exit_events.send(Event {
                    session_id,
                    seq: 0,
                    ts: chrono::Utc::now(),
                    kind: EventKind::AgentExited { code: Some(code) },
                });
            })?;
        Ok(Self {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            state,
            notify,
            events,
            first_events: Mutex::new(Some(first_events)),
            closed: AtomicBool::new(false),
            pid,
            session_id,
        })
    }
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.first_events
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| self.events.subscribe())
    }
    pub fn attach(&self, rows: u16, cols: u16) -> Result<TerminalAttached> {
        validate_size(rows, cols)?;
        let _writer = self.writer.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        ensure!(
            state
                .owner
                .as_ref()
                .is_none_or(|(_, deadline)| *deadline <= Instant::now()),
            "native terminal is controlled by another client; detach it first"
        );
        let token = Uuid::new_v4().to_string();
        state.owner = Some((token.clone(), Instant::now() + LEASE_TIME));
        self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            ..PtySize::default()
        })?;
        state.parser.screen_mut().set_size(rows, cols);
        let first =
            !state.ever_attached && state.chunks.front().is_none_or(|(seq, _, _)| *seq == 1);
        let frame = if first {
            state.frame(Some(0))
        } else {
            state.frame(None)
        };
        state.ever_attached = true;
        drop(state);
        // Let native UIs redraw custom widgets/images at reattachment, rather
        // than relying on the text screen snapshot to represent those assets.
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(pid as i32, libc::SIGWINCH);
            }
        }
        Ok(TerminalAttached { token, frame })
    }
    pub async fn read(&self, token: &str, after: u64) -> Result<TerminalFrame> {
        let changed = self.notify.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let frame = {
            let mut state = self.state.lock().unwrap();
            state.authorize(token)?;
            state.frame(Some(after))
        };
        if !frame.data.is_empty() || frame.exit_code.is_some() || frame.reset {
            return Ok(frame);
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), changed).await;
        let mut state = self.state.lock().unwrap();
        state.authorize(token)?;
        Ok(state.frame(Some(after)))
    }
    pub fn input(&self, token: &str, data: &[u8]) -> Result<()> {
        ensure!(data.len() <= READ_BYTES, "native input packet is too large");
        let mut writer = self.writer.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        state.authorize(token)?;
        ensure!(state.exit_code.is_none(), "native agent has exited");
        drop(state);
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }
    pub fn resize(&self, token: &str, rows: u16, cols: u16) -> Result<()> {
        validate_size(rows, cols)?;
        let mut state = self.state.lock().unwrap();
        state.authorize(token)?;
        self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            ..PtySize::default()
        })?;
        state.parser.screen_mut().set_size(rows, cols);
        Ok(())
    }
    pub fn detach(&self, token: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.authorize(token)?;
        state.owner = None;
        Ok(())
    }
    pub fn interrupt(&self) -> Result<()> {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(&[3])?;
        writer.flush()?;
        Ok(())
    }
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.state.lock().unwrap().exit_code.is_some() {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.killer.lock().unwrap().kill();
    }
}
impl Drop for NativeTerminal {
    fn drop(&mut self) {
        self.close();
    }
}
fn validate_size(rows: u16, cols: u16) -> Result<()> {
    ensure!(
        rows > 0 && cols > 0 && u32::from(rows) * u32::from(cols) <= 160_000,
        "invalid or oversized native terminal dimensions"
    );
    Ok(())
}
mod byte_wire {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(data))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Default)]
struct KeyboardModes {
    base: u16,
    stack: Vec<u16>,
    modify_other: u16,
}
impl KeyboardModes {
    fn formatted(&self) -> Vec<u8> {
        let mut output = format!("\x1b[={}u\x1b[>4;{}m", self.base, self.modify_other).into_bytes();
        for flags in &self.stack {
            output.extend(format!("\x1b[>{flags}u").bytes());
        }
        output
    }
}
impl vte::Perform for KeyboardModes {
    fn csi_dispatch(&mut self, params: &vte::Params, private: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let values: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();
        let first = values.first().copied().unwrap_or(0);
        match (private, action) {
            (b">", 'u') if self.stack.len() < 64 => self.stack.push(first),
            (b"<", 'u') => {
                for _ in 0..first.max(1) {
                    self.stack.pop();
                }
            }
            (b"=", 'u') => {
                if let Some(last) = self.stack.last_mut() {
                    *last = first;
                } else {
                    self.base = first;
                }
            }
            (b">", 'm') if first == 4 => self.modify_other = values.get(1).copied().unwrap_or(0),
            _ => {}
        }
    }
}
