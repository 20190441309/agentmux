//! Integration tests for `agentmux-server`: the unix-socket JSON-RPC
//! daemon fronting [`Orchestrator`], driven end-to-end over real sockets
//! with the real `agentmux-mock-agent` binary.
//!
//! Covers the four scenarios from the Task 11 brief:
//!
//! ① `server/status` returns ok with version/uptime/sessions.
//! ② A non-JSON line → `-32700` parse error; the connection stays open.
//! ③ `session/subscribe` → `project/register` → `workspace/create` →
//!   `session/create` → `session/prompt` streams `session/event`
//!   notifications (the mock's echo + tool_call) to the connection.
//! ④ A second subscribed connection receives the same events.
//!
//! Every test runs the server on a socket inside a tempdir — the real
//! `~/.local/share/agentmux/agentmux.sock` is never touched — and shuts
//! it down via `server/shutdown`, which also exercises that path.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agentmux_core::rpc::{RpcRequest, N_SESSION_EVENT};
use agentmux_core::{
    AdapterKind, AgentId, AgentProfile, AgentRegistry, Config, Event, EventKind, Orchestrator,
    Store,
};
use agentmux_server::{bind_unix_listener, build_daemon, Daemon, ServerPaths};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

/// Generous deadline; the mock replies instantly, so this only bites on
/// regression.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Build `agentmux-mock-agent` once per test process and return the
/// produced debug binary's path. Same logic as `core/tests/common` —
/// `CARGO_BIN_EXE_*` cannot cross packages, so it is duplicated here.
fn mock_agent_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("server/ sits directly under the workspace root")
                .to_path_buf();
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "agentmux-mock-agent"])
                .current_dir(&workspace_root)
                .status()
                .expect("failed to invoke `cargo build -p agentmux-mock-agent`");
            assert!(
                status.success(),
                "`cargo build -p agentmux-mock-agent` failed"
            );
            let target_dir = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .map(|p| {
                    if p.is_absolute() {
                        p
                    } else {
                        workspace_root.join(p)
                    }
                })
                .unwrap_or_else(|| workspace_root.join("target"));
            let binary = target_dir.join("debug").join("agentmux-mock-agent");
            assert!(
                binary.is_file(),
                "mock agent binary missing at {}",
                binary.display()
            );
            binary
        })
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

/// The agent the daemon can actually spawn: the real mock binary.
fn mock_profile() -> AgentProfile {
    AgentProfile {
        id: AgentId::new("mock"),
        name: "Mock Agent".into(),
        adapter: AdapterKind::Acp {
            command: mock_agent_binary(),
            args: vec![],
        },
        env: Default::default(),
        available: true,
    }
}

/// A running daemon on a tempdir socket plus the fixtures keeping it alive.
struct TestDaemon {
    sock: PathBuf,
    serve: JoinHandle<agentmux_core::Result<()>>,
    /// agentmux data dir (store + session JSONL logs).
    _data: TempDir,
    /// Git repo hosting the worktrees.
    repo: TempDir,
}

/// Build an orchestrator seeded with the mock agent profile, wrap it in a
/// [`Daemon`], and serve it on `<data>/test.sock`.
async fn start_daemon() -> TestDaemon {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();

    let store = Store::open(data.path()).unwrap();
    let cfg = Config {
        agents: vec![mock_profile()],
        ..Config::default()
    };
    let orch = Orchestrator::new(
        store,
        AgentRegistry::from_config(&cfg),
        data.path().to_path_buf(),
    );

    let daemon = Daemon::new(orch);
    let sock = data.path().join("test.sock");
    let listener = bind_unix_listener(&sock)
        .await
        .expect("bind should succeed");
    let serve = {
        let daemon = daemon.clone();
        tokio::spawn(async move { daemon.serve(listener).await })
    };

    TestDaemon {
        sock,
        serve,
        _data: data,
        repo,
    }
}

