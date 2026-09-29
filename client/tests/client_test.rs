//! Integration tests for `agentmux-client`: the SDK exercised against the
//! real `agentmux-server` binary spawned on tempdir sockets — the real
//! `~/.local/share/agentmux` paths are never touched.
//!
//! Covers the Task-12 brief:
//!
//! ① `DaemonClient::connect()` on a missing socket auto-spawns the daemon
//!   (`agentmux-server --daemon`, located via `AGENTMUX_SERVER_BIN`), then
//!   `server/status` succeeds and `call("agent/list")` returns the
//!   built-in 4 agents.
//! ② The typed wrappers round-trip the full
//!   project → workspace → session → prompt → kill → remove path.
//! ③ `subscribe_events()` streams `session/event` notifications while a
//!   `session/prompt` runs against the real mock agent; a second
//!   `events()` receiver observes the same broadcast.
//! ④ A JSON-RPC error maps onto `ClientError::Rpc { code, message }`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agentmux_client::{AgentId, ClientError, DaemonClient, Event, EventKind, SessionState};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio_stream::StreamExt;

/// Generous deadline; the mock replies instantly, so this only bites on
/// regression.
const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(25);

/// `std::env::set_var` is process-global; only the auto-start test mutates
/// env, but hold this async lock anyway so a second env-mutating test can
/// never race it.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("client/ sits directly under the workspace root")
        .to_path_buf()
}

fn target_debug_dir() -> PathBuf {
    let root = workspace_root();
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|p| if p.is_absolute() { p } else { root.join(p) })
        .unwrap_or_else(|| root.join("target"))
        .join("debug")
}

/// Build a workspace binary package once per test process and return the
/// produced debug binary. Same pattern as `server/tests`'s
/// `mock_agent_binary` — `CARGO_BIN_EXE_*` cannot cross packages.
fn built_binary(package: &str) -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", package])
        .current_dir(workspace_root())
        .status()
        .expect("failed to invoke cargo build");
    assert!(status.success(), "`cargo build -p {package}` failed");
    let binary = target_debug_dir().join(package);
    assert!(
        binary.is_file(),
        "{package} binary missing at {}",
        binary.display()
    );
    binary
}

fn server_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| built_binary("agentmux-server"))
        .clone()
}

fn mock_agent_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| built_binary("agentmux-mock-agent"))
        .clone()
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("failed to spawn git");
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

/// Throwaway git repo with one commit on `main`.
fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.email", "agentmux-test@example.com"]);
    git(repo, &["config", "user.name", "agentmux test"]);
    std::fs::write(repo.join("README.md"), "# test repo\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "init"]);
    dir
}

/// Config TOML registering the real mock binary under the `mock` id.
/// `agent/list` on this daemon returns the 4 built-ins + `mock`.
fn mock_config() -> String {
    format!(
        "[[agents]]\nid = \"mock\"\ncommand = \"{}\"\n",
        mock_agent_binary().display()
    )
}

/// An `agentmux-server --serve` subprocess on a tempdir socket.
struct TestDaemon {
    sock: PathBuf,
    child: Child,
    /// The git repo sessions are created against.
    repo: TempDir,
    /// Data dir (store + session logs) — kept alive until drop.
    _data: TempDir,
}

/// Spawn the real server binary in the foreground on `<data>/test.sock`,
/// all paths passed as flags so the ambient `AGENTMUX_*` env vars cannot
/// influence it.
fn spawn_daemon(config_toml: Option<&str>) -> TestDaemon {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();
    let sock = data.path().join("test.sock");
    let config_path = data.path().join("config.toml");
    if let Some(toml) = config_toml {
        std::fs::write(&config_path, toml).unwrap();
    }

    let child = Command::new(server_binary())
        .arg("--serve")
        .arg("--socket")
        .arg(&sock)
        .arg("--data-dir")
        .arg(data.path())
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn agentmux-server --serve");

    let mut td = TestDaemon {
        sock,
        child,
        repo,
        _data: data,
    };
    td.wait_ready();
    td
}

