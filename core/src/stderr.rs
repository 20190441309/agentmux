//! Agent stderr capture.
//!
//! Both adapters spawn the agent with its stderr piped and hand the read
//! end to [`spawn_drain`], which runs a small tokio task on the conn's own
//! worker runtime. Every line the agent writes is:
//!
//! - pushed onto a shared last-N ring ([`SharedTail`], [`TAIL_LINES`]
//!   lines), so error paths and exit watchers can surface the freshest
//!   stderr without re-reading the log, and
//! - appended to the session's stderr log (`<data_dir>/sessions/
//!   <id>.stderr.log`, path carried in
//!   [`crate::config::SpawnOptions::stderr_log`]) through a
//!   [`CappedLog`] that truncates once [`LOG_CAP_BYTES`] is exceeded so a
//!   noisy agent cannot fill the disk.
//!
//! The drain task lives on the conn's runtime and ends on stderr EOF
//! (child exit or pipe close); [`StderrCapture::drop`] aborts it as a
//! fallback, so teardown never leaves a drain running.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::Notify;

/// Number of stderr lines retained in memory for error/tail reporting.
/// The task asks for "approximately ten".
pub const TAIL_LINES: usize = 12;

/// Max bytes the on-disk stderr log keeps. On overflow the file is
/// truncated and a marker line is written, so the log always holds the
/// freshest ≤ `LOG_CAP_BYTES` of stderr.
pub const LOG_CAP_BYTES: u64 = 256 * 1024;

/// How long exit watchers / error paths wait for the drain to flush its
/// final lines after the child died. EOF lands almost instantly on child
/// death; the bound only matters if the reader is somehow stuck.
pub const EOF_WAIT: Duration = Duration::from_millis(500);

/// Shared view of the agent's last stderr lines plus an EOF signal.
/// Cloning is cheap (`Arc`); the drain task holds one side, the conn and
/// its exit watcher the other.
#[derive(Clone, Debug)]
pub struct SharedTail {
    inner: Arc<TailShared>,
}

#[derive(Debug)]
struct TailShared {
    lines: Mutex<VecDeque<String>>,
    /// Set once the drain hit EOF; paired with `eof` so `wait_eof` wakes.
    drained: AtomicBool,
    eof: Notify,
}

impl SharedTail {
    fn new() -> SharedTail {
        SharedTail {
            inner: Arc::new(TailShared {
                lines: Mutex::new(VecDeque::new()),
                drained: AtomicBool::new(false),
                eof: Notify::new(),
            }),
        }
    }

    /// Snapshot of the retained lines, oldest → newest.
    pub fn lines(&self) -> Vec<String> {
        self.inner.lines.lock().unwrap().iter().cloned().collect()
    }

    /// `"\nagent stderr tail:\n  <line>…"` when the tail is non-empty,
    /// else `""` — shaped for appending onto error strings and notes.
    pub fn suffix(&self) -> String {
        let lines = self.lines();
        if lines.is_empty() {
            return String::new();
        }
        let mut out = String::from("\nagent stderr tail:");
        for line in lines {
            out.push('\n');
            out.push_str("  ");
            out.push_str(&line);
        }
        out
    }

    /// Resolve once the drain observed stderr EOF, or after `bound`
    /// elapses. Bounded so a stuck reader cannot wedge teardown or an
    /// error path; returns instantly once `drained` is set.
    pub async fn wait_eof(&self, bound: Duration) {
        let _ = tokio::time::timeout(bound, self.wait_eof_inner()).await;
    }

    async fn wait_eof_inner(&self) {
        loop {
            // `notify_waiters` stores no permit: register interest before
            // checking the flag so a `finish` landing in between still
            // wakes this waiter.
            let notified = self.inner.eof.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.drained.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    /// Push one line, dropping the oldest past [`TAIL_LINES`].
    fn push(&self, line: String) {
        let mut lines = self.inner.lines.lock().unwrap();
        if lines.len() == TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Mark the drain finished and wake every waiter.
    fn finish(&self) {
        self.inner.drained.store(true, Ordering::Release);
        self.inner.eof.notify_waiters();
    }
}

/// Owning handle for a spawned stderr drain task. [`SharedTail`] is
/// reachable via [`StderrCapture::tail`]; dropping the capture aborts
/// the task (normally it has already exited on EOF).
#[derive(Debug)]
pub struct StderrCapture {
    tail: SharedTail,
    drain: tokio::task::JoinHandle<()>,
}

impl StderrCapture {
    pub fn tail(&self) -> SharedTail {
        self.tail.clone()
    }

    /// `tail.suffix()` — the formatted last-lines block, or `""`.
    pub fn tail_suffix(&self) -> String {
        self.tail.suffix()
    }
}

impl Drop for StderrCapture {
    fn drop(&mut self) {
        self.drain.abort();
    }
}

/// Spawn the stderr drain on the caller's tokio runtime. `stderr` is the
/// child's piped stderr (anything `AsyncRead + Unpin + Send` — tests feed
/// a `duplex`); `path` is the optional on-disk log. The file is created
/// lazily on the first line so a quiet agent leaves no empty log.
pub fn spawn_drain<R>(stderr: R, path: Option<PathBuf>) -> StderrCapture
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let tail = SharedTail::new();
    let task_tail = tail.clone();
    let drain = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut log = path.and_then(|p| CappedLog::open(&p).ok());
        let mut buf = Vec::new();
        // Raw bytes, decoded lossily: stderr is not guaranteed UTF-8, and
        // stopping on a stray byte would close the pipe under the agent.
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
            }
            let line = String::from_utf8_lossy(&buf).into_owned();
            task_tail.push(line.clone());
            if let Some(log) = &mut log {
                log.write_line(&line);
            }
        }
        task_tail.finish();
    });
    StderrCapture { tail, drain }
}