/// A newline-delimited JSON-RPC client connection.
///
/// Notifications interleave with responses on a subscribed connection, so
/// [`request`](Client::request) stashes any `session/event` notifications
/// it reads past into `events`; [`next_event`](Client::next_event) drains
/// that stash before reading the socket again.
struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
    /// `session/event` params seen while waiting for responses.
    events: VecDeque<Event>,
}

impl Client {
    async fn connect(sock: &Path) -> Client {
        let stream = UnixStream::connect(sock)
            .await
            .expect("connect to daemon socket");
        let (reader, writer) = stream.into_split();
        Client {
            reader: BufReader::new(reader),
            writer,
            next_id: 0,
            events: VecDeque::new(),
        }
    }

    /// Write one raw line verbatim (a `\n` is appended if missing).
    async fn send_line(&mut self, text: &str) {
        let mut line = text.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("write to daemon socket");
        self.writer.flush().await.unwrap();
    }

    /// Read one line and return it parsed as a [`Value`].
    async fn read_msg(&mut self) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(TIMEOUT, self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a line from the daemon")
            .expect("read from daemon socket");
        assert!(n > 0, "daemon closed the connection unexpectedly");
        serde_json::from_str(&line).expect("daemon sent non-JSON line")
    }

    /// Read lines until the response carrying `id` arrives; any
    /// `session/event` notifications read along the way are stashed.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = RpcRequest::new(method, params, json!(id));
        self.send_line(&serde_json::to_string(&req).unwrap()).await;
        loop {
            let msg = self.read_msg().await;
            if msg.get("id") == Some(&json!(id)) {
                return msg;
            }
            self.stash_notification(&msg);
        }
    }

    /// Stash `msg` if it is a `session/event` notification.
    fn stash_notification(&mut self, msg: &Value) {
        if msg.get("method") == Some(&json!(N_SESSION_EVENT)) {
            let ev: Event = serde_json::from_value(msg["params"].clone())
                .expect("session/event params should be an Event");
            self.events.push_back(ev);
        }
    }

    /// The next `session/event` notification — stashed ones first, then
    /// lines read fresh off the socket.
    async fn next_event(&mut self) -> Event {
        if let Some(ev) = self.events.pop_front() {
            return ev;
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a session/event notification"
            );
            let msg = self.read_msg().await;
            self.stash_notification(&msg);
            if let Some(ev) = self.events.pop_front() {
                return ev;
            }
        }
    }

    /// Collect events until `pred` matches one (stashed + socket).
    async fn collect_events_until(&mut self, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let ev = tokio::time::timeout(
                deadline.saturating_duration_since(Instant::now()),
                self.next_event(),
            )
            .await
            .expect("timed out collecting session/event notifications");
            let hit = pred(&ev);
            seen.push(ev);
            if hit {
                return seen;
            }
        }
    }
}

/// Whether `e` is an `agent_message_chunk` update for `session_id`;
/// returns the chunk text.
fn chunk_text(e: &Event, session_id: agentmux_core::SessionId) -> Option<&str> {
    if e.session_id != session_id {
        return None;
    }
    match &e.kind {
        EventKind::SessionUpdate(v) if v["update"]["sessionUpdate"] == "agent_message_chunk" => {
            v["update"]["content"]["text"].as_str()
        }
        _ => None,
    }
}

/// Whether `e` is the mock's `tool_call` update — the last update of a
/// normal turn.
fn is_tool_call_update(e: &Event, session_id: agentmux_core::SessionId) -> bool {
    e.session_id == session_id
        && matches!(&e.kind, EventKind::SessionUpdate(v)
            if v["update"]["sessionUpdate"] == "tool_call")
}

/// Drive the RPC path `project/register` → `workspace/create` →
/// `session/create` (mock agent) on `client`; returns the session id.
async fn create_session_via_rpc(client: &mut Client, repo: &Path, ws_name: &str) -> Value {
    let resp = client
        .request("project/register", json!({"root_path": repo}))
        .await;
    let project_id = resp["result"]["project"]["id"]
        .as_str()
        .expect("project/register should return a project id")
        .to_string();

    let resp = client
        .request(
            "workspace/create",
            json!({"project_id": project_id, "name": ws_name, "base": "main"}),
        )
        .await;
    let workspace_id = resp["result"]["workspace"]["id"]
        .as_str()
        .expect("workspace/create should return a workspace id")
        .to_string();

    let resp = client
        .request(
            "session/create",
            json!({"workspace_id": workspace_id, "agent_id": "mock"}),
        )
        .await;
    resp["result"]["session"]["id"].clone()
}