fn socket_accepts(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

impl TestDaemon {
    /// Poll until the daemon accepts connections (or exits early).
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if socket_accepts(&self.sock) {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("daemon exited during startup: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not start listening on {}",
                self.sock.display()
            );
            std::thread::sleep(POLL);
        }
    }

    /// After `server/shutdown`: reap the process and confirm the socket
    /// file is gone.
    fn wait_exited(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not exit after server/shutdown"
            );
            std::thread::sleep(POLL);
        }
        let deadline = Instant::now() + TIMEOUT;
        while self.sock.exists() && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        assert!(
            !self.sock.exists(),
            "socket file should be unlinked on shutdown"
        );
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// RAII restore for `std::env::set_var` mutations.
struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn set(vars: &[(&'static str, &OsStr)]) -> EnvGuard {
        let saved = vars
            .iter()
            .map(|(key, value)| {
                let prev = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, prev)
            })
            .collect();
        EnvGuard(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in &self.0 {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Whether `e` is an `agent_message_chunk` update carrying `text` for
/// `session_id`.
fn is_chunk(e: &Event, session_id: agentmux_core::SessionId, text: &str) -> bool {
    e.session_id == session_id
        && matches!(&e.kind, EventKind::SessionUpdate(v)
            if v["update"]["sessionUpdate"] == "agent_message_chunk"
                && v["update"]["content"]["text"] == text)
}

/// Whether `e` is the mock's `tool_call` update — the last update of a
/// normal turn.
fn is_tool_call(e: &Event, session_id: agentmux_core::SessionId) -> bool {
    e.session_id == session_id
        && matches!(&e.kind, EventKind::SessionUpdate(v)
            if v["update"]["sessionUpdate"] == "tool_call")
}

/// ① The brief's core scenario: with no socket file present,
/// `DaemonClient::connect()` auto-spawns the daemon and serves requests —
/// `server/status` answers and `call("agent/list")` returns the built-in
/// 4 agents (no config file was written).
#[tokio::test]
async fn connect_auto_starts_daemon_and_serves_requests() {
    let _env_guard = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agentmux.sock");
    let data = dir.path().join("data");
    // Never written — Config::load falls back to the built-in agents.
    let config = dir.path().join("config.toml");
    let _env = EnvGuard::set(&[
        ("AGENTMUX_SOCK", sock.as_os_str()),
        ("AGENTMUX_DATA_DIR", data.as_os_str()),
        ("AGENTMUX_CONFIG", config.as_os_str()),
        ("AGENTMUX_SERVER_BIN", server_binary().as_os_str()),
    ]);

    assert!(
        !sock.exists() && !socket_accepts(&sock),
        "precondition: no daemon on {}",
        sock.display()
    );

    let mut client = DaemonClient::connect()
        .await
        .expect("connect should auto-spawn the daemon");

    let status = client.server_status().await.expect("server/status failed");
    assert!(!status.version.is_empty());
    assert_eq!(status.sessions, 0);

    // Raw `call` path: agent/list returns the 4 built-ins.
    let result = client
        .call("agent/list", Value::Null)
        .await
        .expect("agent/list failed");
    let agents = result["agents"].as_array().expect("agents array");
    assert_eq!(
        agents.len(),
        4,
        "expected the 4 built-in agents: {agents:?}"
    );
    for id in ["claude-code", "codex", "opencode", "pi"] {
        assert!(
            agents.iter().any(|a| a["id"] == json!(id)),
            "built-in agent {id} missing: {agents:?}"
        );
    }

    // `ensure_daemon` on a live socket is a cheap no-op (no respawn).
    DaemonClient::ensure_daemon().expect("ensure_daemon on a live socket");

    client.shutdown().await.expect("server/shutdown failed");

    // The detached daemon exits and unlinks its socket.
    let deadline = Instant::now() + TIMEOUT;
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(!sock.exists(), "socket file should be unlinked on shutdown");
}

/// ② Typed wrappers: register → create → prompt → kill → remove, plus
/// list/status calls — all over the real daemon.
#[tokio::test]
async fn typed_methods_roundtrip_against_real_daemon() {
    let mut td = spawn_daemon(Some(&mock_config()));
    let mut client = DaemonClient::connect_to(&td.sock)
        .await
        .expect("connect_to a live daemon");

    let status = client.server_status().await.unwrap();
    assert!(!status.version.is_empty());
    assert_eq!(status.sessions, 0);

    let project = client
        .register_project(td.repo.path(), None)
        .await
        .expect("register_project");
    assert_eq!(project.root_path, td.repo.path());
    let projects = client.list_projects().await.unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, project.id);

    let workspace = client
        .create_workspace(project.id, "ws1", Some("main"))
        .await
        .expect("create_workspace");
    assert_eq!(workspace.project_id, project.id);
    assert_eq!(workspace.name, "ws1");
    assert_eq!(workspace.branch, "agentmux/ws1");
    let workspaces = client.list_workspaces(project.id).await.unwrap();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, workspace.id);

    let session = client
        .create_session(workspace.id, AgentId::new("mock"), None)
        .await
        .expect("create_session with the mock agent");
    assert_eq!(session.workspace_id, workspace.id);
    assert_eq!(session.agent_id, AgentId::new("mock"));
    assert_eq!(session.state, SessionState::Ready);
    assert!(client
        .list_sessions(workspace.id)
        .await
        .unwrap()
        .iter()
        .any(|s| s.id == session.id));

    // A turn round-trips: `prompt` resolves once the turn ends.
    client.prompt(session.id, "hi", vec![]).await.unwrap();
    let after = client
        .list_sessions(workspace.id)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert_eq!(after.state, SessionState::Ready);

    client.kill(session.id).await.expect("kill");
    let killed = client
        .list_sessions(workspace.id)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.id == session.id)
        .unwrap();
    assert_eq!(killed.state, SessionState::Done);

    let agents = client.list_agents().await.unwrap();
    assert_eq!(agents.len(), 5, "4 built-ins + mock: {agents:?}");
    let mock = agents
        .iter()
        .find(|a| a.id == AgentId::new("mock"))
        .expect("mock profile");
    assert!(mock.available, "mock binary should probe available");

    // Terminal sessions don't block workspace removal.
    assert!(client.remove_workspace(workspace.id).await.unwrap());
    assert!(client.remove_project(project.id).await.unwrap());

    client.shutdown().await.unwrap();
    td.wait_exited();
}

/// ③ `subscribe_events()` streams `session/event` notifications for a
/// live prompt turn; a second `events()` receiver sees the same events
/// (broadcast semantics).
#[tokio::test]
async fn subscribe_events_streams_the_prompt_turn() {
    let td = spawn_daemon(Some(&mock_config()));
    let mut client = DaemonClient::connect_to(&td.sock).await.unwrap();

    let events = client
        .subscribe_events()
        .await
        .expect("subscribe_events should ack");
    tokio::pin!(events);

    let project = client.register_project(td.repo.path(), None).await.unwrap();
    let workspace = client
        .create_workspace(project.id, "ws1", Some("main"))
        .await
        .unwrap();
    let session = client
        .create_session(workspace.id, AgentId::new("mock"), None)
        .await
        .unwrap();
    client.prompt(session.id, "hi", vec![]).await.unwrap();

    // The turn's updates arrive on the event stream — the echo plus the
    // closing tool_call.
    let mut saw_echo = false;
    let mut saw_tool_call = false;
    let deadline = Instant::now() + TIMEOUT;
    while !(saw_echo && saw_tool_call) {
        let ev = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            events.next(),
        )
        .await
        .expect("timed out waiting for a session/event notification")
        .expect("event stream closed while the daemon is alive");
        saw_echo |= is_chunk(&ev, session.id, "mock reply: hi");
        saw_tool_call |= is_tool_call(&ev, session.id);
        assert!(ev.seq > 0, "orchestrator-assigned seqs are nonzero: {ev:?}");
    }

    // A second `events()` receiver gets the same broadcast for the next turn.
    let mut second = client.events();
    client.prompt(session.id, "again", vec![]).await.unwrap();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let ev = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            second.recv(),
        )
        .await
        .expect("timed out on the second events() receiver")
        .expect("event broadcast closed while the daemon is alive");
        if is_chunk(&ev, session.id, "mock reply: again") {
            break;
        }
    }

    client.shutdown().await.unwrap();
    let mut td = td;
    td.wait_exited();
}

