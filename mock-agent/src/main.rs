//! `agentmux-mock-agent` — a deterministic ACP agent for agentmux
//! integration tests (consumed by Tasks 5/9/11/13 and by
//! `core/tests/acp_e2e_test.rs`).
//!
//! It speaks the Agent Client Protocol over stdio, following the
//! server-side pattern from `agent-client-protocol`'s `examples/agent.rs`:
//! an [`acp::AgentSideConnection`] on a `current_thread` runtime +
//! `LocalSet` (the crate's io futures are `!Send`), with session
//! notifications funnelled through an mpsc channel to a background task
//! that owns the connection handle.
//!
//! # Behaviour contract
//!
//! - `initialize` → normal `InitializeResponse` (`agentInfo.name` is
//!   `"agentmux-mock-agent"`).
//! - `session/new` → fixed session id [`SESSION_ID`].
//! - `session/prompt` → sends two `session/update` notifications, in
//!   order:
//!     1. `agent_message_chunk` whose text is `"mock reply: <prompt text>"`
//!        (the echo lets tests correlate a turn with its prompt),
//!     2. `tool_call` with `kind = "edit"` and location `src/lib.rs`,
//!        then responds with `stop_reason = "end_turn"`.
//! - Prompt text containing the token `crash` → `std::process::exit(1)`.
//! - Prompt text containing the token `exit42` → `std::process::exit(42)`
//!   (distinct code for `AgentExited` coverage).
//! - Prompt text containing the token `noisy` → writes 15 stderr lines
//!   (`mock stderr line 1..15`) then `std::process::exit(3)` — stderr
//!   capture + tail coverage (more lines than the retained tail, so
//!   truncation is exercised too).
//! - Prompt text containing the token `hang` → the prompt future never
//!   resolves (for timeout/cancel tests); the agent process stays alive.
//! - Prompt text containing the token `perm` → between steps 1 and 2 the
//!   agent issues `session/request_permission` (options: allow-once,
//!   allow-always, reject-once), waits for the answer, then reports it
//!   as an `agent_message_chunk` reading `"permission outcome:
//!   selected:<option-id>"` or `"permission outcome: cancelled"` —
//!   letting integration tests drive the whole approval loop.
//!
//! Trigger matching is by whole token (see [`has_trigger`]), not raw
//! substring — so "changed" doesn't accidentally mean "hang".

use std::cell::Cell;

use agent_client_protocol::{self as acp, Client as _};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

/// Fixed session id returned by every `session/new` — deterministic so
/// tests can hard-assert on it.
const SESSION_ID: &str = "mock-session-1";

/// Channel carrying `session/update` notifications from [`MockAgent`] to
/// the connection task, each paired with a oneshot that resolves once the
/// notification has been written to the wire. Awaiting that oneshot keeps
/// update ordering strict and guarantees updates precede the prompt
/// response.
type UpdateTx = mpsc::UnboundedSender<(acp::SessionNotification, oneshot::Sender<()>)>;

/// Channel carrying `session/request_permission` calls to the task that
/// owns the `AgentSideConnection` (the handle isn't `Clone`, so requests
/// must be funnelled to it). The oneshot returns the client's response.
type PermTx = mpsc::UnboundedSender<(
    acp::RequestPermissionRequest,
    oneshot::Sender<Result<acp::RequestPermissionResponse, acp::Error>>,
)>;

struct MockAgent {
    session_update_tx: UpdateTx,
    permission_tx: PermTx,
    /// Monotonic ids for the tool calls we report.
    next_tool_call_id: Cell<u64>,
}