/// Line-wise append-only writer that keeps the file under
/// [`LOG_CAP_BYTES`]: once appending a line would exceed the cap the
/// file is truncated, a marker line is written, and new lines continue
/// from there — so the log always ends with the freshest output.
struct CappedLog {
    file: File,
    written: u64,
}

impl CappedLog {
    fn open(path: &Path) -> std::io::Result<CappedLog> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(CappedLog { file, written })
    }

    /// Append `line` plus `\n`, truncating-in-place on cap overflow.
    /// IO errors are dropped on the floor: stderr logging is
    /// best-effort and must never disturb the drain loop.
    fn write_line(&mut self, line: &str) {
        let n = line.len() as u64 + 1;
        if self.written + n > LOG_CAP_BYTES {
            if self.file.set_len(0).is_err() || self.file.seek(SeekFrom::Start(0)).is_err() {
                return;
            }
            let marker = format!("... {} bytes of earlier stderr truncated ...", self.written);
            if self.file.write_all(marker.as_bytes()).is_err()
                || self.file.write_all(b"\n").is_err()
            {
                return;
            }
            self.written = marker.len() as u64 + 1;
        }
        if self.file.write_all(line.as_bytes()).is_err() || self.file.write_all(b"\n").is_err() {
            return;
        }
        self.written += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn tail_ring_keeps_the_last_n_lines() {
        let tail = SharedTail::new();
        for i in 1..=20 {
            tail.push(format!("line {i}"));
        }
        let lines = tail.lines();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines.first().unwrap(), "line 9");
        assert_eq!(lines.last().unwrap(), "line 20");
    }

    #[test]
    fn suffix_is_empty_for_a_quiet_agent_and_formatted_otherwise() {
        let tail = SharedTail::new();
        assert_eq!(tail.suffix(), "");
        tail.push("boom".to_string());
        let suffix = tail.suffix();
        assert!(suffix.contains("stderr tail"), "{suffix:?}");
        assert!(suffix.contains("boom"), "{suffix:?}");
    }

    #[test]
    fn capped_log_truncates_on_overflow_and_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap.log");
        let mut log = CappedLog::open(&path).unwrap();

        // Overfill by writing lines larger than the cap boundary.
        let big = "x".repeat(1024);
        for _ in 0..(LOG_CAP_BYTES / 1024 + 4) {
            log.write_line(&big);
        }
        let meta = std::fs::metadata(&path).unwrap();
        assert!(
            meta.len() <= LOG_CAP_BYTES,
            "log exceeded cap: {}",
            meta.len()
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("truncated"), "{content:?}");
    }

    #[tokio::test]
    async fn drain_survives_invalid_utf8_and_keeps_reading() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let capture = spawn_drain(reader, None);
        writer
            .write_all(b"before \xff\xfe bytes\r\n")
            .await
            .unwrap();
        writer.write_all(b"after\n").await.unwrap();
        drop(writer);
        capture.tail().wait_eof(Duration::from_secs(5)).await;
        let lines = capture.tail().lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("before "), "{lines:?}");
        assert_eq!(lines[1], "after");
    }

    #[tokio::test]
    async fn eof_wakes_a_waiter_that_registered_before_finish() {
        let tail = SharedTail::new();
        let waiter = tail.clone();
        let task = tokio::spawn(async move {
            let start = std::time::Instant::now();
            waiter.wait_eof(Duration::from_secs(5)).await;
            start.elapsed()
        });
        tokio::task::yield_now().await;
        tail.finish();
        assert!(task.await.unwrap() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn drain_feeds_tail_and_log_then_signals_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.stderr.log");
        let (mut tx, rx) = tokio::io::duplex(4096);
        let capture = spawn_drain(rx, Some(path.clone()));

        tx.write_all(b"first\nsecond\nthird\n").await.unwrap();
        drop(tx); // close the pipe → drain EOFs
        capture.tail().wait_eof(Duration::from_secs(2)).await;

        assert_eq!(capture.tail().lines(), vec!["first", "second", "third"]);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "first\nsecond\nthird\n");
    }
}