/// ④ A JSON-RPC `error` response maps onto `ClientError::Rpc`, not a
/// generic transport failure.
#[tokio::test]
async fn rpc_error_maps_to_typed_client_error() {
    let td = spawn_daemon(None);
    let mut client = DaemonClient::connect_to(&td.sock).await.unwrap();

    let err = client
        .call("no/such-method", Value::Null)
        .await
        .expect_err("unknown method should error");
    match err {
        ClientError::Rpc { code, message } => {
            assert_eq!(code, -32601, "method-not-found code: {message}");
            assert!(message.contains("no/such-method"), "{message}");
        }
        other => panic!("expected ClientError::Rpc, got {other:?}"),
    }

    // The connection is still usable after an error response.
    client.server_status().await.unwrap();
    client.shutdown().await.unwrap();
    let mut td = td;
    td.wait_exited();
}

/// ⑤ Liveness flag: `is_closed()` is false on a live connection and
/// flips true once the peer is gone (reader task hits EOF). The TUI
/// polls this to detect daemon disconnects — the event broadcast never
/// yields `None` while a `DaemonClient` holds the sender.
#[tokio::test]
async fn is_closed_flips_when_the_peer_drops() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("peer.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();

    let mut client = DaemonClient::connect_existing(&sock).await.unwrap();
    assert!(!client.is_closed(), "fresh connection should be live");
    let (server_side, _) = listener.accept().await.unwrap();

    drop(server_side); // peer EOF
    let deadline = Instant::now() + TIMEOUT;
    while !client.is_closed() {
        assert!(
            Instant::now() < deadline,
            "reader never noticed the dropped peer"
        );
        tokio::time::sleep(POLL).await;
    }

    // A call on the dead socket fails fast instead of hanging.
    let err = client.server_status().await.unwrap_err();
    assert!(matches!(err, ClientError::Transport(_)), "{err}");
}

