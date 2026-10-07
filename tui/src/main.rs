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

mod agent_info;
mod app;
mod attention;
#[cfg(test)]
mod audit;
mod commands;
mod context;
mod editor;
mod external;
mod files;
mod focus;
mod input;
mod interaction;
mod markdown;
mod naming;
mod native;
mod newsession;
mod reasoning;
mod recovery;
mod references;
mod shell;
mod theme;
mod transcript;
mod ui;
mod workbench;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::workbench::Prompt;
use agentmux_client::DaemonClient;
use agentmux_core::{AgentId, Event, PermissionDecision, SessionId, SessionRef, Workspace};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyEventKind, MouseButton, MouseEventKind,
};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

use crate::app::{App, AppAction, PendingRelay, SessionView};
use crate::newsession::WorkspacePick;

/// Result of a spawned daemon call, reported back to the UI loop.
enum UiMsg {
    Info {
        session: SessionId,
        request: u64,
        result: Result<serde_json::Value, String>,
    },
    Context {
        workspace: agentmux_core::WorkspaceId,
        request: u64,
        saving: bool,
        result: Result<agentmux_core::rpc::WorkspaceContextResult, String>,
    },
    Reference {
        session: SessionId,
        seq: u64,
        result: Result<Event, String>,
    },
    Copy {
        session: SessionId,
        text: String,
        result: Result<(), String>,
    },
    NativeList {
        session_id: SessionId,
        serial: u64,
        result: Result<agentmux_core::rpc::NativeListResult, String>,
    },
    NativeOpened {
        source: SessionId,
        result: Result<agentmux_core::Session, String>,
    },
    Renamed {
        session_id: SessionId,
        result: Result<Event, String>,
    },
    Recovered {
        session_id: SessionId,
        result: Result<agentmux_core::rpc::SessionHistoryResult, String>,
    },
    Pi {
        session_id: SessionId,
        serial: Option<u64>,
        command: serde_json::Value,
        result: Result<serde_json::Value, String>,
    },
    Resumed {
        session_id: SessionId,
        result: Result<(), String>,
    },
    PromptFinished {
        session_id: SessionId,
        prompt: Prompt,
        error: Option<String>,
    },
    History {
        session_id: SessionId,
        result: Result<agentmux_core::rpc::SessionHistoryResult, String>,
    },
    Diff {
        request: u64,
        session_id: SessionId,
        path: String,
        result: Result<agentmux_core::rpc::WorkspaceDiffResult, String>,
    },
    Files {
        workspace_id: agentmux_core::WorkspaceId,
        request: u64,
        result: Result<agentmux_core::rpc::WorkspaceChangesResult, String>,
    },
    Project(Result<agentmux_core::Project, String>),
    /// Something to show in the status bar (success note or error).
    Status(String),
    Error {
        session_id: Option<SessionId>,
        message: String,
    },
    /// `session/create` succeeded — insert into the list + select it.
    /// `new_workspace` is the workspace the wizard just created, so the
    /// session groups under its header even before a reload. Boxed:
    /// `SessionView` dwarfs the `Status` variant.
    Created {
        epoch: u64,
        view: Box<SessionView>,
        new_workspace: Option<Workspace>,
    },
    CreateFailed {
        epoch: u64,
        failure: CreateFailure,
    },
    /// `session/permission` failed — the request is still parked, so
    /// the overlay must un-pend and let the user answer again. Success
    /// needs no message: `PermissionResolved` arrives on the event
    /// stream and dismisses the dialog.
    PermissionFailed {
        session_id: SessionId,
        request_id: String,
        error: String,
    },
}

/// A daemon operation the loop can run on a background task.
enum DaemonCall {
    Info {
        session: SessionId,
        request: u64,
    },
    Context {
        workspace: agentmux_core::WorkspaceId,
        request: u64,
    },
    SaveContext {
        params: agentmux_core::rpc::WorkspaceContextSaveParams,
        request: u64,
    },
    Reference {
        session: SessionId,
        seq: u64,
    },
    NativeList {
        session_id: SessionId,
        serial: u64,
    },
    NativeOpen(agentmux_core::rpc::NativeOpenParams),
    Rename {
        session_id: SessionId,
        title: String,
    },
    Pi {
        session_id: SessionId,
        serial: Option<u64>,
        command: serde_json::Value,
    },
    History {
        session_id: SessionId,
        before: Option<u64>,
    },
    Diff {
        request: u64,
        session_id: SessionId,
        path: String,
        params: agentmux_core::rpc::WorkspaceDiffParams,
    },
    Files {
        workspace_id: agentmux_core::WorkspaceId,
        request: u64,
    },
    Project(String),
    /// `session/prompt` — blocks until the turn finishes.
    Prompt {
        session_id: SessionId,
        prompt: Prompt,
    },
    /// `workspace/create` (for [`WorkspacePick::New`]) then
    /// `session/create`.
    Create {
        epoch: u64,
        workspace: WorkspacePick,
        agent_id: AgentId,
        agent_name: String,
    },
    /// `session/cancel` — interrupts the in-flight turn.
    Cancel {
        session_id: SessionId,
    },
    /// `session/kill` — terminate the session (recoverable via resume).
    Kill {
        session_id: SessionId,
    },
    /// `session/resume` — bring a `Done`/`Error` session back on a fresh
    /// adapter connection.
    Resume {
        session_id: SessionId,
    },
    /// `session/permission` — answer a parked permission request.
    Permission {
        session_id: SessionId,
        request_id: String,
        outcome: PermissionDecision,
    },
}

struct CreateFailure {
    error: String,
    new_workspace: Option<Box<Workspace>>,
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
    "agentmux-tui — terminal agent workbench\n\nUsage: agentmux-tui [--restart-daemon] [--help]\n\n--restart-daemon  Restart the background service before opening the TUI.\n                  Running agents disconnect; resume tasks after restart.\n\nType directly. Enter sends; Alt+Enter inserts a newline.\nCtrl+P tasks · Ctrl+B sidebar · F6 focus · F1 help\n/mux new · /mux project <path> · /mux files · /mux permissions · /mux relay · /mux quit\nAGENTMUX_THEME=dark|light|terminal"
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StartupOptions {
    help: bool,
    restart_daemon: bool,
    attach: Option<SessionId>,
    seed_fd: Option<i32>,
}

fn parse_options(args: impl IntoIterator<Item = String>) -> Result<StartupOptions, String> {
    let mut options = StartupOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => options.help = true,
            "--restart-daemon" => options.restart_daemon = true,
            "--attach" => {
                let id = args.next().ok_or("--attach requires a session id")?;
                options.attach = Some(
                    serde_json::from_value(serde_json::Value::String(id))
                        .map_err(|_| "invalid native session id")?,
                );
            }
            "--seed-fd" => {
                let fd: i32 = args
                    .next()
                    .ok_or("--seed-fd requires a descriptor")?
                    .parse()
                    .map_err(|_| "invalid seed descriptor")?;
                if fd < 3 {
                    return Err("seed descriptor must not replace standard I/O".into());
                }
                options.seed_fd = Some(fd);
            }
            _ => return Err(format!("unknown argument: {arg}\n{}", usage())),
        }
    }
    if options.seed_fd.is_some() && options.attach.is_none() {
        return Err("--seed-fd requires --attach".into());
    }
    if options.attach.is_some() && options.restart_daemon {
        return Err("native attachment must not restart the daemon".into());
    }
    Ok(options)
}

