//! `agentmux-tui` — ratatui front-end for the agentmux daemon.
//!
//! Pure client: connects to the daemon socket (auto-starting it when
//! needed), subscribes to the session event stream, loads an initial
//! snapshot (projects → workspaces → sessions + agents), then runs a
//! `tokio::select!` loop merging daemon events, terminal input and
//! results from spawned daemon calls.
//!
//! `App` is deliberately UI-state-only: keys become [`AppAction`]s, and
//! every daemon call happens here — on a *fresh* connection per action
//! — so a `session/prompt` running for a whole turn never wedges the
//! UI and `ctrl-c` can still `session/cancel` it.

mod app;
mod input;
mod newsession;
mod theme;
mod ui;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use agentmux_client::DaemonClient;
use agentmux_core::{AgentId, Event, SessionId, SessionRef, Workspace};
use crossterm::event::{Event as TermEvent, EventStream};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

use crate::app::{App, AppAction, PendingRelay, SessionView};
use crate::newsession::WorkspacePick;

/// Result of a spawned daemon call, reported back to the UI loop.
enum UiMsg {
    /// Something to show in the status bar (success note or error).
    Status(String),
    /// `session/create` succeeded — insert into the list + select it.
    /// `new_workspace` is the workspace the wizard just created, so the
    /// session groups under its header even before a reload. Boxed:
    /// `SessionView` dwarfs the `Status` variant.
    Created {
        view: Box<SessionView>,
        new_workspace: Option<Workspace>,
    },
}

/// A daemon operation the loop can run on a background task.
enum DaemonCall {
    /// `session/prompt` — blocks until the turn finishes.
    Prompt {
        session_id: SessionId,
        text: String,
        references: Vec<SessionRef>,
    },
    /// `workspace/create` (for [`WorkspacePick::New`]) then
    /// `session/create`.
    Create {
        workspace: WorkspacePick,
        agent_id: AgentId,
        agent_name: String,
    },
    /// `session/cancel` — interrupts the in-flight turn.
    Cancel { session_id: SessionId },
    /// `session/kill` — terminate the session (recoverable via resume).
    Kill { session_id: SessionId },
    /// `session/resume` — bring a `Done`/`Error` session back on a fresh
    /// adapter connection.
    Resume { session_id: SessionId },
}

/// Map staged relays onto the wire `SessionRef` shape: the referenced
/// session is the *source*; `target` only guided where the prompt goes.
fn session_refs(relays: &[PendingRelay]) -> Vec<SessionRef> {
    relays
        .iter()
        .map(|r| SessionRef {
            session_id: r.source,
            event_seq: r.seq,
        })
        .collect()
}

fn usage() -> &'static str {
    "agentmux-tui — terminal client for the agentmux daemon\n\
     \n\
     Usage: agentmux-tui [--help]\n\
     \n\
     Connects to the daemon socket ($AGENTMUX_SOCK or the default data\n\
     dir), starting the daemon first when needed.\n\
     \n\
     Keys: q quit · j/k select · i/a prompt · n new-session wizard ·\n\
     \x20     @ relay event→session · tab files panel · x kill ·\n\
     \x20     r resume · ctrl-c cancel"
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().skip(1).any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentmux-tui: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    // `connect` auto-spawns the daemon when the socket is dead.
    let mut client = DaemonClient::connect().await?;
    let mut events = client.subscribe_events().await?;
    let mut app = snapshot(&mut client).await?;
    app.set_status(format!("connected to {}", client.socket_path().display()));
    let socket_path = client.socket_path().to_path_buf();
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiMsg>(64);

    // `try_init` enables raw mode + alt screen and installs a panic hook
    // that restores the terminal; `restore` handles the clean exit path.
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        &client,
        &mut events,
        &ui_tx,
        &mut ui_rx,
        &socket_path,
    )
    .await;
    ratatui::restore();
    result
}

/// The initial UI snapshot: every project's workspaces, each
/// workspace's sessions (decorated with agent/workspace display names),
/// and the configured agents.
async fn snapshot(client: &mut DaemonClient) -> Result<App, Box<dyn Error>> {
    let projects = client.list_projects().await?;
    let mut workspaces = Vec::new();
    for project in &projects {
        workspaces.extend(client.list_workspaces(project.id).await?);
    }
    let agents = client.list_agents().await?;
    let mut views = Vec::new();
    for ws in &workspaces {
        for session in client.list_sessions(ws.id).await? {
            let agent_name = agents
                .iter()
                .find(|a| a.id == session.agent_id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| session.agent_id.to_string());
            views.push(SessionView {
                session,
                agent_name,
                workspace_name: ws.name.clone(),
            });
        }
    }
    Ok(App::new(projects, workspaces, views, agents))
}