/// Ask the daemon to shut down and wait for the accept loop to exit.
async fn shutdown(td: TestDaemon) {
    let mut client = Client::connect(&td.sock).await;
    let resp = client.request("server/shutdown", Value::Null).await;
    assert_eq!(resp["result"], json!(null), "shutdown should ack: {resp}");
    td.serve
        .await
        .expect("serve task panicked")
        .expect("serve returned an error");
    assert!(
        !td.sock.exists(),
        "socket file should be unlinked on shutdown"
    );
}

/// ① `server/status` returns version/uptime/sessions.
#[tokio::test]
async fn server_status_returns_version_uptime_and_sessions() {
    let td = start_daemon().await;
    let mut client = Client::connect(&td.sock).await;

    let resp = client.request("server/status", Value::Null).await;
    let result = &resp["result"];
    assert_eq!(
        result["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "status should report the server crate version: {result}"
    );
    assert!(result["uptime_secs"].as_u64().is_some());
    assert_eq!(result["sessions"], json!(0));

    shutdown(td).await;
}

/// ② Malformed JSON → `-32700`, and the connection stays open.
#[tokio::test]
async fn malformed_json_gets_parse_error_and_connection_survives() {
    let td = start_daemon().await;
    let mut client = Client::connect(&td.sock).await;

    client.send_line("this is not json at all").await;
    let resp = client.read_msg().await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32700),
        "expected a parse error: {resp}"
    );
    assert_eq!(resp["id"], json!(null));

    // The connection is still usable.
    let resp = client.request("server/status", Value::Null).await;
    assert!(
        resp["result"].is_object(),
        "post-error request failed: {resp}"
    );

    shutdown(td).await;
}

/// ③ `session/subscribe` then the full RPC path → the connection receives
/// `session/event` notifications (state transitions + the mock's updates).
#[tokio::test]
async fn subscribed_connection_receives_session_event_stream() {
    let td = start_daemon().await;
    let mut client = Client::connect(&td.sock).await;

    // Subscribe first: the ack is a normal result, then events flow.
    let resp = client.request("session/subscribe", Value::Null).await;
    assert_eq!(resp["result"], json!(null), "subscribe should ack: {resp}");

    let session_id = create_session_via_rpc(&mut client, td.repo.path(), "ws1").await;
    let sid: agentmux_core::SessionId =
        serde_json::from_value(session_id.clone()).expect("session id");

    let resp = client
        .request(
            "session/prompt",
            json!({"session_id": session_id, "text": "hi"}),
        )
        .await;
    assert_eq!(
        resp["result"],
        json!(null),
        "prompt should complete: {resp}"
    );

    // The turn's updates arrived as notifications — the echo plus the
    // closing tool_call. Some may still be in flight after the response.
    let events = client
        .collect_events_until(|e| is_tool_call_update(e, sid))
        .await;
    assert!(
        events
            .iter()
            .any(|e| chunk_text(e, sid) == Some("mock reply: hi")),
        "expected the mock's echo among session events: {events:?}"
    );
    assert!(
        events.iter().all(|e| e.seq > 0),
        "orchestrator-assigned seqs should be nonzero: {events:?}"
    );

    shutdown(td).await;
}