#[tokio::main]
async fn main() -> ExitCode {
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("agentmux-tui: {error}");
            return ExitCode::FAILURE;
        }
    };
    if options.help {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    if let Some(id) = options.attach {
        return match native::attach(&DaemonClient::default_socket_path(), id, options.seed_fd).await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("agentmux native: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentmux-tui: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: StartupOptions) -> Result<(), Box<dyn Error>> {
    // `connect` auto-spawns the daemon when the socket is dead.
    let mut client = if options.restart_daemon {
        eprintln!("Restarting daemon…");
        DaemonClient::restart().await?
    } else {
        DaemonClient::connect().await?
    };
    let mut events = client.subscribe_events().await?;
    let mut app = snapshot(&mut client).await?;
    if app.status.is_none() {
        app.set_status("connected · New space · Add agent · F1 help");
    }
    let socket_path = client.socket_path().to_path_buf();
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiMsg>(64);

    // `try_init` enables raw mode + alt screen and installs a panic hook
    // that restores the terminal; `restore` handles the clean exit path.
    let mut terminal = ratatui::try_init()?;
    if let Err(error) =
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)
    {
        ratatui::restore();
        return Err(error.into());
    }
    let result = loop {
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
        if let Some(edit) = app.wb.external_edit.take() {
            let _ = crossterm::execute!(
                std::io::stdout(),
                DisableBracketedPaste,
                DisableMouseCapture
            );
            ratatui::restore();
            let text = match &edit {
                external::EditRequest::Draft { text, .. }
                | external::EditRequest::Context { text, .. } => text.clone(),
            };
            let edited = tokio::task::spawn_blocking(move || external::edit_text(&text)).await;
            match (edit, edited) {
                (
                    external::EditRequest::Context {
                        workspace,
                        original,
                        ..
                    },
                    Ok(Ok(text)),
                ) => {
                    app.wb.context.snapshots.entry(workspace).or_default().draft =
                        Some(context::Draft {
                            expected: original,
                            text,
                        });
                    app.wb.context.view = context::View::Draft;
                    app.set_status(
                        "Shared context edit retained; Save commits it, nothing sent to agents.",
                    );
                }
                (external::EditRequest::Draft { session, .. }, Ok(Ok(text))) => {
                    if app.selected_session_id() == Some(session) {
                        app.input = text;
                        app.wb.cursor = app.input.len();
                    } else {
                        let draft = app.wb.sessions.entry(session).or_default();
                        draft.draft = text;
                        draft.cursor = draft.draft.len();
                    }
                    app.set_status("Draft updated by editor; not sent.");
                }
                (_, Ok(Err(error))) => app.set_error(error),
                (_, Err(error)) => {
                    app.set_error(format!("Editor failed: {error}; original text kept"))
                }
            }
            terminal = ratatui::try_init()?;
            crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
            continue;
        }
        if let Some(id) = app.wb.native_attach.take() {
            let _ = crossterm::execute!(
                std::io::stdout(),
                DisableBracketedPaste,
                DisableMouseCapture
            );
            ratatui::restore();
            let seed = app.wb.native_seed.take();
            match native::launch(&socket_path, id, seed.clone()).await {
                Ok(true) => {
                    if seed.is_some() {
                        app.input.clear();
                        app.wb.cursor = 0;
                    }
                }
                Ok(false) => app.set_error(
                    "Native attachment ended with an error. Agent and draft are retained.",
                ),
                Err(error) => app.set_error(format!("Native attachment failed: {error}")),
            }
            terminal = ratatui::try_init()?;
            crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
            continue;
        }
        break result;
    };
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
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
    let mut app = App::new(projects, workspaces, views, agents);
    for id in app
        .sessions
        .iter()
        .map(|v| v.session.id)
        .collect::<Vec<_>>()
    {
        match client.history(id, None).await {
            Ok(page) => {
                for event in &page.pending_permissions {
                    app.handle_event(event.clone());
                }
                app.merge_history_page(id, page);
            }
            Err(e) => app.set_error_for(
                Some(id),
                format!("history unavailable (update daemon): {e}"),
            ),
        }
    }
    Ok(app)
}

/// How often the loop polls [`DaemonClient::is_closed`]. The event
/// stream itself never ends (the broadcast sender is held by the
/// client), so disconnect detection has to go through the flag.
const DISCONNECT_POLL: Duration = Duration::from_millis(250);

/// Only events that can change this UI should wake the render loop. Mouse
/// motion/drag reports can arrive at hundreds of Hz on remote terminals.
fn actionable_terminal_event(event: &TermEvent) -> bool {
    match event {
        TermEvent::Key(key) => key.kind != KeyEventKind::Release,
        TermEvent::Mouse(mouse) => matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
        ),
        TermEvent::Paste(_) | TermEvent::Resize(..) => true,
        _ => false,
    }
}

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
    let mut term_events = EventStream::new().filter(|event| {
        futures_util::future::ready(match event {
            Ok(event) => actionable_terminal_event(event),
            Err(_) => true,
        })
    });
    let mut daemon_live = true;
    let mut liveness = tokio::time::interval(DISCONNECT_POLL);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    // Coalesce wheel/input bursts and streamed deltas at up to 60 FPS.
    // Cached conversation rows keep these frames independent of scroll depth.
    let mut frames = tokio::time::interval(Duration::from_micros(16_667));
    frames.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut recovery = recovery::Recovery::new(app);
    app.wb.columns = terminal.size()?.width;
    terminal.draw(|frame| ui::draw(frame, app))?;
    loop {
        if app.wb.native_attach.is_some() || app.wb.external_edit.is_some() {
            break;
        }
        if files::visible(app) {
            if let Some((workspace_id, request)) = app.file_refresh(false) {
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Files {
                        workspace_id,
                        request,
                    },
                );
            }
        }
        if context::visible(app) {
            if let Some((workspace, request)) = app.context_refresh(false) {
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Context { workspace, request },
                );
            }
        }
        if let Some((session, seq)) = references::needed(app) {
            spawn_call(ui_tx, socket_path, DaemonCall::Reference { session, seq });
        }
        if let Some((session, request)) = app.info_refresh(false) {
            spawn_call(ui_tx, socket_path, DaemonCall::Info { session, request });
        }
        if daemon_live {
            recovery.schedule(ui_tx, socket_path);
        }
        if app.selected_is_pi() && app.input.starts_with('/') {
            if let Some(session_id) = app.selected_session_id() {
                if !app.wb.pi_loaded.contains(&session_id) && app.wb.pi_loading.insert(session_id) {
                    spawn_call(
                        ui_tx,
                        socket_path,
                        DaemonCall::Pi {
                            session_id,
                            serial: None,
                            command: serde_json::json!({"type":"get_commands"}),
                        },
                    );
                }
            }
        }
        if let Some((session_id, prompt)) = app.ready_queued() {
            spawn_call(
                ui_tx,
                socket_path,
                DaemonCall::Prompt { session_id, prompt },
            );
        }
        tokio::select! {
            _ = frames.tick() => {
                if dirty {
                    app.wb.columns = terminal.size()?.width;
                    terminal.draw(|frame| ui::draw(frame, app))?;
                    dirty = false;
                    // A coalesced wheel burst can reach the oldest cached row
                    // in this frame. Start prefetch without requiring another tick.
                    if app.needs_older_history() {
                        dispatch(AppAction::LoadHistory, app, ui_tx, socket_path);
                    }
                }
            }
            // `events.next()` can never yield None while `client` holds
            // the broadcast sender — dead-connection detection is the
            // liveness arm below, not stream exhaustion.
            maybe_event = events.next(), if daemon_live => {
                if let Some(ev) = maybe_event {
                    if recovery.observe(&ev) {
                        app.handle_event(ev);
                    } else if recovery::is_lag(&ev) {
                        app.set_status("Syncing missed output…");
                    }
                    dirty = true;
                }
            }
            _ = liveness.tick(), if daemon_live => {
                if app.run_status().is_some() || files::visible(app) || context::visible(app) { dirty = true; }
                if client.is_closed() {
                    daemon_live = false;
                    app.wb.connected = false;
                    app.set_error_for(None, "Daemon disconnected — use Menu → Exit TUI to close. Drafts kept.");
                    dirty = true;
                }
            }
            term = term_events.next() => match term {
                Some(Ok(TermEvent::Key(key))) => {
                    dirty = true;
                    if dispatch(app.handle_key(key), app, ui_tx, socket_path) {
                        break;
                    }
                }
                Some(Ok(TermEvent::Paste(text))) => {
                    dirty = true;
                    app.paste_text(&text);
                }
                Some(Ok(TermEvent::Mouse(mouse))) => {
                    dirty = true;
                    if dispatch(interaction::mouse(app, mouse), app, ui_tx, socket_path) { break; }
                }
                Some(Ok(TermEvent::Resize(..))) => {
                    app.wb.hits.borrow_mut().clear();
                    app.wb.control_focus = None;
                    dirty = true;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => { app.set_error_for(None, format!("terminal event error: {e}")); dirty = true; }
                None => break, // stdin closed
            },
            Some(msg) = ui_rx.recv() => {
                if let UiMsg::Recovered { session_id, result } = &msg {
                    recovery.finished(*session_id, result.as_ref().ok());
                }
                apply_msg(app, msg); dirty = true;
            },
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
        AppAction::RefreshInfo => {
            if let Some((session, request)) = app.info_refresh(true) {
                spawn_call(ui_tx, socket_path, DaemonCall::Info { session, request });
            }
        }
        AppAction::RefreshContext => {
            if let Some((workspace, request)) = app.context_refresh(true) {
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Context { workspace, request },
                );
            }
        }
        AppAction::SaveContext(params) => {
            app.wb.context.serial += 1;
            let request = app.wb.context.serial;
            let snapshot = app
                .wb
                .context
                .snapshots
                .entry(params.workspace_id)
                .or_default();
            if snapshot.request.is_none() {
                snapshot.request = Some(request);
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::SaveContext { params, request },
                );
            }
        }
        AppAction::CopyText { session, text } => {
            let tx = ui_tx.clone();
            tokio::spawn(async move {
                let copied = text.clone();
                let result = tokio::task::spawn_blocking(move || external::clipboard(&copied))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                let _ = tx
                    .send(UiMsg::Copy {
                        session,
                        text,
                        result,
                    })
                    .await;
            });
        }
        AppAction::AttachNative => {
            if let Some(view) = app.selected_session().cloned() {
                if !view.session.native_terminal {
                    app.set_status("Use /mux native for a supported native handoff.");
                } else if matches!(
                    view.session.state,
                    agentmux_core::SessionState::Done | agentmux_core::SessionState::Error(_)
                ) {
                    if app.wb.resuming.insert(view.session.id) {
                        spawn_call(
                            ui_tx,
                            socket_path,
                            DaemonCall::Resume {
                                session_id: view.session.id,
                            },
                        );
                    }
                } else {
                    app.wb.native_attach = Some(view.session.id);
                }
            }
        }
        AppAction::BrowseNative => {
            if let Some(view) = app.selected_session().cloned() {
                let pi = app.selected_is_pi()
                    || app.agents.iter().any(|a| {
                        a.id == view.session.agent_id
                            && matches!(
                                a.adapter,
                                agentmux_core::AdapterKind::Native {
                                    session_backend: Some(agentmux_core::NativeSessionBackend::Pi),
                                    ..
                                }
                            )
                    });
                if !pi && view.session.native_terminal {
                    if app.wb.pi_busy.insert(view.session.id) {
                        spawn_call(
                            ui_tx,
                            socket_path,
                            DaemonCall::NativeOpen(agentmux_core::rpc::NativeOpenParams {
                                session_id: view.session.id,
                                session_file: None,
                                native: true,
                                history: true,
                            }),
                        );
                    }
                } else {
                    app.wb.pi_serial += 1;
                    let serial = app.wb.pi_serial;
                    app.wb.pi_panel = Some(commands::PiPanel {
                        session_id: view.session.id,
                        serial,
                        title: "Pi · resume conversation".into(),
                        choices: vec![],
                        query: String::new(),
                        cursor: 0,
                        loading: true,
                        native_catalog: true,
                        native_mode: view.session.native_terminal,
                        native_available: false,
                        structured_available: !view.session.native_terminal,
                    });
                    spawn_call(
                        ui_tx,
                        socket_path,
                        DaemonCall::NativeList {
                            session_id: view.session.id,
                            serial,
                        },
                    );
                }
            }
        }
        AppAction::OpenNative {
            session_id,
            session_file,
            native,
            history,
        } => {
            if !app.wb.pi_busy.insert(session_id) {
                app.set_status("A native history operation is already running.");
                return false;
            }
            if let Some(panel) = app.wb.pi_panel.as_mut() {
                panel.loading = true;
            }
            spawn_call(
                ui_tx,
                socket_path,
                DaemonCall::NativeOpen(agentmux_core::rpc::NativeOpenParams {
                    session_id,
                    session_file,
                    native,
                    history,
                }),
            );
        }
        AppAction::Pi(command) => {
            if let Some(session_id) = app.selected_session_id() {
                if !app.wb.pi_busy.insert(session_id) {
                    app.set_status("A Pi command is already running.");
                    return false;
                }
                app.wb.pi_serial += 1;
                let serial = app.wb.pi_serial;
                app.wb.control_focus = None;
                app.wb.pi_panel = Some(commands::PiPanel {
                    native_catalog: false,
                    native_mode: false,
                    native_available: false,
                    structured_available: false,
                    session_id,
                    serial,
                    title: "Pi".into(),
                    choices: vec![],
                    query: String::new(),
                    cursor: 0,
                    loading: true,
                });
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Pi {
                        session_id,
                        serial: Some(serial),
                        command,
                    },
                );
            }
        }
        AppAction::None => {}
        AppAction::Quit => return true,
        AppAction::Submit { text, references } => {
            if app.selected_is_native() {
                app.input = text;
                if !references.is_empty() {
                    app.pending_relays = references;
                    app.set_status(
                        "Quoted references remain in the draft; native input was not sent.",
                    );
                    return false;
                }
                app.wb.native_seed = Some(app.input.clone());
                return dispatch(AppAction::AttachNative, app, ui_tx, socket_path);
            }
            let prompt = Prompt { text, references };
            if let Some(session_id) = app.selected_session_id() {
                if !app.wb.connected {
                    app.restore_prompt(session_id, prompt);
                    app.set_error("Daemon disconnected. Draft kept; reopen the TUI to reconnect.");
                } else {
                    let needs_resume = matches!(
                        app.selected_session().unwrap().session.state,
                        agentmux_core::SessionState::Done | agentmux_core::SessionState::Error(_)
                    );
                    app.record_prompt(session_id, &prompt.text);
                    app.wb
                        .queues
                        .entry(session_id)
                        .or_default()
                        .push_back(prompt);
                    if app.wb.resuming.contains(&session_id) {
                        app.set_status("Message queued; waiting for the agent to restart…");
                    } else if needs_resume {
                        app.wb.resuming.insert(session_id);
                        spawn_call(ui_tx, socket_path, DaemonCall::Resume { session_id });
                        app.set_status("Restarting agent; your message will send when ready…");
                    } else {
                        app.set_status("Message queued for this agent.");
                    }
                }
            } else {
                app.input = prompt.text;
                app.pending_relays = prompt.references;
                app.wb.cursor = app.input.len();
                app.set_status("no session selected");
            }
        }
        AppAction::LoadHistory => {
            if let Some(session_id) = app.selected_session_id() {
                let before = app.session_events().map(|e| e.seq).min();
                let ui = app.wb.sessions.entry(session_id).or_default();
                if !ui.loading {
                    ui.loading = true;
                    spawn_call(
                        ui_tx,
                        socket_path,
                        DaemonCall::History { session_id, before },
                    );
                }
            }
        }
        AppAction::InspectFile(path) => {
            if let Some(view) = app.selected_session() {
                let workspace_id = view.session.workspace_id;
                return dispatch(
                    AppAction::InspectChange(agentmux_core::rpc::WorkspaceDiffParams {
                        workspace_id,
                        path,
                        path_bytes: None,
                        old_path: None,
                        old_path_bytes: None,
                        scope: app.wb.files.diff_scope,
                    }),
                    app,
                    ui_tx,
                    socket_path,
                );
            }
        }
        AppAction::InspectChange(params) => {
            if app.file_workspace() == Some(params.workspace_id) {
                let session_id = app.selected_session_id().unwrap();
                let request = app.begin_diff(params.path.clone());
                app.wb.files.diff_scope = params.scope;
                app.wb.files.diff_params = Some(params.clone());
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Diff {
                        request,
                        session_id,
                        path: params.path.clone(),
                        params,
                    },
                );
                app.set_status("loading workspace diff…");
            }
        }
        AppAction::RefreshFiles => {
            if let Some((workspace_id, request)) = app.file_refresh(true) {
                spawn_call(
                    ui_tx,
                    socket_path,
                    DaemonCall::Files {
                        workspace_id,
                        request,
                    },
                );
            }
        }
        AppAction::RegisterProject(path) => {
            spawn_call(ui_tx, socket_path, DaemonCall::Project(path))
        }
        AppAction::RenameSession { session_id, title } => {
            if !app.wb.connected || !app.wb.renaming.insert(session_id) {
                if let Some(panel) = app.wb.naming.as_mut() {
                    panel.loading = false;
                    panel.error = Some("Disconnected or rename already in progress.".into());
                }
                return false;
            }
            spawn_call(ui_tx, socket_path, DaemonCall::Rename { session_id, title });
        }
        AppAction::CreateSession {
            workspace,
            agent_id,
        } => {
            if app.wb.creating.is_some() {
                app.set_status("An agent is already being created.");
                return false;
            }
            if !app.wb.connected {
                if let Some(wiz) = app.wizard.as_mut() {
                    wiz.submitting = false;
                    wiz.error = Some("Daemon disconnected. Reopen the TUI to reconnect.".into());
                }
                app.set_error("Daemon disconnected. Reopen the TUI to reconnect.");
                return false;
            }
            app.wb.creating = Some(app.wb.interaction_epoch);
            if let Some(wiz) = app.wizard.as_mut() {
                wiz.error = None;
            }
            let agent_name = app
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| agent_id.to_string());
            let label = match &workspace {
                WorkspacePick::Directory { name, .. } => name.clone(),
                WorkspacePick::Existing { name, .. } => name.clone(),
                WorkspacePick::New { name, .. } => format!("new workspace {name}"),
            };
            spawn_call(
                ui_tx,
                socket_path,
                DaemonCall::Create {
                    epoch: app.wb.interaction_epoch,
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
            Some(session_id) if app.wb.resuming.contains(&session_id) => {
                app.set_status("Agent is already reconnecting…");
            }
            Some(_) if !app.wb.connected => {
                app.set_error("Daemon disconnected. Reopen the TUI to reconnect.");
            }
            Some(_)
                if !app.selected_session().is_some_and(|v| {
                    matches!(
                        v.session.state,
                        agentmux_core::SessionState::Done | agentmux_core::SessionState::Error(_)
                    )
                }) =>
            {
                app.set_status("This task is already active. Type a message to continue.");
            }
            Some(session_id) => {
                app.wb.resuming.insert(session_id);
                spawn_call(ui_tx, socket_path, DaemonCall::Resume { session_id });
                app.set_status("resuming session…");
            }
            None => app.set_status("no session selected"),
        },
        AppAction::RespondPermission {
            session_id,
            request_id,
            outcome,
        } => {
            spawn_call(
                ui_tx,
                socket_path,
                DaemonCall::Permission {
                    session_id,
                    request_id,
                    outcome,
                },
            );
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
                DaemonCall::Info { session, request } => {
                    let result = async {
                        let state = client
                            .pi_command(session, serde_json::json!({"type":"get_state"}))
                            .await
                            .map_err(|error| error.to_string())?;
                        let stats = client
                            .pi_command(session, serde_json::json!({"type":"get_session_stats"}))
                            .await
                            .map_err(|error| error.to_string())?;
                        Ok::<_, String>(serde_json::json!({"state":state,"stats":stats}))
                    }
                    .await;
                    UiMsg::Info {
                        session,
                        request,
                        result,
                    }
                }
                DaemonCall::Context { workspace, request } => UiMsg::Context {
                    workspace,
                    request,
                    saving: false,
                    result: client
                        .workspace_context(workspace)
                        .await
                        .map_err(|error| error.to_string()),
                },
                DaemonCall::SaveContext { params, request } => UiMsg::Context {
                    workspace: params.workspace_id,
                    request,
                    saving: true,
                    result: client
                        .save_workspace_context(params)
                        .await
                        .map_err(|error| error.to_string()),
                },
                DaemonCall::Reference { session, seq } => UiMsg::Reference {
                    session,
                    seq,
                    result: client
                        .read_history_event(session, seq)
                        .await
                        .map_err(|error| error.to_string()),
                },
                DaemonCall::NativeList { session_id, serial } => UiMsg::NativeList {
                    session_id,
                    serial,
                    result: client
                        .native_list(session_id)
                        .await
                        .map_err(|e| e.to_string()),
                },
                DaemonCall::NativeOpen(params) => UiMsg::NativeOpened {
                    source: params.session_id,
                    result: client.native_open(params).await.map_err(|e| e.to_string()),
                },
                DaemonCall::Rename { session_id, title } => UiMsg::Renamed {
                    session_id,
                    result: client
                        .set_session_title(session_id, title)
                        .await
                        .map_err(|e| e.to_string()),
                },
                DaemonCall::Pi {
                    session_id,
                    serial,
                    command,
                } => {
                    let result = client
                        .pi_command(session_id, command.clone())
                        .await
                        .map_err(|e| e.to_string());
                    UiMsg::Pi {
                        session_id,
                        serial,
                        command,
                        result,
                    }
                }
                DaemonCall::Prompt { session_id, prompt } => {
                    let error = client
                        .prompt(
                            session_id,
                            prompt.text.clone(),
                            session_refs(&prompt.references),
                        )
                        .await
                        .err()
                        .map(|e| e.to_string());
                    UiMsg::PromptFinished {
                        session_id,
                        prompt,
                        error,
                    }
                }
                DaemonCall::History { session_id, before } => UiMsg::History {
                    session_id,
                    result: client
                        .history(session_id, before)
                        .await
                        .map_err(|e| e.to_string()),
                },
                DaemonCall::Diff {
                    request,
                    session_id,
                    path,
                    params,
                } => UiMsg::Diff {
                    request,
                    session_id,
                    path: path.clone(),
                    result: client
                        .file_diff_info(params)
                        .await
                        .map_err(|e| e.to_string()),
                },
                DaemonCall::Files {
                    workspace_id,
                    request,
                } => UiMsg::Files {
                    workspace_id,
                    request,
                    result: client
                        .workspace_changes(workspace_id)
                        .await
                        .map_err(|error| error.to_string()),
                },
                DaemonCall::Project(path) => UiMsg::Project(
                    client
                        .register_project(path, None)
                        .await
                        .map_err(|e| e.to_string()),
                ),
                DaemonCall::Cancel { session_id } => match client.cancel(session_id).await {
                    Ok(()) => UiMsg::Status("prompt cancelled".to_string()),
                    Err(e) => UiMsg::Error {
                        session_id: Some(session_id),
                        message: format!("cancel failed: {e}"),
                    },
                },
                DaemonCall::Kill { session_id } => match client.kill(session_id).await {
                    Ok(()) => UiMsg::Status("session killed".to_string()),
                    Err(e) => UiMsg::Error {
                        session_id: Some(session_id),
                        message: format!("kill failed: {e}"),
                    },
                },
                DaemonCall::Resume { session_id } => UiMsg::Resumed {
                    session_id,
                    result: client.resume(session_id).await.map_err(|e| e.to_string()),
                },
                DaemonCall::Permission {
                    session_id,
                    request_id,
                    outcome,
                } => match client
                    .respond_permission(session_id, &request_id, outcome)
                    .await
                {
                    Ok(()) => UiMsg::Status("permission answered".to_string()),
                    Err(e) => UiMsg::PermissionFailed {
                        session_id,
                        request_id,
                        error: format!("{e}"),
                    },
                },
                DaemonCall::Create {
                    epoch,
                    workspace,
                    agent_id,
                    agent_name,
                } => match create_session(&mut client, workspace, agent_id, agent_name).await {
                    Ok((view, new_workspace)) => UiMsg::Created {
                        epoch,
                        view: Box::new(view),
                        new_workspace,
                    },
                    Err(failure) => UiMsg::CreateFailed { epoch, failure },
                },
            },
            Err(e) => match call {
                DaemonCall::Info { session, request } => UiMsg::Info {
                    session,
                    request,
                    result: Err(e.to_string()),
                },
                DaemonCall::Context { workspace, request } => UiMsg::Context {
                    workspace,
                    request,
                    saving: false,
                    result: Err(e.to_string()),
                },
                DaemonCall::SaveContext { params, request } => UiMsg::Context {
                    workspace: params.workspace_id,
                    request,
                    saving: true,
                    result: Err(e.to_string()),
                },
                DaemonCall::Reference { session, seq } => UiMsg::Reference {
                    session,
                    seq,
                    result: Err(e.to_string()),
                },
                DaemonCall::NativeList { session_id, serial } => UiMsg::NativeList {
                    session_id,
                    serial,
                    result: Err(e.to_string()),
                },
                DaemonCall::NativeOpen(params) => UiMsg::NativeOpened {
                    source: params.session_id,
                    result: Err(e.to_string()),
                },
                DaemonCall::Rename { session_id, .. } => UiMsg::Renamed {
                    session_id,
                    result: Err(e.to_string()),
                },
                DaemonCall::Pi {
                    session_id,
                    serial,
                    command,
                } => UiMsg::Pi {
                    session_id,
                    serial,
                    command,
                    result: Err(e.to_string()),
                },
                DaemonCall::Resume { session_id } => UiMsg::Resumed {
                    session_id,
                    result: Err(e.to_string()),
                },
                DaemonCall::Prompt { session_id, prompt } => UiMsg::PromptFinished {
                    session_id,
                    prompt,
                    error: Some(e.to_string()),
                },
                DaemonCall::Permission {
                    session_id,
                    request_id,
                    ..
                } => UiMsg::PermissionFailed {
                    session_id,
                    request_id,
                    error: e.to_string(),
                },
                DaemonCall::History { session_id, .. } => UiMsg::History {
                    session_id,
                    result: Err(e.to_string()),
                },
                DaemonCall::Diff {
                    request,
                    session_id,
                    path,
                    ..
                } => UiMsg::Diff {
                    request,
                    session_id,
                    path,
                    result: Err(e.to_string()),
                },
                DaemonCall::Files {
                    workspace_id,
                    request,
                } => UiMsg::Files {
                    workspace_id,
                    request,
                    result: Err(e.to_string()),
                },
                DaemonCall::Create { epoch, .. } => UiMsg::CreateFailed {
                    epoch,
                    failure: CreateFailure {
                        error: format!("Daemon unreachable: {e}"),
                        new_workspace: None,
                    },
                },
                DaemonCall::Cancel { session_id } | DaemonCall::Kill { session_id } => {
                    UiMsg::Error {
                        session_id: Some(session_id),
                        message: format!("daemon unreachable: {e}"),
                    }
                }
                _ => UiMsg::Error {
                    session_id: None,
                    message: format!("daemon unreachable: {e}"),
                },
            },
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
) -> Result<(SessionView, Option<Workspace>), CreateFailure> {
    let (workspace_id, workspace_name, new_workspace) = match workspace {
        WorkspacePick::Directory { project_id, .. } => client
            .open_workspace(project_id)
            .await
            .map(|ws| (ws.id, ws.name.clone(), Some(ws)))
            .map_err(|e| CreateFailure {
                error: format!("workspace/open failed: {e}"),
                new_workspace: None,
            })?,
        WorkspacePick::Existing { id, name } => (id, name, None),
        WorkspacePick::New { project_id, name } => client
            .create_workspace(project_id, name, None)
            .await
            .map(|ws| (ws.id, ws.name.clone(), Some(ws)))
            .map_err(|e| CreateFailure {
                error: format!("workspace/create failed: {e}"),
                new_workspace: None,
            })?,
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
                new_workspace.clone(),
            )
        })
        .map_err(|e| CreateFailure {
            error: format!("session/create failed: {e}"),
            new_workspace: new_workspace.map(Box::new),
        })
}