impl MockAgent {
    async fn send_commands(&self, session_id: &acp::SessionId) -> Result<(), acp::Error> {
        let Ok(names) = std::env::var("MOCK_COMMANDS") else {
            return Ok(());
        };
        let commands = names
            .split(',')
            .filter(|name| !name.is_empty())
            .map(|name| {
                acp::AvailableCommand::new(name, format!("Mock {name}")).input(
                    acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                        "[arguments]",
                    )),
                )
            })
            .collect();
        self.send_update(
            session_id,
            acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                commands,
            )),
        )
        .await
    }

    /// Queue one `session/update` and wait until the connection task has
    /// flushed it. Errors when the connection is gone map to
    /// `internal_error`, failing the in-flight prompt.
    async fn send_update(
        &self,
        session_id: &acp::SessionId,
        update: acp::SessionUpdate,
    ) -> Result<(), acp::Error> {
        let (tx, rx) = oneshot::channel();
        self.session_update_tx
            .send((
                acp::SessionNotification::new(session_id.clone(), update),
                tx,
            ))
            .map_err(|_| acp::Error::internal_error())?;
        rx.await.map_err(|_| acp::Error::internal_error())
    }

    /// Issue `session/request_permission` for the turn's tool call and
    /// return a textual outcome (`"selected:<option-id>"` / `"cancelled"`)
    /// the prompt then reports as an agent message chunk.
    async fn ask_permission(&self, session_id: &acp::SessionId) -> Result<String, acp::Error> {
        let request = acp::RequestPermissionRequest::new(
            session_id.clone(),
            acp::ToolCallUpdate::new(
                format!("mock-tool-call-{}", self.next_tool_call_id.get()),
                acp::ToolCallUpdateFields::new()
                    .title("mock edit of src/lib.rs".to_string())
                    .kind(acp::ToolKind::Edit)
                    .locations(vec![acp::ToolCallLocation::new("src/lib.rs")]),
            ),
            vec![
                acp::PermissionOption::new(
                    "allow",
                    "Allow once",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    "always",
                    "Always allow",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                acp::PermissionOption::new("deny", "Reject", acp::PermissionOptionKind::RejectOnce),
            ],
        );
        let (tx, rx) = oneshot::channel();
        self.permission_tx
            .send((request, tx))
            .map_err(|_| acp::Error::internal_error())?;
        let resp = rx.await.map_err(|_| acp::Error::internal_error())??;
        Ok(match resp.outcome {
            acp::RequestPermissionOutcome::Selected(sel) => {
                format!("selected:{}", sel.option_id)
            }
            _ => "cancelled".to_string(),
        })
    }
}

/// Whether `text` contains `word` as a whole token (bounded by
/// non-alphanumeric characters).
///
/// Triggers are matched by token, not substring: prose like "changed" or
/// "exchanging" must not trip the `hang` trigger — e.g. the
/// orchestrator's shared-context preamble legitimately contains both.
fn has_trigger(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|w| w == word)
}