/// How often the loop polls [`DaemonClient::is_closed`]. The event
/// stream itself never ends (the broadcast sender is held by the
/// client), so disconnect detection has to go through the flag.
const DISCONNECT_POLL: Duration = Duration::from_millis(250);

/// The render/input loop: daemon events, terminal keys and spawned-call
/// results merged over `tokio::select!`. `client` is held open — the
/// event stream is pumped by its reader task — and polled for liveness.
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    client: &DaemonClient,
    events: &mut (impl Stream<Item = Event> + Unpin),
    ui_tx: &mpsc::Sender<UiMsg>,
    ui_rx: &mut mpsc::Receiver<UiMsg>,
    socket_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut term_events = EventStream::new();
    let mut daemon_live = true;
    let mut liveness = tokio::time::interval(DISCONNECT_POLL);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        tokio::select! {
            // `events.next()` can never yield None while `client` holds
            // the broadcast sender — dead-connection detection is the
            // liveness arm below, not stream exhaustion.
            maybe_event = events.next(), if daemon_live => {
                if let Some(ev) = maybe_event {
                    app.handle_event(ev);
                }
            }
            _ = liveness.tick(), if daemon_live => {
                if client.is_closed() {
                    daemon_live = false;
                    app.set_status("daemon disconnected — event stream ended (q to quit)");
                }
            }
            term = term_events.next() => match term {
                Some(Ok(TermEvent::Key(key))) => {
                    if dispatch(app.handle_key(key), app, ui_tx, socket_path) {
                        break;
                    }
                }
                Some(Ok(TermEvent::Resize(..))) => {}
                Some(Ok(_)) => {} // mouse/focus/paste — unused in v1
                Some(Err(e)) => app.set_status(format!("terminal event error: {e}")),
                None => break, // stdin closed
            },
            Some(msg) = ui_rx.recv() => apply_msg(app, msg),
        }
    }
    Ok(())
}

/// Turn an [`AppAction`] into daemon work. Returns `true` to quit.
/// Long-running calls go to a spawned task with its own connection —
/// the loop stays responsive.
fn dispatch(
    action: AppAction,
    app: &mut App,
    ui_tx: &mpsc::Sender<UiMsg>,
    socket_path: &Path,
) -> bool {
    match action {
        AppAction::None => {}
        AppAction::Quit => return true,
        AppAction::Submit { text, references } => match app.selected_session_id() {
            Some(session_id) => {
                // `App` staged relays in its own terms; the wire wants
                // `SessionRef`s — `source`/`seq` only.
                let references = session_refs(&references);
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Prompt {
                        session_id,
                        text,
                        references,
                    },
                );
                app.set_status("prompt sent…");
            }
            None => app.set_status("no session selected"),
        },
        AppAction::CreateSession {
            workspace,
            agent_id,
        } => {
            let agent_name = app
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| agent_id.to_string());
            let label = match &workspace {
                WorkspacePick::Existing { name, .. } => name.clone(),
                WorkspacePick::New { name, .. } => format!("new workspace {name}"),
            };
            spawn_call(
                ui_tx,
                socket_path,
                DaemonCall::Create {
                    workspace,
                    agent_id,
                    agent_name,
                },
            );
            app.set_status(format!("creating session in {label}…"));
        }
        AppAction::CancelPrompt => match app.selected_session_id() {
            Some(session_id) => {
                spawn_call(ui_tx, socket_path, DaemonCall::Cancel { session_id });
                app.set_status("cancelling…");
            }
            None => app.set_status("no session selected"),
        },
        AppAction::KillSession => match app.selected_session_id() {
            Some(session_id) => {
                spawn_call(ui_tx, socket_path, DaemonCall::Kill { session_id });
                app.set_status("killing session…");
            }
            None => app.set_status("no session selected"),
        },
        AppAction::ResumeSession => match app.selected_session_id() {
            Some(session_id) => {
                spawn_call(ui_tx, socket_path, DaemonCall::Resume { session_id });
                app.set_status("resuming session…");
            }
            None => app.set_status("no session selected"),
        },
    }
    false
}