/// Apply a spawned-call result to the UI.
fn apply_msg(app: &mut App, msg: UiMsg) {
    match msg {
        UiMsg::Info {
            session,
            request,
            result,
        } => app.info_reply(session, request, result),
        UiMsg::Context {
            workspace,
            request,
            saving,
            result,
        } => app.context_reply(workspace, request, saving, result),
        UiMsg::Reference {
            session,
            seq,
            result,
        } => {
            app.wb.references.loading.remove(&(session, seq));
            app.wb.references.cache.insert((session, seq), result);
        }
        UiMsg::Copy {
            session,
            text,
            result,
        } => match result {
            Ok(()) => app.set_status("Copied original text."),
            Err(error) => match external::retain_copy(&text) {
                Ok(path) => {
                    let location = external::display_path(&path);
                    app.wb.transcript.copy_file = Some(path);
                    app.set_error_for(
                        Some(session),
                        format!(
                            "{error}. Original text retained while this TUI is open: {}",
                            location.display()
                        ),
                    );
                }
                Err(saved) => app.set_error_for(
                    Some(session),
                    format!(
                        "{error}; fallback failed: {saved}. Original message remains readable."
                    ),
                ),
            },
        },
        UiMsg::Files {
            workspace_id,
            request,
            result,
        } => app.apply_files(workspace_id, request, result),
        UiMsg::NativeList {
            session_id,
            serial,
            result,
        } => {
            if let Some(panel) =
                app.wb.pi_panel.as_mut().filter(|p| {
                    p.session_id == session_id && p.serial == serial && p.native_catalog
                })
            {
                panel.loading = false;
                match result {
                    Ok(catalog) => {
                        panel.native_available = catalog.native_available;
                        panel.structured_available = catalog.structured_available;
                        panel.native_mode = catalog.native_available;
                        panel.choices = catalog
                            .conversations
                            .into_iter()
                            .map(|s| commands::Choice {
                                label: s.title,
                                detail: s.session_id.chars().take(12).collect(),
                                command: Some(serde_json::json!({"file":s.session_file})),
                            })
                            .collect();
                        if panel.choices.is_empty() {
                            panel.choices.push(commands::Choice {
                                label: "No native conversations in this directory".into(),
                                detail: String::new(),
                                command: None,
                            });
                        }
                    }
                    Err(error) => {
                        panel.title = "Native history unavailable".into();
                        panel.choices = vec![commands::Choice {
                            label: error,
                            detail: String::new(),
                            command: None,
                        }];
                    }
                }
            }
        }
        UiMsg::NativeOpened { source, result } => {
            app.wb.pi_busy.remove(&source);
            match result {
                Ok(session) => {
                    let native = session.native_terminal;
                    let agent_name = app
                        .agents
                        .iter()
                        .find(|a| a.id == session.agent_id)
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| session.agent_id.to_string());
                    let workspace_name = app
                        .workspaces
                        .iter()
                        .find(|w| w.id == session.workspace_id)
                        .map(|w| w.name.clone())
                        .unwrap_or_else(|| "directory".into());
                    let in_flow = app.selected_session_id() == Some(source)
                        && !app.wb.menu
                        && app.wizard.is_none()
                        && app.wb.naming.is_none()
                        && !app.review_open();
                    let id = session.id;
                    let index =
                        if let Some(i) = app.sessions.iter().position(|v| v.session.id == id) {
                            app.sessions[i].session = session;
                            i
                        } else {
                            app.add_session(SessionView {
                                session,
                                agent_name,
                                workspace_name,
                            })
                        };
                    if in_flow {
                        app.wb.pi_panel = None;
                        app.select_session(index);
                        app.mode = app::InputMode::Editing;
                        if native {
                            app.wb.native_attach = Some(id);
                        }
                    } else {
                        app.unread.insert(id);
                    }
                    app.set_status("Native conversation opened.");
                }
                Err(error) => {
                    if let Some(panel) = app.wb.pi_panel.as_mut() {
                        panel.loading = false;
                    }
                    app.set_error_for(
                        Some(source),
                        format!("Native open failed: {error}. Original conversation retained."),
                    );
                }
            }
        }
        UiMsg::Renamed { session_id, result } => {
            app.wb.renaming.remove(&session_id);
            match result {
                Ok(event) => {
                    app.handle_event(event);
                    if app
                        .wb
                        .naming
                        .as_ref()
                        .is_some_and(|p| p.session_id == session_id && p.loading)
                    {
                        app.wb.naming = None;
                    }
                    app.set_status("Agent conversation renamed.");
                }
                Err(error) => {
                    if let Some(panel) = app
                        .wb
                        .naming
                        .as_mut()
                        .filter(|p| p.session_id == session_id && p.loading)
                    {
                        panel.loading = false;
                        panel.error = Some(error.clone());
                    }
                    app.set_error_for(Some(session_id), format!("Rename failed: {error}"));
                }
            }
        }
        UiMsg::Recovered { session_id, result } => match result {
            Ok(page) => recovery::apply(app, session_id, page),
            Err(error) => app.set_error_for(
                Some(session_id),
                format!("Output sync failed; retrying: {error}"),
            ),
        },
        UiMsg::Pi {
            session_id,
            serial,
            command,
            result,
        } => match serial {
            Some(serial) => commands::apply_reply(app, session_id, serial, &command, result),
            None => commands::apply_catalog(app, session_id, result),
        },
        UiMsg::Resumed { session_id, result } => {
            app.wb.pi_loaded.remove(&session_id);
            app.wb.pi_commands.remove(&session_id);
            app.wb.resuming.remove(&session_id);
            match result {
                Ok(()) => {
                    if app.selected_session_id() == Some(session_id)
                        && app.selected_is_native()
                        && !app.review_open()
                    {
                        app.wb.native_attach = Some(session_id);
                    }
                    app.set_status("Agent reconnected. Queued messages will send when ready.");
                }
                Err(error) => {
                    if let Some(mut queue) = app.wb.queues.remove(&session_id) {
                        let first = queue.pop_front();
                        app.wb.failed.entry(session_id).or_default().extend(queue);
                        if let Some(prompt) = first {
                            app.restore_prompt(session_id, prompt);
                        }
                    }
                    app.set_error_for(
                        Some(session_id),
                        format!(
                        "Restart failed: {error}. Messages kept; Menu → Recover failed message."
                    ),
                    );
                }
            }
        }
        UiMsg::PromptFinished {
            session_id,
            prompt,
            error,
        } => {
            app.wb.in_flight.remove(&session_id);
            if let Some(error) = error {
                if let Some(queue) = app.wb.queues.remove(&session_id) {
                    app.wb.failed.entry(session_id).or_default().extend(queue);
                }
                app.restore_prompt(session_id, prompt);
                app.set_error_for(
                    Some(session_id),
                    format!(
                        "prompt failed: {error} · draft kept; /recover for other pending drafts"
                    ),
                );
            } else {
                app.set_status("turn finished");
            }
        }
        UiMsg::History { session_id, result } => match result {
            Ok(page) => app.merge_history_page(session_id, page),
            Err(e) => {
                let ui = app.wb.sessions.entry(session_id).or_default();
                ui.loading = false;
                // Do not let the redraw of this error immediately enqueue
                // the same failed prefetch again.
                ui.history_retry_after = Some(std::time::Instant::now() + Duration::from_secs(2));
                app.set_error_for(Some(session_id), format!("history failed: {e}"));
            }
        },
        UiMsg::Diff {
            request,
            session_id,
            path,
            result,
        } => {
            if app.selected_session_id() == Some(session_id)
                && app.wb.pending_diff == Some((request, session_id, path.clone()))
                && matches!(
                    app.mode,
                    app::InputMode::Editing | app::InputMode::Normal | app::InputMode::Sidebar
                )
                && !app.wb.menu
                && !app.review_open()
            {
                app.wb.pending_diff = None;
                match result {
                    Ok(diff) => {
                        if app
                            .wb
                            .files
                            .diff_params
                            .as_ref()
                            .is_some_and(|params| params.scope != diff.scope)
                        {
                            app.wb.files.diff_scope = app.wb.files.shown_scope;
                            app.set_error(
                                "The daemon does not support this diff scope; previous view kept.",
                            );
                            return;
                        }
                        let same = app
                            .wb
                            .inspection
                            .as_ref()
                            .is_some_and(|(current, _)| current == &path);
                        app.wb.files.diff_binary = diff.binary;
                        app.wb.files.diff_truncated = diff.truncated;
                        app.wb.files.shown_scope = diff.scope;
                        app.wb.inspection = Some((path, diff.text));
                        if !same {
                            app.wb.inspect_scroll = 0;
                        }
                        focus::select(app, focus::Pane::Reading);
                        app.wb.drawer = false;
                        app.status = None;
                    }
                    Err(e) => app.set_error_for(Some(session_id), format!("diff failed: {e}")),
                }
            }
        }
        UiMsg::Project(result) => match result {
            Ok(project) => {
                let id = project.id;
                if !app.projects.iter().any(|p| p.id == project.id) {
                    app.projects.push(project);
                }
                let new_space_row = app.workspaces.iter().filter(|w| w.project_id == id).count();
                if let Some(wiz) = app.wizard.as_mut().filter(|w| w.registering) {
                    wiz.registering = false;
                    wiz.project_id = Some(id);
                    wiz.step = newsession::WizardStep::Workspace;
                    wiz.workspace_cursor = new_space_row;
                }
                app.set_status("Project added. Choose or create a space.");
            }
            Err(e) => {
                if let Some(wiz) = app.wizard.as_mut() {
                    wiz.registering = false;
                }
                app.set_error_for(None, format!("project registration failed: {e}"));
            }
        },
        UiMsg::Status(s) => app.set_status(s),
        UiMsg::Error {
            session_id,
            message,
        } => app.set_error_for(session_id, message),
        UiMsg::Created {
            epoch,
            view,
            new_workspace,
        } => {
            let in_flow = app.wb.creating == Some(epoch)
                && app.mode == app::InputMode::NewSession
                && app.wizard.as_ref().is_some_and(|w| w.submitting);
            if app.wb.creating == Some(epoch) {
                app.wb.creating = None;
            }
            // A wizard-created workspace joins the grouping list so the
            // new session renders under its header.
            if let Some(ws) = new_workspace {
                if !app.workspaces.iter().any(|w| w.id == ws.id) {
                    app.workspaces.push(ws);
                }
            }
            let index = app.add_session(*view);
            if in_flow
                || (epoch == app.wb.interaction_epoch
                    && !app.wb.menu
                    && matches!(app.mode, app::InputMode::Editing | app::InputMode::Normal))
            {
                app.wizard = None;
                app.select_session(index);
                app.mode = app::InputMode::Editing;
                if app.selected_is_native()
                    && app.sessions[index].session.state == agentmux_core::SessionState::Ready
                {
                    app.wb.native_attach = Some(app.sessions[index].session.id);
                }
                let status = match &app.sessions[index].session.state {
                    agentmux_core::SessionState::Error(reason) => {
                        format!("Agent connection failed: {reason}. Resume retries this agent.")
                    }
                    agentmux_core::SessionState::Ready => "Agent ready.".into(),
                    _ => "Agent added; connecting...".into(),
                };
                if matches!(
                    app.sessions[index].session.state,
                    agentmux_core::SessionState::Error(_)
                ) {
                    app.set_error_for(Some(app.sessions[index].session.id), status);
                } else {
                    app.set_status(status);
                }
            } else {
                app.unread.insert(app.sessions[index].session.id);
                app.set_status("Agent added in background — select it from Agents.");
            }
        }
        UiMsg::CreateFailed { epoch, failure } => {
            if app.wb.creating == Some(epoch) {
                app.wb.creating = None;
                if let Some(wiz) = app.wizard.as_mut().filter(|w| w.submitting) {
                    wiz.submitting = false;
                    wiz.error = Some(failure.error.clone());
                    if let Some(ws) = &failure.new_workspace {
                        wiz.workspace = Some(WorkspacePick::Existing {
                            id: ws.id,
                            name: ws.name.clone(),
                        });
                    }
                }
            }
            if let Some(ws) = failure.new_workspace {
                if !app.workspaces.iter().any(|w| w.id == ws.id) {
                    app.workspaces.push(*ws);
                }
            }
            app.set_error_for(
                None,
                format!("{}; retry from Add agent or New space.", failure.error),
            );
        }
        UiMsg::PermissionFailed {
            session_id,
            request_id,
            error,
        } => {
            app.permission_answer_failed(session_id, &request_id, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_options_are_explicit() {
        assert_eq!(parse_options([]).unwrap(), StartupOptions::default());
        assert!(
            parse_options(["--restart-daemon".into()])
                .unwrap()
                .restart_daemon
        );
        assert!(
            parse_options(["--restart-daemon".into(), "--help".into()])
                .unwrap()
                .help
        );
        assert!(parse_options(["--restart-deamon".into()]).is_err());
    }
    use agentmux_core::rpc::{SessionPromptParams, M_SESSION_PROMPT};

    fn session_view() -> SessionView {
        SessionView {
            session: agentmux_core::Session {
                id: SessionId::new(),
                workspace_id: agentmux_core::WorkspaceId::new(),
                agent_id: AgentId::new("mock"),
                state: agentmux_core::SessionState::Ready,
                acp_session_id: None,
                native_session_file: None,
                native_terminal: false,
                references: vec![],
                created_at: chrono::Utc::now(),
            },
            agent_name: "Mock".into(),
            workspace_name: "main".into(),
        }
    }

    #[test]
    fn delayed_creation_preserves_new_input_and_selection() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let selected = app.selected_session_id();
        let epoch = app.wb.interaction_epoch;
        app.mode = app::InputMode::Editing;
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        ));
        let new_view = session_view();
        let new_id = new_view.session.id;
        apply_msg(
            &mut app,
            UiMsg::Created {
                epoch,
                view: Box::new(new_view),
                new_workspace: None,
            },
        );
        assert_eq!(app.selected_session_id(), selected);
        assert_eq!(app.input, "x");
        assert!(app.unread.contains(&new_id));
    }

    #[test]
    fn asynchronous_failure_keeps_its_source_and_survives_unrelated_success() {
        let first = session_view();
        let mut second = session_view();
        second.session.state = agentmux_core::SessionState::Error("daemon restarted".into());
        let first_id = first.session.id;
        let second_id = second.session.id;
        let mut app = App::new(vec![], vec![], vec![first, second], vec![]);
        app.insert_text("current draft");
        app.wb
            .queues
            .entry(second_id)
            .or_default()
            .push_back(Prompt {
                text: "queued draft".into(),
                references: vec![],
            });
        apply_msg(
            &mut app,
            UiMsg::Resumed {
                session_id: second_id,
                result: Err("adapter cannot restore\nfull error detail".into()),
            },
        );
        apply_msg(
            &mut app,
            UiMsg::Status("unrelated operation succeeded".into()),
        );
        let feedback = app.wb.attention.latest_error().unwrap();
        assert_eq!(feedback.session, Some(second_id));
        assert!(feedback.message.contains("full error detail"));
        assert_eq!(app.selected_session_id(), Some(first_id));
        assert_eq!(app.input, "current draft");
        assert_eq!(app.attention_items().len(), 1);
        assert_eq!(app.wb.failed[&second_id][0].text, "queued draft");
    }

    #[test]
    fn native_resume_does_not_attach_over_an_open_attention_panel() {
        let mut view = session_view();
        view.session.native_terminal = true;
        let id = view.session.id;
        let mut app = App::new(vec![], vec![], vec![view], vec![]);
        app.insert_text("draft");
        attention::open(&mut app);
        apply_msg(
            &mut app,
            UiMsg::Resumed {
                session_id: id,
                result: Ok(()),
            },
        );
        assert!(app.wb.native_attach.is_none());
        assert!(app.wb.attention.panel.is_some());
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn creation_selects_new_task_when_user_has_not_moved_on() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let epoch = app.wb.interaction_epoch;
        let new_view = session_view();
        let new_id = new_view.session.id;
        apply_msg(
            &mut app,
            UiMsg::Created {
                epoch,
                view: Box::new(new_view),
                new_workspace: None,
            },
        );
        assert_eq!(app.selected_session_id(), Some(new_id));
        assert_eq!(app.mode, app::InputMode::Editing);
    }

    #[test]
    fn creation_finishes_in_flow_and_preserves_original_draft() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let old_id = app.selected_session_id().unwrap();
        app.insert_text("source draft");
        app.start_add_agent();
        app.wizard.as_mut().unwrap().submitting = true;
        app.wb.creating = Some(10);
        app.wb.interaction_epoch = 11;
        let mut new_view = session_view();
        new_view.session.workspace_id = app.sessions[0].session.workspace_id;
        let id = new_view.session.id;
        apply_msg(
            &mut app,
            UiMsg::Created {
                epoch: 10,
                view: Box::new(new_view),
                new_workspace: None,
            },
        );
        assert_eq!(app.selected_session_id(), Some(id));
        assert!(app.wizard.is_none());
        assert!(app.wb.creating.is_none());
        assert!(app.input.is_empty());
        let index = app
            .sessions
            .iter()
            .position(|v| v.session.id == old_id)
            .unwrap();
        app.select_session(index);
        assert_eq!(app.input, "source draft");
    }

    #[test]
    fn closed_creation_does_not_reopen_or_steal_selection() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let old = app.selected_session_id();
        app.start_add_agent();
        app.wizard.as_mut().unwrap().submitting = true;
        let epoch = app.wb.interaction_epoch;
        app.wb.creating = Some(epoch);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        app.insert_text("new draft");
        let view = session_view();
        let id = view.session.id;
        apply_msg(
            &mut app,
            UiMsg::Created {
                epoch,
                view: Box::new(view),
                new_workspace: None,
            },
        );
        assert_eq!(app.selected_session_id(), old);
        assert_eq!(app.input, "new draft");
        assert!(app.unread.contains(&id));
        assert!(app.wizard.is_none());
        assert!(app.wb.creating.is_none());
    }

    #[test]
    fn partial_creation_failure_reuses_workspace_on_retry() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        app.start_add_agent();
        app.wizard.as_mut().unwrap().submitting = true;
        app.wb.creating = Some(1);
        let ws = Workspace {
            id: agentmux_core::WorkspaceId::new(),
            project_id: agentmux_core::ProjectId::new(),
            name: "created-space".into(),
            worktree_path: "/tmp/created-space".into(),
            branch: "agentmux/created-space".into(),
            managed_worktree: true,
            created_at: chrono::Utc::now(),
        };
        apply_msg(
            &mut app,
            UiMsg::CreateFailed {
                epoch: 1,
                failure: CreateFailure {
                    error: "session/create failed".into(),
                    new_workspace: Some(Box::new(ws.clone())),
                },
            },
        );
        assert!(app.wb.creating.is_none());
        let wizard = app.wizard.as_ref().unwrap();
        assert!(!wizard.submitting);
        assert!(wizard.error.is_some());
        assert_eq!(
            wizard.workspace,
            Some(WorkspacePick::Existing {
                id: ws.id,
                name: ws.name.clone()
            })
        );
        assert_eq!(app.workspaces, vec![ws]);
    }

    #[test]
    fn pending_creation_blocks_duplicate_requests_even_after_close() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        app.wb.creating = Some(1);
        app.start_add_agent();
        assert!(app.wizard.is_none());
        let (tx, _rx) = mpsc::channel(1);
        assert!(!dispatch(
            AppAction::CreateSession {
                workspace: WorkspacePick::Existing {
                    id: app.sessions[0].session.workspace_id,
                    name: "main".into()
                },
                agent_id: AgentId::new("mock"),
            },
            &mut app,
            &tx,
            Path::new("/unused")
        ));
        assert_eq!(app.wb.creating, Some(1));
    }

    #[test]
    fn resume_active_task_does_not_call_daemon() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let (tx, _rx) = mpsc::channel(1);
        // This synchronous test would panic if dispatch spawned an RPC task.
        assert!(!dispatch(
            AppAction::ResumeSession,
            &mut app,
            &tx,
            Path::new("/unused")
        ));
        assert!(app.status.as_deref().unwrap().contains("already active"));
    }

    #[tokio::test]
    async fn sending_to_terminated_agent_waits_for_resume_and_ready() {
        for ready_first in [true, false] {
            let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
            let sid = app.sessions[0].session.id;
            app.sessions[0].session.state =
                agentmux_core::SessionState::Error("daemon restarted".into());
            let (tx, _rx) = mpsc::channel(4);
            for text in ["first", "second"] {
                dispatch(
                    AppAction::Submit {
                        text: text.into(),
                        references: vec![],
                    },
                    &mut app,
                    &tx,
                    Path::new("/unused"),
                );
            }
            assert!(app.wb.resuming.contains(&sid));
            assert_eq!(app.wb.queues[&sid].len(), 2);
            if ready_first {
                app.sessions[0].session.state = agentmux_core::SessionState::Ready;
            }
            assert!(app.ready_queued().is_none());
            apply_msg(
                &mut app,
                UiMsg::Resumed {
                    session_id: sid,
                    result: Ok(()),
                },
            );
            if !ready_first {
                assert!(app.ready_queued().is_none());
                app.sessions[0].session.state = agentmux_core::SessionState::Ready;
            }
            let (target, prompt) = app.ready_queued().unwrap();
            assert_eq!(target, sid);
            assert_eq!(prompt.text, "first");
            assert!(app.ready_queued().is_none(), "do not send a prompt twice");
            assert_eq!(app.wb.queues[&sid][0].text, "second");
        }
    }

    #[tokio::test]
    async fn resume_failure_preserves_messages_and_newer_draft() {
        let mut app = App::new(vec![], vec![], vec![session_view(), session_view()], vec![]);
        let sid = app.sessions[0].session.id;
        app.sessions[0].session.state = agentmux_core::SessionState::Done;
        let (tx, _rx) = mpsc::channel(4);
        dispatch(
            AppAction::Submit {
                text: "keep this".into(),
                references: vec![],
            },
            &mut app,
            &tx,
            Path::new("/unused"),
        );
        app.select_session(1);
        app.insert_text("another agent's draft");
        apply_msg(
            &mut app,
            UiMsg::Resumed {
                session_id: sid,
                result: Err("agent executable missing".into()),
            },
        );
        assert_eq!(app.input, "another agent's draft");
        assert_eq!(app.wb.failed[&sid][0].text, "keep this");
        assert!(!app.wb.resuming.contains(&sid));
        assert!(!app.wb.queues.contains_key(&sid));
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .contains("agent executable missing"));
    }

    #[tokio::test]
    async fn manual_resume_leaves_draft_unsent_and_deduplicates_requests() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let sid = app.sessions[0].session.id;
        app.sessions[0].session.state = agentmux_core::SessionState::Done;
        app.insert_text("unsent draft");
        let (tx, _rx) = mpsc::channel(4);
        for _ in 0..2 {
            dispatch(
                AppAction::ResumeSession,
                &mut app,
                &tx,
                Path::new("/unused"),
            );
        }
        assert_eq!(app.wb.resuming.len(), 1);
        assert!(app.wb.resuming.contains(&sid));
        assert_eq!(app.input, "unsent draft");
        assert!(app.wb.queues.is_empty());
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .contains("already reconnecting"));
    }

    #[test]
    fn disconnected_send_preserves_draft_without_starting_resume() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        app.wb.connected = false;
        let (tx, _rx) = mpsc::channel(1);
        dispatch(
            AppAction::Submit {
                text: "keep draft".into(),
                references: vec![],
            },
            &mut app,
            &tx,
            Path::new("/unused"),
        );
        assert_eq!(app.input, "keep draft");
        assert!(app.wb.resuming.is_empty());
        assert!(app.wb.queues.is_empty());
        assert!(app.status.as_deref().unwrap().contains("disconnected"));
    }

    #[tokio::test]
    async fn unreachable_daemon_during_resume_restores_submitted_text() {
        let mut app = App::new(vec![], vec![], vec![session_view()], vec![]);
        let sid = app.sessions[0].session.id;
        app.sessions[0].session.state =
            agentmux_core::SessionState::Error("daemon restarted".into());
        let (tx, mut rx) = mpsc::channel(1);
        let missing_socket = std::env::temp_dir().join(format!("agentmux-missing-{sid}.sock"));
        dispatch(
            AppAction::Submit {
                text: "你是谁".into(),
                references: vec![],
            },
            &mut app,
            &tx,
            &missing_socket,
        );
        let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&msg, UiMsg::Resumed { result: Err(_), .. }));
        apply_msg(&mut app, msg);
        assert_eq!(app.input, "你是谁");
        assert!(!app.wb.resuming.contains(&sid));
        assert!(!app.wb.queues.contains_key(&sid));
    }

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
