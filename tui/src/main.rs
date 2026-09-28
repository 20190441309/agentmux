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
mod ui;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use agentmux_client::DaemonClient;
use agentmux_core::{AgentId, Event, SessionId, WorkspaceId};
use crossterm::event::{Event as TermEvent, EventStream};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

use crate::app::{App, AppAction, SessionView};

/// Result of a spawned daemon call, reported back to the UI loop.
enum UiMsg {
    /// Something to show in the status bar (success note or error).
    Status(String),
    /// `session/create` succeeded — insert into the list + select it.
    Created(SessionView),
}

/// A daemon operation the loop can run on a background task.
enum DaemonCall {
    /// `session/prompt` — blocks until the turn finishes.
    Prompt { session_id: SessionId, text: String },
    /// `session/create`.
    Create {
        workspace_id: WorkspaceId,
        agent_id: AgentId,
        agent_name: String,
        workspace_name: String,
    },
    /// `session/cancel` — interrupts the in-flight turn.
    Cancel { session_id: SessionId },
}

fn usage() -> &'static str {
    "agentmux-tui — terminal client for the agentmux daemon\n\
     \n\
     Usage: agentmux-tui [--help]\n\
     \n\
     Connects to the daemon socket ($AGENTMUX_SOCK or the default data\n\
     dir), starting the daemon first when needed.\n\
     \n\
     Keys: q quit · j/k select · i/a prompt · n new session ·\n\
     \x20     @ relay (T14) · ctrl-c cancel prompt"
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
    Ok(App::new(workspaces, views, agents))
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
        AppAction::Submit(text) => match app.selected_session_id() {
            Some(session_id) => {
                spawn_call(ui_tx, socket_path, DaemonCall::Prompt { session_id, text });
                app.set_status("prompt sent…");
            }
            None => app.set_status("no session selected"),
        },
        // Minimal v1 flow: first workspace + first available agent
        // (falling back to the first agent). The full picker is T14.
        AppAction::NewSession => {
            let workspace = app.workspaces.first();
            let agent = app
                .agents
                .iter()
                .find(|a| a.available)
                .or_else(|| app.agents.first());
            match (workspace, agent) {
                (Some(ws), Some(agent)) => {
                    spawn_call(
                        ui_tx,
                        socket_path,
                        DaemonCall::Create {
                            workspace_id: ws.id,
                            agent_id: agent.id.clone(),
                            agent_name: agent.name.clone(),
                            workspace_name: ws.name.clone(),
                        },
                    );
                    app.set_status(format!("creating session in {}…", ws.name));
                }
                (None, _) => app.set_status("no workspace registered — create one first"),
                (_, None) => app.set_status("no agent configured"),
            }
        }
        AppAction::CancelPrompt => match app.selected_session_id() {
            Some(session_id) => {
                spawn_call(ui_tx, socket_path, DaemonCall::Cancel { session_id });
                app.set_status("cancelling…");
            }
            None => app.set_status("no session selected"),
        },
        AppAction::Relay => {
            app.set_status("relay: not wired yet — lands in Task 14");
        }
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
                DaemonCall::Prompt { session_id, text } => {
                    match client.prompt(session_id, text, vec![]).await {
                        Ok(()) => UiMsg::Status("prompt turn finished".to_string()),
                        Err(e) => UiMsg::Status(format!("prompt failed: {e}")),
                    }
                }
                DaemonCall::Cancel { session_id } => match client.cancel(session_id).await {
                    Ok(()) => UiMsg::Status("prompt cancelled".to_string()),
                    Err(e) => UiMsg::Status(format!("cancel failed: {e}")),
                },
                DaemonCall::Create {
                    workspace_id,
                    agent_id,
                    agent_name,
                    workspace_name,
                } => match client.create_session(workspace_id, agent_id, None).await {
                    Ok(session) => UiMsg::Created(SessionView {
                        session,
                        agent_name,
                        workspace_name,
                    }),
                    Err(e) => UiMsg::Status(format!("session/create failed: {e}")),
                },
            },
            Err(e) => UiMsg::Status(format!("daemon unreachable: {e}")),
        };
        // The loop may have exited — a dropped receiver is fine.
        let _ = ui_tx.send(msg).await;
    });
}

/// Apply a spawned-call result to the UI.
fn apply_msg(app: &mut App, msg: UiMsg) {
    match msg {
        UiMsg::Status(s) => app.set_status(s),
        UiMsg::Created(view) => {
            let index = app.add_session(view);
            app.selected = index;
            app.set_status("session created");
        }
    }
}