/// ④ A second subscribed connection receives the same event stream.
#[tokio::test]
async fn second_subscribed_connection_receives_same_events() {
    let td = start_daemon().await;
    let mut client_a = Client::connect(&td.sock).await;
    let mut client_b = Client::connect(&td.sock).await;

    for c in [&mut client_a, &mut client_b] {
        let resp = c.request("session/subscribe", Value::Null).await;
        assert_eq!(resp["result"], json!(null));
    }

    let session_id = create_session_via_rpc(&mut client_a, td.repo.path(), "ws1").await;
    let sid: agentmux_core::SessionId =
        serde_json::from_value(session_id.clone()).expect("session id");
    let resp = client_a
        .request(
            "session/prompt",
            json!({"session_id": session_id, "text": "fanout"}),
        )
        .await;
    assert_eq!(resp["result"], json!(null));

    // Both connections see the turn's events.
    let a_events = client_a
        .collect_events_until(|e| is_tool_call_update(e, sid))
        .await;
    let b_events = client_b
        .collect_events_until(|e| is_tool_call_update(e, sid))
        .await;
    for (name, events) in [("A", a_events), ("B", b_events)] {
        assert!(
            events
                .iter()
                .any(|e| chunk_text(e, sid) == Some("mock reply: fanout")),
            "client {name} should see the echo: {events:?}"
        );
    }

    shutdown(td).await;
}

/// ⑤ Regression: `build_daemon` probes adapter availability at boot —
/// a config file alone (no `agent/register` call, no `agent/list`
/// write-back) must suffice for `session/create`. Config-loaded profiles
/// are always `available: false` until probed, so an unprobed registry
/// would refuse every spawn with "agent … is not available".
#[tokio::test]
async fn build_daemon_probes_agent_availability_so_sessions_can_create() {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();
    let sock = data.path().join("test.sock");

    // The mock agent exists only on disk — `available` is not a config
    // field and always deserializes to false.
    let config_path = data.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[[agents]]\nid = \"mock\"\ncommand = \"{}\"\n",
            mock_agent_binary().display()
        ),
    )
    .unwrap();

    let paths = ServerPaths {
        data_dir: data.path().to_path_buf(),
        socket_path: sock.clone(),
        config_path,
    };
    let daemon = build_daemon(&paths).expect("build_daemon");
    let listener = bind_unix_listener(&sock).await.unwrap();
    let serve = {
        let daemon = daemon.clone();
        tokio::spawn(async move { daemon.serve(listener).await })
    };
    let td = TestDaemon {
        sock,
        serve,
        _data: data,
        repo,
    };

    let mut client = Client::connect(&td.sock).await;

    // The real assertion, run *before* any `agent/list` — `list_agents`
    // writes probe results back into the registry, so listing first
    // would mask a missing boot probe. This would be "agent mock is not
    // available" on the unprobed registry.
    let session_id = create_session_via_rpc(&mut client, td.repo.path(), "ws1").await;
    assert!(
        session_id.is_string(),
        "session/create should have succeeded: {session_id}"
    );

    // `agent/list` (fresh probe) agrees with what `session/create` saw.
    let resp = client.request("agent/list", Value::Null).await;
    let mock = resp["result"]["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|p| p["id"] == json!("mock")))
        .expect("mock should be in agent/list");
    assert_eq!(
        mock["available"],
        json!(true),
        "boot probe should mark the mock available: {mock}"
    );

    shutdown(td).await;
}

/// ⑥ An inbound line that isn't valid UTF-8 gets a `-32700` parse error
/// and the connection stays open (it must not be silently dropped).
#[tokio::test]
async fn invalid_utf8_line_gets_parse_error_and_connection_survives() {
    let td = start_daemon().await;
    let mut client = Client::connect(&td.sock).await;

    client
        .writer
        .write_all(&[0x66, 0x6f, 0x80, b'\n']) // "fo" + invalid continuation byte
        .await
        .unwrap();
    client.writer.flush().await.unwrap();
    let resp = client.read_msg().await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32700),
        "expected a parse error: {resp}"
    );

    let resp = client.request("server/status", Value::Null).await;
    assert!(
        resp["result"].is_object(),
        "post-error request failed: {resp}"
    );

    shutdown(td).await;
}