/// Run `call` on a fresh daemon connection in a background task and
/// report back through `ui_tx`. Fresh connections matter: the daemon
/// dispatches sequentially per connection, so a `prompt` blocking for a
/// whole turn can't be interrupted by a `cancel` on the same socket.
fn spawn_call(ui_tx: &mpsc::Sender<UiMsg>, socket_path: &Path, call: DaemonCall) {
    let ui_tx = ui_tx.clone();
    let socket_path: PathBuf = socket_path.to_path_buf();
    tokio::spawn(async move {
        let msg = match DaemonClient::connect_existing(&socket_path).await {
            Ok(mut client) => match call {
                DaemonCall::Prompt {
                    session_id,
                    text,
                    references,
                } => match client.prompt(session_id, text, references).await {
                    Ok(()) => UiMsg::Status("prompt turn finished".to_string()),
                    Err(e) => UiMsg::Status(format!("prompt failed: {e}")),
                },
                DaemonCall::Cancel { session_id } => match client.cancel(session_id).await {
                    Ok(()) => UiMsg::Status("prompt cancelled".to_string()),
                    Err(e) => UiMsg::Status(format!("cancel failed: {e}")),
                },
                DaemonCall::Kill { session_id } => match client.kill(session_id).await {
                    Ok(()) => UiMsg::Status("session killed".to_string()),
                    Err(e) => UiMsg::Status(format!("kill failed: {e}")),
                },
                DaemonCall::Resume { session_id } => match client.resume(session_id).await {
                    Ok(()) => UiMsg::Status("session resumed".to_string()),
                    Err(e) => UiMsg::Status(format!("resume failed: {e}")),
                },
                DaemonCall::Create {
                    workspace,
                    agent_id,
                    agent_name,
                } => match create_session(&mut client, workspace, agent_id, agent_name).await {
                    Ok((view, new_workspace)) => UiMsg::Created {
                        view: Box::new(view),
                        new_workspace,
                    },
                    Err(e) => UiMsg::Status(e),
                },
            },
            Err(e) => UiMsg::Status(format!("daemon unreachable: {e}")),
        };
        // The loop may have exited — a dropped receiver is fine.
        let _ = ui_tx.send(msg).await;
    });
}

/// `session/create`, preceded by `workspace/create` when the wizard
/// asked for a fresh worktree. Returns the session view plus the new
/// workspace (so the UI can grow its grouping list).
async fn create_session(
    client: &mut DaemonClient,
    workspace: WorkspacePick,
    agent_id: AgentId,
    agent_name: String,
) -> Result<(SessionView, Option<Workspace>), String> {
    let (workspace_id, workspace_name, new_workspace) = match workspace {
        WorkspacePick::Existing { id, name } => (id, name, None),
        WorkspacePick::New { project_id, name } => client
            .create_workspace(project_id, name, None)
            .await
            .map(|ws| (ws.id, ws.name.clone(), Some(ws)))
            .map_err(|e| format!("workspace/create failed: {e}"))?,
    };
    client
        .create_session(workspace_id, agent_id, None)
        .await
        .map(|session| {
            (
                SessionView {
                    session,
                    agent_name,
                    workspace_name,
                },
                new_workspace,
            )
        })
        .map_err(|e| format!("session/create failed: {e}"))
}

/// Apply a spawned-call result to the UI.
fn apply_msg(app: &mut App, msg: UiMsg) {
    match msg {
        UiMsg::Status(s) => app.set_status(s),
        UiMsg::Created {
            view,
            new_workspace,
        } => {
            // A wizard-created workspace joins the grouping list so the
            // new session renders under its header.
            if let Some(ws) = new_workspace {
                if !app.workspaces.iter().any(|w| w.id == ws.id) {
                    app.workspaces.push(ws);
                }
            }
            let index = app.add_session(*view);
            app.selected = index;
            app.mark_selected_viewed();
            app.set_status("session created");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmux_core::rpc::{SessionPromptParams, M_SESSION_PROMPT};

    /// Brief test ①: the Submit path must produce `session/prompt`
    /// params whose `references` carry the staged relays — `source` →
    /// `session_id`, `seq` → `event_seq` (target is not on the wire).
    #[test]
    fn staged_relays_become_session_ref_params() {
        let src = SessionId::new();
        let target = SessionId::new();
        let relays = vec![PendingRelay {
            source: src,
            seq: 7,
            target,
        }];
        let refs = session_refs(&relays);
        assert_eq!(
            refs,
            vec![SessionRef {
                session_id: src,
                event_seq: 7
            }]
        );

        // …and they serialize onto the wire shape unchanged.
        let prompt_target = SessionId::new();
        let params = SessionPromptParams {
            session_id: prompt_target,
            text: "incorporate it".into(),
            references: refs,
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["method"], serde_json::Value::Null); // params only
        assert_eq!(
            v["references"],
            serde_json::json!([{"session_id": src, "event_seq": 7}])
        );
        assert_eq!(M_SESSION_PROMPT, "session/prompt");
    }
}