/// ⑥ `respond_permission`: the `perm` trigger parks the turn on a
/// `PermissionRequest` event; a second client connection answers it and
/// the prompt completes — the typed wrapper round-trips the real RPC.
#[tokio::test]
async fn respond_permission_unparks_a_parked_turn() {
    let td = spawn_daemon(Some(&mock_config()));
    let mut prompter = DaemonClient::connect_to(&td.sock).await.unwrap();
    let mut answerer = DaemonClient::connect_to(&td.sock).await.unwrap();

    // The answerer subscribes: it watches for the PermissionRequest.
    let events = answerer
        .subscribe_events()
        .await
        .expect("subscribe_events should ack");
    tokio::pin!(events);

    let project = prompter
        .register_project(td.repo.path(), None)
        .await
        .unwrap();
    let workspace = prompter
        .create_workspace(project.id, "ws1", Some("main"))
        .await
        .unwrap();
    let session = prompter
        .create_session(workspace.id, AgentId::new("mock"), None)
        .await
        .unwrap();

    // The prompt parks mid-turn; move the client into the task and get
    // it back when the turn ends.
    let turn = tokio::spawn(async move {
        let r = prompter.prompt(session.id, "perm", vec![]).await;
        (prompter, r)
    });

    let request_id = loop {
        let ev = tokio::time::timeout(TIMEOUT, events.next())
            .await
            .expect("timed out waiting for PermissionRequest")
            .expect("event stream ended while the daemon is alive");
        if let EventKind::PermissionRequest { request_id, .. } = &ev.kind {
            assert_eq!(ev.session_id, session.id);
            break request_id.clone();
        }
    };

    answerer
        .respond_permission(session.id, &request_id, agentmux_client::PermissionDecision::AllowOnce)
        .await
        .expect("respond_permission should ack");

    let (prompter, result) = turn.await.expect("prompt task panicked");
    result.expect("prompt should complete once answered");

    // PermissionResolved and the mock's outcome chunk land on the stream.
    let mut saw_resolved = false;
    let mut saw_outcome = false;
    let deadline = Instant::now() + TIMEOUT;
    while !(saw_resolved && saw_outcome) {
        let ev = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            events.next(),
        )
        .await
        .expect("timed out waiting for the resolved events")
        .expect("event stream ended while the daemon is alive");
        if ev.session_id != session.id {
            continue;
        }
        saw_resolved |= matches!(
            &ev.kind,
            EventKind::PermissionResolved { request_id: r, outcome }
                if *r == request_id && outcome == "selected:allow"
        );
        saw_outcome |= is_chunk(&ev, session.id, "permission outcome: selected:allow");
    }
    drop(prompter);

    // The answering connection is still usable afterwards.
    answerer.shutdown().await.unwrap();
    let mut td = td;
    td.wait_exited();
}