/// ⑦ `session/permission`: a `perm` prompt parks on the mock's
/// `session/request_permission`. The parked request arrives on the
/// subscribed connection as a `PermissionRequest` event, a second
/// connection answers it, the prompt completes, and the paired
/// `PermissionResolved` + the mock's outcome chunk land on the bus.
#[tokio::test]
async fn session_permission_roundtrip_resolves_parked_prompt() {
    let td = start_daemon().await;
    let mut c1 = Client::connect(&td.sock).await;
    let resp = c1.request("session/subscribe", Value::Null).await;
    assert_eq!(resp["result"], json!(null), "subscribe should ack: {resp}");

    let session_id = create_session_via_rpc(&mut c1, td.repo.path(), "ws1").await;
    let sid: agentmux_core::SessionId =
        serde_json::from_value(session_id.clone()).expect("session id");

    // Sent as a raw line, not `request()`: the response only arrives
    // after the permission is answered, so the read loop must interleave
    // notifications below instead of blocking inside `request`.
    let prompt_id = json!(9001);
    c1.send_line(
        &serde_json::to_string(&RpcRequest::new(
            "session/prompt",
            json!({"session_id": session_id, "text": "perm please"}),
            prompt_id.clone(),
        ))
        .unwrap(),
    )
    .await;

    // Pump messages until the parked PermissionRequest notification lands.
    let request_id = loop {
        if let Some(id) = c1.events.iter().find_map(|e| match &e.kind {
            EventKind::PermissionRequest { request_id, .. } if e.session_id == sid => {
                Some(request_id.clone())
            }
            _ => None,
        }) {
            break id;
        }
        let msg = c1.read_msg().await;
        assert!(
            msg.get("id") != Some(&prompt_id),
            "prompt finished before its permission was answered: {msg}"
        );
        c1.stash_notification(&msg);
    };

    // A second connection answers — the first is still mid-prompt.
    let mut c2 = Client::connect(&td.sock).await;
    let resp = c2
        .request(
            "session/permission",
            json!({
                "session_id": session_id,
                "request_id": request_id,
                "outcome": "allow_once",
            }),
        )
        .await;
    assert_eq!(
        resp["result"],
        json!(null),
        "session/permission should ack: {resp}"
    );

    // The turn resumes: the prompt response lands, then the paired
    // PermissionResolved and the mock's own outcome chunk.
    let resp = loop {
        let msg = c1.read_msg().await;
        if msg.get("id") == Some(&prompt_id) {
            break msg;
        }
        c1.stash_notification(&msg);
    };
    assert_eq!(
        resp["result"],
        json!(null),
        "prompt should complete once answered: {resp}"
    );

    let events = c1
        .collect_events_until(|e| chunk_text(e, sid) == Some("permission outcome: selected:allow"))
        .await;
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            EventKind::PermissionResolved { request_id: r, outcome }
                if *r == request_id && outcome == "selected:allow"
        )),
        "expected a paired PermissionResolved: {events:?}"
    );

    shutdown(td).await;
}

/// ⑧ `session/permission` param validation: missing fields and a bogus
/// outcome string are `-32602`; a well-formed call on an unknown session
/// is the orchestrator's `-32603`.
#[tokio::test]
async fn session_permission_validates_params() {
    let td = start_daemon().await;
    let mut client = Client::connect(&td.sock).await;

    let resp = client
        .request(
            "session/permission",
            json!({"session_id": agentmux_core::SessionId::new()}),
        )
        .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32602),
        "missing request_id/outcome should be invalid params: {resp}"
    );

    let resp = client
        .request(
            "session/permission",
            json!({
                "session_id": agentmux_core::SessionId::new(),
                "request_id": "req-1",
                "outcome": "shrug",
            }),
        )
        .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32602),
        "an unknown outcome string should be invalid params: {resp}"
    );

    let resp = client
        .request(
            "session/permission",
            json!({
                "session_id": agentmux_core::SessionId::new(),
                "request_id": "req-1",
                "outcome": "allow_once",
            }),
        )
        .await;
    assert_eq!(
        resp["error"]["code"],
        json!(-32603),
        "a well-formed call on an unknown session should be internal: {resp}"
    );

    // The connection is still usable.
    let resp = client.request("server/status", Value::Null).await;
    assert!(
        resp["result"].is_object(),
        "post-error request failed: {resp}"
    );

    shutdown(td).await;
}