/// Flatten the prompt's text blocks into one string for the trigger
/// checks and the echo reply.
fn prompt_text(prompt: &[acp::ContentBlock]) -> String {
    let mut text = String::new();
    for block in prompt {
        if let acp::ContentBlock::Text(t) = block {
            text.push_str(&t.text);
        }
    }
    text
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for MockAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        Ok(acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(
                acp::AgentCapabilities::new()
                    .load_session(std::env::var_os("MOCK_NO_LOAD").is_none()),
            )
            .agent_info(acp::Implementation::new(
                "agentmux-mock-agent",
                env!("CARGO_PKG_VERSION"),
            )))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        if std::env::var_os("MOCK_FORBID_NEW").is_some() {
            return Err(acp::Error::invalid_params());
        }
        self.send_commands(&acp::SessionId::new(SESSION_ID)).await?;
        Ok(acp::NewSessionResponse::new(SESSION_ID))
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        if std::env::var_os("MOCK_NO_LOAD").is_some()
            || std::env::var_os("MOCK_LOAD_FAIL").is_some()
            || args.session_id.to_string() != SESSION_ID
        {
            return Err(acp::Error::invalid_params());
        }
        self.send_commands(&args.session_id).await?;
        let replay_count = std::env::var("MOCK_LOAD_REPLAY_COUNT")
            .ok()
            .and_then(|count| count.parse::<usize>().ok())
            .unwrap_or(1);
        for _ in 0..replay_count {
            self.send_update(
                &args.session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::from("mock replay sentinel"),
                )),
            )
            .await?;
        }
        Ok(acp::LoadSessionResponse::new())
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let text = prompt_text(&args.prompt);

        // Trigger words first — a crashing/hung agent sends no updates.
        if has_trigger(&text, "crash") {
            std::process::exit(1);
        }
        if has_trigger(&text, "exit42") {
            std::process::exit(42);
        }
        if has_trigger(&text, "noisy") {
            // 15 > the 12-line stderr tail, so only the freshest survive.
            for i in 1..=15 {
                eprintln!("mock stderr line {i}");
            }
            std::process::exit(3);
        }
        if has_trigger(&text, "hang") {
            // Never resolves: the request stays pending until the client
            // gives up or kills us. Cancel cannot unwedge it, which is
            // exactly what the timeout tests exercise.
            std::future::pending::<()>().await;
        }

        // 1) Echo the prompt back as an agent message chunk.
        self.send_update(
            &args.session_id,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::from(
                format!("mock reply: {text}"),
            ))),
        )
        .await?;

        // The `perm` trigger parks the turn on `session/request_permission`
        // and reports what the client answered.
        if has_trigger(&text, "perm") {
            let outcome = self.ask_permission(&args.session_id).await?;
            self.send_update(
                &args.session_id,
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::from(format!("permission outcome: {outcome}")),
                )),
            )
            .await?;
        }

        // 2) A completed edit tool call on src/lib.rs.
        let n = self.next_tool_call_id.get();
        self.next_tool_call_id.set(n + 1);
        let tool_call =
            acp::ToolCall::new(format!("mock-tool-call-{n}"), "mock edit of src/lib.rs")
                .kind(acp::ToolKind::Edit)
                .status(acp::ToolCallStatus::Completed)
                .locations(vec![acp::ToolCallLocation::new("src/lib.rs")]);
        self.send_update(&args.session_id, acp::SessionUpdate::ToolCall(tool_call))
            .await?;

        Ok(acp::PromptResponse::new(acp::StopReason::EndTurn))
    }

    async fn cancel(&self, _args: acp::CancelNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> acp::Result<()> {
    let outgoing = tokio::io::stdout().compat_write();
    let incoming = tokio::io::stdin().compat();

    // The crate's io futures are `!Send`; everything runs on this
    // LocalSet, mirroring examples/agent.rs.
    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (perm_tx, mut perm_rx) = mpsc::unbounded_channel();
            let agent = MockAgent {
                session_update_tx: tx,
                permission_tx: perm_tx,
                next_tool_call_id: Cell::new(0),
            };
            let (conn, handle_io) =
                acp::AgentSideConnection::new(agent, outgoing, incoming, |fut| {
                    tokio::task::spawn_local(fut);
                });
            // Flush queued session notifications in send order and serve
            // permission requests; each sender waits on its oneshot, so
            // order is preserved. A parked `request_permission` naturally
            // holds the queue — the agent sends nothing while it waits.
            tokio::task::spawn_local(async move {
                loop {
                    tokio::select! {
                        item = rx.recv() => match item {
                            Some((notification, ack)) => {
                                if conn.session_notification(notification).await.is_err() {
                                    // Connection is gone — the pending
                                    // prompt fails on the dropped oneshot.
                                    return;
                                }
                                let _ = ack.send(());
                            }
                            None => return,
                        },
                        item = perm_rx.recv() => match item {
                            Some((request, reply)) => {
                                let _ = reply.send(conn.request_permission(request).await);
                            }
                            None => return,
                        },
                    }
                }
            });
            // Serve until stdin closes (client dropped or killed us).
            handle_io.await
        })
        .await
}
