//! Agent slash completion and Pi RPC controls. With an agent selected, workbench
//! commands always use `/mux ...`; unqualified slash commands belong to agents.
use crate::{
    app::{App, AppAction, InputMode},
    interaction::{buttons, hit, Target},
    theme::THEME,
};
use agentmux_core::{AdapterKind, SessionId};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
    Frame,
};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

pub fn observe_acp_commands(app: &mut App, event: &agentmux_core::Event) {
    let Some(items) = event.available_commands() else {
        return;
    };
    if app
        .wb
        .acp_commands
        .get(&event.session_id)
        .is_some_and(|(seq, _)| *seq >= event.seq)
    {
        return;
    }
    if app
        .wb
        .sessions
        .get(&event.session_id)
        .is_some_and(|ui| event.seq < ui.conversation_start)
    {
        return;
    }
    let mut commands: Vec<SlashCommand> = Vec::new();
    for item in items {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = name.trim_start_matches('/');
        if name.is_empty()
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
            || name == "mux"
        {
            continue;
        }
        let name = format!("/{name}");
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let hint = item
            .pointer("/input/hint")
            .and_then(Value::as_str)
            .unwrap_or("");
        let description = if hint.is_empty() {
            description.into()
        } else {
            format!("{description} {hint}")
        };
        let command = SlashCommand {
            name: name.clone(),
            description,
        };
        if let Some(existing) = commands.iter_mut().find(|c| c.name == name) {
            *existing = command;
        } else {
            commands.push(command);
        }
    }
    app.wb
        .acp_commands
        .insert(event.session_id, (event.seq, commands));
}
#[derive(Clone)]
pub struct Choice {
    pub label: String,
    pub detail: String,
    pub command: Option<Value>,
}
pub struct PiPanel {
    pub native_catalog: bool,
    pub native_mode: bool,
    pub native_available: bool,
    pub structured_available: bool,
    pub session_id: SessionId,
    pub serial: u64,
    pub title: String,
    pub choices: Vec<Choice>,
    pub query: String,
    pub cursor: usize,
    pub loading: bool,
}
impl App {
    pub fn selected_is_opencode_acp(&self) -> bool {
        self.selected_session().is_some_and(|s| self.agents.iter().any(|a| {
            a.id == s.session.agent_id && matches!(&a.adapter, AdapterKind::Acp { command, .. }
                if a.id.0 == "opencode" || command.file_name().is_some_and(|name| name == "opencode"))
        }))
    }
    pub fn selected_is_pi(&self) -> bool {
        self.selected_session().is_some_and(|s| {
            self.agents.iter().any(|a| {
                a.id == s.session.agent_id && matches!(a.adapter, AdapterKind::PiRpc { .. })
            })
        })
    }
}

pub fn suggestions(app: &App) -> Vec<SlashCommand> {
    if app.mode != InputMode::Editing
        || app.wb.menu
        || app.wb.naming.is_some()
        || app.wb.pi_panel.is_some()
        || app.wb.attention.panel.is_some()
        || app.wb.transcript.panel.is_some()
        || app.wb.references.panel.is_some()
        || app.wb.info.panel.is_some()
        || app.wb.context.panel.is_some()
        || !app.input.starts_with('/')
        || app.input.chars().any(char::is_whitespace)
        || app.wb.slash_dismissed.as_deref() == Some(app.input.as_str())
    {
        return vec![];
    }
    let mut choices = Vec::new();
    if app.selected_is_pi() {
        for (name, description) in [
            ("model", "Pi · choose a model"),
            ("thinking", "Pi · reasoning level"),
            ("settings", "Pi · model and runtime settings"),
            ("compact", "Pi · compact conversation [instructions]"),
            ("session", "Pi · session statistics"),
            ("name", "Pi · set session name <name>"),
            ("new", "Pi · new conversation in this workspace"),
            ("resume", "Pi · native conversation history"),
        ] {
            choices.push(SlashCommand {
                name: format!("/{name}"),
                description: description.into(),
            });
        }
        if let Some(items) = app
            .selected_session_id()
            .and_then(|id| app.wb.pi_commands.get(&id))
        {
            for item in items {
                if let Some(existing) = choices.iter_mut().find(|c| c.name == item.name) {
                    *existing = item.clone();
                } else {
                    choices.push(item.clone());
                }
            }
        }
        choices.push(SlashCommand {
            name: "/mux".into(),
            description: "Workbench: /mux add-agent, /mux new, /mux resume".into(),
        });
    } else if app.selected_session_id().is_some() {
        if let Some((_, commands)) = app
            .selected_session_id()
            .and_then(|id| app.wb.acp_commands.get(&id))
        {
            choices.extend(commands.iter().cloned());
        }
        if app.selected_is_opencode_acp() {
            for &(name, description) in opencode_commands() {
                let name = format!("/{name}");
                if !choices.iter().any(|command| command.name == name) {
                    choices.push(SlashCommand {
                        name,
                        description: description.into(),
                    });
                }
            }
        }
        choices.push(SlashCommand {
            name: "/mux".into(),
            description: "Workbench: /mux add-agent, /mux new, /mux resume".into(),
        });
    } else {
        for name in [
            "new",
            "add-agent",
            "rename",
            "tasks",
            "files",
            "context",
            "permissions",
            "attention",
            "error",
            "resume",
            "cancel",
            "help",
            "quit",
        ] {
            choices.push(SlashCommand {
                name: format!("/{name}"),
                description: "Workbench".into(),
            });
        }
    }
    let query = app.input.to_lowercase();
    choices
        .into_iter()
        .filter(|c| c.name.to_lowercase().starts_with(&query))
        .collect()
}

/// Called on Enter. Only explicit workbench commands are consumed locally;
/// unknown agent commands retain their leading slash and are sent verbatim.
pub fn submit(app: &mut App) -> AppAction {
    let input = app.input.trim().to_string();
    let (name, argument) = input
        .split_once(char::is_whitespace)
        .unwrap_or((&input, ""));
    let argument = argument.trim();
    let local = if name == "/mux" {
        Some(format!("/{}", argument.trim_start_matches('/')))
    } else if app.selected_session_id().is_none() {
        Some(input.clone())
    } else {
        None
    };
    if let Some(command) = local {
        if command == "/" {
            app.input.clear();
            app.wb.cursor = 0;
            return app.command("/help");
        }
        let local_name = command.split_whitespace().next().unwrap_or("");
        if is_local(local_name) {
            app.input.clear();
            app.wb.cursor = 0;
            return app.command(&command);
        }
        if name == "/mux" {
            app.set_status("Unknown workbench command. Open Menu or use /mux help.");
            return AppAction::None;
        }
    }
    let acp_registered = app
        .selected_session_id()
        .and_then(|id| app.wb.acp_commands.get(&id))
        .is_some_and(|(_, commands)| commands.iter().any(|c| c.name == name));
    if app.selected_is_opencode_acp() && !acp_registered {
        match name {
            "/new" | "/clear" => {
                if let Some(view) = app.selected_session().cloned() {
                    if !argument.is_empty() {
                        app.set_status("This OpenCode command takes no arguments. Draft kept.");
                        return AppAction::None;
                    }
                    app.input.clear();
                    app.wb.cursor = 0;
                    return AppAction::CreateSession {
                        workspace: crate::newsession::WorkspacePick::Existing {
                            id: view.session.workspace_id,
                            name: view.workspace_name,
                        },
                        agent_id: view.session.agent_id,
                    };
                }
            }
            "/exit" | "/quit" | "/q" if argument.is_empty() => {
                app.input.clear();
                app.wb.cursor = 0;
                return AppAction::KillSession;
            }
            "/compact" if !argument.is_empty() => {
                app.set_status("OpenCode /compact takes no arguments. Draft kept.");
                return AppAction::None;
            }
            _ if opencode_commands()
                .iter()
                .any(|(command, _)| name == format!("/{command}"))
                && name != "/compact" =>
            {
                app.set_status(format!("{name} requires OpenCode's native UI. Use /mux add-agent and select OpenCode (native). Draft kept."));
                return AppAction::None;
            }
            _ => {}
        }
    }
    let registered = app
        .selected_session_id()
        .and_then(|id| app.wb.pi_commands.get(&id))
        .is_some_and(|items| items.iter().any(|c| c.name == name));
    if app.selected_is_pi() && !registered {
        let request = match name {
            "/model" if argument.is_empty() => Some(json!({"type":"get_available_models"})),
            "/model" => match argument.split_once('/') {
                Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                    Some(json!({"type":"set_model", "provider":provider,"modelId":model}))
                }
                _ => {
                    app.set_status("Use /model to choose, or /model provider/model-id.");
                    return AppAction::None;
                }
            },
            "/thinking" if argument.is_empty() => {
                Some(json!({"type":"get_available_thinking_levels"}))
            }
            "/thinking" => Some(json!({"type":"set_thinking_level", "level":argument})),
            "/settings" => Some(json!({"type":"get_state"})),
            "/session" => Some(json!({"type":"get_session_stats"})),
            "/compact" => Some(json!({"type":"compact", "customInstructions":argument})),
            "/name" if !argument.is_empty() => {
                Some(json!({"type":"set_session_name", "name":argument}))
            }
            "/name" => {
                app.set_status("Use /name followed by a session name.");
                return AppAction::None;
            }
            "/new" => {
                if let Some(view) = app.selected_session().cloned() {
                    app.input.clear();
                    app.wb.cursor = 0;
                    return AppAction::CreateSession {
                        workspace: crate::newsession::WorkspacePick::Existing {
                            id: view.session.workspace_id,
                            name: view.workspace_name,
                        },
                        agent_id: view.session.agent_id,
                    };
                }
                None
            }
            "/resume" => {
                app.input.clear();
                app.wb.cursor = 0;
                if argument.is_empty() {
                    return AppAction::BrowseNative;
                }
                if let Some(session_id) = app.selected_session_id() {
                    return AppAction::OpenNative {
                        session_id,
                        session_file: Some(argument.into()),
                        native: false,
                        history: false,
                    };
                }
                None
            }
            "/quit" => {
                app.input.clear();
                app.wb.cursor = 0;
                return AppAction::KillSession;
            }
            "/help" | "/hotkeys" | "/bug" | "/import" | "/clone" | "/trust" | "/copy"
            | "/debug" | "/tree" | "/fork" | "/login" | "/logout" | "/reload" | "/share"
            | "/export" | "/changelog" | "/scoped-models" => {
                app.set_status(format!("{name} uses Pi's native interface. Open native mode with /mux native, then run the command there. Draft kept."));
                return AppAction::None;
            }
            _ => None,
        };
        if let Some(command) = request {
            app.input.clear();
            app.wb.cursor = 0;
            return AppAction::Pi(command);
        }
    }
    if app.selected_session_id().is_none() || !app.wb.connected {
        app.set_status("Select a connected task before sending an agent command. Draft kept.");
        return AppAction::None;
    }
    if !app.pending_relays.is_empty() {
        app.set_status(
            "Send quoted references in a normal message before running an agent command.",
        );
        return AppAction::None;
    }
    app.wb.cursor = 0;
    AppAction::Submit {
        text: std::mem::take(&mut app.input),
        references: vec![],
    }
}

fn opencode_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("compact", "OpenCode · compact conversation (ACP)"),
        ("new", "OpenCode · new conversation"),
        ("clear", "OpenCode · new conversation"),
        ("exit", "OpenCode · stop this agent"),
        ("quit", "OpenCode · stop this agent"),
        ("q", "OpenCode · stop this agent"),
        ("models", "OpenCode · choose model (native UI)"),
        ("agents", "OpenCode · choose agent (native UI)"),
        ("variants", "OpenCode · model variant (native UI)"),
        ("thinking", "OpenCode · model variant (native UI)"),
        ("effort", "OpenCode · model variant (native UI)"),
        ("sessions", "OpenCode · conversation history (native UI)"),
        ("resume", "OpenCode · conversation history (native UI)"),
        ("continue", "OpenCode · conversation history (native UI)"),
        ("rename", "OpenCode · rename conversation (native UI)"),
        ("fork", "OpenCode · fork conversation (native UI)"),
        ("timeline", "OpenCode · jump to message (native UI)"),
        ("undo", "OpenCode · undo message (native UI)"),
        ("redo", "OpenCode · redo message (native UI)"),
        ("share", "OpenCode · share conversation (native UI)"),
        ("unshare", "OpenCode · unshare conversation (native UI)"),
        ("copy", "OpenCode · copy transcript (native UI)"),
        ("export", "OpenCode · export transcript (native UI)"),
        ("editor", "OpenCode · external editor (native UI)"),
        ("skills", "OpenCode · skills (native UI)"),
        ("connect", "OpenCode · connect provider (native UI)"),
        ("mcps", "OpenCode · MCP servers (native UI)"),
        ("settings", "OpenCode · settings (native UI)"),
        ("status", "OpenCode · status (native UI)"),
        ("themes", "OpenCode · theme (native UI)"),
        ("reload", "OpenCode · reload configuration (native UI)"),
        ("help", "OpenCode · help (native UI)"),
        ("debug", "OpenCode · debug information (native UI)"),
        ("cd", "OpenCode · working directory (native UI)"),
        ("worktrees", "OpenCode · manage workspaces (native UI)"),
        ("open", "OpenCode · open project (native UI)"),
        ("projects", "OpenCode · open project (native UI)"),
        ("project", "OpenCode · open project (native UI)"),
        ("terminal", "OpenCode · terminal (native UI)"),
        ("pair", "OpenCode · pair device (native UI)"),
        ("web", "OpenCode · pair device (native UI)"),
        ("restart", "OpenCode · restart service (native UI)"),
        ("update", "OpenCode · update (native UI)"),
    ]
}
fn is_local(name: &str) -> bool {
    matches!(
        name,
        "/terminal"
            | "/history"
            | "/native"
            | "/new"
            | "/add-agent"
            | "/rename"
            | "/project"
            | "/tasks"
            | "/info"
            | "/refresh-info"
            | "/search"
            | "/messages"
            | "/edit-draft"
            | "/previous-message"
            | "/next-message"
            | "/focus"
            | "/sidebar"
            | "/files"
            | "/file-search"
            | "/refresh-files"
            | "/context"
            | "/context-preview"
            | "/refresh-context"
            | "/edit-context"
            | "/save-context"
            | "/references"
            | "/remove-reference"
            | "/permissions"
            | "/attention"
            | "/error"
            | "/relay"
            | "/cancel"
            | "/resume"
            | "/kill"
            | "/latest"
            | "/tools"
            | "/thinking"
            | "/unqueue"
            | "/recover"
            | "/help"
            | "/quit"
            | "/"
    )
}

pub fn filtered(panel: &PiPanel) -> Vec<usize> {
    let q = panel.query.to_lowercase();
    panel
        .choices
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            format!("{} {}", c.label, c.detail)
                .to_lowercase()
                .contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}
pub fn choose(app: &mut App, index: usize) -> AppAction {
    if let Some(panel) = &app.wb.pi_panel {
        if Some(panel.session_id) != app.selected_session_id() {
            return AppAction::None;
        }
        if panel.native_catalog {
            if let Some(file) = panel
                .choices
                .get(index)
                .and_then(|c| c.command.as_ref())
                .and_then(|v| v["file"].as_str())
            {
                return AppAction::OpenNative {
                    session_id: panel.session_id,
                    session_file: Some(file.into()),
                    native: panel.native_mode,
                    history: false,
                };
            }
            return AppAction::None;
        }
        return panel
            .choices
            .get(index)
            .and_then(|c| c.command.clone())
            .map(AppAction::Pi)
            .unwrap_or(AppAction::None);
    }
    if let Some(choice) = suggestions(app).get(index) {
        app.input = choice.name.clone();
        app.wb.cursor = app.input.len();
        app.wb.slash_cursor = 0;
        return submit(app);
    }
    AppAction::None
}
pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    if app.mode != InputMode::Editing || app.wb.menu || app.wb.attention.panel.is_some() {
        return None;
    }
    if let Some(panel) = app.wb.pi_panel.as_mut() {
        match key.code {
            KeyCode::Left if panel.native_catalog && panel.structured_available => {
                panel.native_mode = false
            }
            KeyCode::Right if panel.native_catalog && panel.native_available => {
                panel.native_mode = true
            }
            KeyCode::Esc => {
                app.wb.pi_panel = None;
                app.wb.control_focus = None;
            }
            KeyCode::Up => panel.cursor = panel.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                panel.cursor = (panel.cursor + 1).min(filtered(panel).len().saturating_sub(1))
            }
            KeyCode::PageUp => panel.cursor = panel.cursor.saturating_sub(8),
            KeyCode::PageDown => {
                panel.cursor = (panel.cursor + 8).min(filtered(panel).len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let index = filtered(panel).get(panel.cursor).copied();
                return Some(index.map(|i| choose(app, i)).unwrap_or(AppAction::None));
            }
            KeyCode::Backspace => {
                panel.query.pop();
                panel.cursor = 0;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                panel.query.push(c);
                panel.cursor = 0;
            }
            _ => {}
        }
        return Some(AppAction::None);
    }
    let choices = suggestions(app);
    if choices.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Esc => {
            app.wb.slash_dismissed = Some(app.input.clone());
            Some(AppAction::None)
        }
        KeyCode::Up => {
            app.wb.slash_cursor = app.wb.slash_cursor.saturating_sub(1);
            Some(AppAction::None)
        }
        KeyCode::Down => {
            app.wb.slash_cursor = (app.wb.slash_cursor + 1).min(choices.len() - 1);
            Some(AppAction::None)
        }
        KeyCode::Tab => {
            app.input = format!(
                "{} ",
                choices[app.wb.slash_cursor.min(choices.len() - 1)].name
            );
            app.wb.cursor = app.input.len();
            app.wb.slash_cursor = 0;
            Some(AppAction::None)
        }
        KeyCode::Enter
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            Some(choose(app, app.wb.slash_cursor.min(choices.len() - 1)))
        }
        _ => {
            app.wb.slash_cursor = 0;
            None
        }
    }
}

pub fn apply_catalog(app: &mut App, sid: SessionId, result: Result<Value, String>) {
    app.wb.pi_loading.remove(&sid);
    app.wb.pi_loaded.insert(sid);
    match result {
        Ok(value) => { let list = value.get("commands").and_then(Value::as_array).into_iter().flatten().filter_map(|v| {
            Some(SlashCommand { name: format!("/{}", v.get("name")?.as_str()?.trim_start_matches('/')), description: format!("Pi {} · {}", v.get("source").and_then(Value::as_str).unwrap_or("command"), v.get("description").and_then(Value::as_str).unwrap_or("")) })
        }).collect(); app.wb.pi_commands.insert(sid,list); }
        Err(e) => app.set_status(format!("Pi command list unavailable: {e}. If session/pi is unknown, update and restart the daemon.")),
    }
}
pub fn apply_reply(
    app: &mut App,
    sid: SessionId,
    serial: u64,
    command: &Value,
    result: Result<Value, String>,
) {
    app.wb.pi_busy.remove(&sid);
    let Some(panel) = app
        .wb
        .pi_panel
        .as_mut()
        .filter(|p| p.session_id == sid && p.serial == serial)
    else {
        return;
    };
    panel.loading = false;
    let data = match result {
        Ok(data) => data,
        Err(e) => {
            let message = e.clone();
            panel.title = "Pi · command failed".into();
            panel.choices = if e.contains("method not found") {
                vec![
                    info("The daemon needs an update.".into()),
                    info("Restart it to enable Pi commands.".into()),
                ]
            } else {
                vec![info(e)]
            };
            app.set_error_for(Some(sid), format!("Pi command failed: {message}"));
            return;
        }
    };
    match command.get("type").and_then(Value::as_str).unwrap_or("") {
        "get_available_models" => {
            panel.title = "Pi · choose model".into();
            panel.choices = data
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?;
                    let provider = m.get("provider")?.as_str()?;
                    Some(Choice {
                        label: format!("{provider}/{id}"),
                        detail: m.get("name").and_then(Value::as_str).unwrap_or("").into(),
                        command: Some(json!({"type":"set_model","provider":provider,"modelId":id})),
                    })
                })
                .collect();
        }
        "get_available_thinking_levels" => {
            panel.title = "Pi · thinking level".into();
            panel.choices = data
                .get("levels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|level| Choice {
                    label: level.into(),
                    detail: String::new(),
                    command: Some(json!({"type":"set_thinking_level","level":level})),
                })
                .collect();
        }
        "get_state" => {
            panel.title = "Pi · settings".into();
            let model = data
                .pointer("/model/id")
                .and_then(Value::as_str)
                .unwrap_or("none");
            let level = data
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .unwrap_or("off");
            panel.choices = vec![
                Choice {
                    label: format!("Model: {model}"),
                    detail: "Choose model".into(),
                    command: Some(json!({"type":"get_available_models"})),
                },
                Choice {
                    label: format!("Thinking: {level}"),
                    detail: "Choose reasoning level".into(),
                    command: Some(json!({"type":"get_available_thinking_levels"})),
                },
            ];
            if let Some(enabled) = data.get("autoCompactionEnabled").and_then(Value::as_bool) {
                panel.choices.push(Choice {
                    label: format!("Auto compaction: {enabled}"),
                    detail: "Click to toggle".into(),
                    command: Some(json!({"type":"set_auto_compaction","enabled":!enabled})),
                });
            }
            panel.choices.push(info(
                "Pi terminal theme/keybindings are local to its native TUI.".into(),
            ));
        }
        "get_session_stats" => {
            panel.title = "Pi · session statistics".into();
            panel.choices = serde_json::to_string_pretty(&data)
                .unwrap_or_default()
                .lines()
                .map(|l| info(l.into()))
                .collect();
        }
        "compact" => {
            panel.title = "Pi · compacted".into();
            panel.choices = data
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("Compaction completed.")
                .lines()
                .map(|l| info(l.into()))
                .collect();
        }
        _ => {
            panel.title = "Pi · updated".into();
            let message = match command.get("type").and_then(Value::as_str) {
                Some("set_model") => format!(
                    "Model: {}/{}",
                    command["provider"].as_str().unwrap_or(""),
                    command["modelId"].as_str().unwrap_or("")
                ),
                Some("set_thinking_level") => format!(
                    "Thinking level: {}",
                    command["level"].as_str().unwrap_or("")
                ),
                Some("set_session_name") => format!(
                    "Pi session name: {}",
                    command["name"].as_str().unwrap_or("")
                ),
                Some("set_auto_compaction") => format!(
                    "Auto compaction: {}",
                    if command["enabled"].as_bool().unwrap_or(false) {
                        "on"
                    } else {
                        "off"
                    }
                ),
                _ => "Pi settings updated.".into(),
            };
            panel.choices = vec![info(message)];
        }
    }
    if panel.choices.is_empty() {
        panel
            .choices
            .push(info("No choices returned by Pi.".into()));
    }
}
fn info(label: String) -> Choice {
    Choice {
        label,
        detail: String::new(),
        command: None,
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    if app.mode != InputMode::Editing || app.wb.menu {
        return;
    }
    let all = suggestions(app);
    let (area, title, items, indices, cursor) = if let Some(panel) = &app.wb.pi_panel {
        let area = crate::ui::centered(frame.area(), 82, 20);
        app.wb.hits.borrow_mut().clear();
        let indices = filtered(panel);
        let items = indices
            .iter()
            .map(|i| {
                let c = &panel.choices[*i];
                ListItem::new(Line::from(vec![
                    Span::styled(c.label.clone(), THEME.text),
                    Span::styled(format!("  {}", c.detail), THEME.dim),
                ]))
            })
            .collect::<Vec<_>>();
        (
            area,
            format!(
                " {} · {}{} ",
                panel.title,
                if panel.loading {
                    "loading…"
                } else {
                    "filter: "
                },
                panel.query
            ),
            items,
            indices,
            panel.cursor,
        )
    } else if !all.is_empty() {
        let editor_y = app
            .wb
            .hits
            .borrow()
            .iter()
            .find_map(|h| match h.target {
                Target::Editor { area, .. } => Some(area.y.saturating_sub(1)),
                _ => None,
            })
            .unwrap_or(frame.area().bottom().saturating_sub(6));
        let height = (all.len() as u16 + 3).min(11).min(editor_y);
        let width = frame.area().width.saturating_sub(4).min(86);
        let area = Rect::new(
            frame.area().x + 2,
            editor_y.saturating_sub(height),
            width,
            height,
        );
        let items = all
            .iter()
            .map(|c| {
                ListItem::new(Line::from(vec![
                    Span::styled(c.name.clone(), THEME.accent),
                    Span::styled(format!("  {}", c.description), THEME.dim),
                ]))
            })
            .collect();
        (
            area,
            " Commands · ↑↓ choose · Tab complete · Enter run ".into(),
            items,
            (0..all.len()).collect(),
            app.wb.slash_cursor.min(all.len() - 1),
        )
    } else {
        return;
    };
    hit(app, area, Target::Blocked);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(THEME.border)
        .style(THEME.panel);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let list_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    let mut state = ListState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(THEME.selection)
            .highlight_symbol("› "),
        list_area,
        &mut state,
    );
    for (row, index) in indices
        .iter()
        .skip(state.offset())
        .take(list_area.height as usize)
        .enumerate()
    {
        hit(
            app,
            Rect::new(list_area.x, list_area.y + row as u16, list_area.width, 1),
            Target::AgentChoice(*index),
        );
    }
    buttons(
        frame,
        app,
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        &[("Close", Target::Close)],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmux_core::*;
    fn pi_app() -> App {
        let id = AgentId::new("pi");
        let agent = AgentProfile {
            id: id.clone(),
            name: "Pi".into(),
            adapter: AdapterKind::PiRpc {
                command: "pi".into(),
                args: vec![],
            },
            env: Default::default(),
            available: true,
        };
        let session = Session {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            agent_id: id,
            state: SessionState::Ready,
            acp_session_id: None,
            native_session_file: None,
            native_terminal: false,
            references: vec![],
            created_at: chrono::Utc::now(),
        };
        App::new(
            vec![],
            vec![],
            vec![crate::app::SessionView {
                session,
                agent_name: "Pi".into(),
                workspace_name: "main".into(),
            }],
            vec![agent],
        )
    }
    #[test]
    fn slash_opens_choices_and_tab_completes_without_sending() {
        let mut app = pi_app();
        app.insert_text("/mo");
        assert_eq!(suggestions(&app)[0].name, "/model");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            AppAction::None
        );
        assert_eq!(app.input, "/model ");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            AppAction::Pi(json!({"type":"get_available_models"}))
        );
    }
    #[test]
    fn pi_extensions_pass_through_and_local_commands_have_namespace() {
        let mut app = pi_app();
        app.insert_text("/fix-tests 中文");
        assert!(matches!(submit(&mut app),AppAction::Submit{text,..} if text=="/fix-tests 中文"));
        app.insert_text("/thinking high");
        assert_eq!(
            submit(&mut app),
            AppAction::Pi(json!({"type":"set_thinking_level","level":"high"}))
        );
        app.insert_text("/mux thinking");
        assert_eq!(submit(&mut app), AppAction::None);
        assert!(!app.wb.thoughts);
        app.insert_text("/new");
        assert!(matches!(submit(&mut app), AppAction::CreateSession { .. }));
    }
    #[test]
    fn command_lists_belong_to_sessions_and_unsupported_builtin_keeps_input() {
        let mut app = pi_app();
        let sid = app.selected_session_id().unwrap();
        apply_catalog(
            &mut app,
            sid,
            Ok(
                json!({"commands":[{"name":"skill:review","description":"Review", "source":"skill"}]}),
            ),
        );
        app.insert_text("/skill");
        assert_eq!(suggestions(&app)[0].name, "/skill:review");
        app.input = "/login".into();
        assert_eq!(submit(&mut app), AppAction::None);
        assert_eq!(app.input, "/login");
        assert!(app.status.as_deref().unwrap().contains("native interface"));
        // Pi extensions can override a built-in command name.
        apply_catalog(
            &mut app,
            sid,
            Ok(
                json!({"commands":[{"name":"model","source":"extension","description":"Custom model command"}]}),
            ),
        );
        app.input = "/model".into();
        assert!(suggestions(&app)[0].description.contains("extension"));
        assert!(matches!(submit(&mut app), AppAction::Submit {text,..} if text == "/model"));
    }
    #[test]
    fn acp_agents_own_unqualified_commands_even_when_workbench_names_collide() {
        let mut app = pi_app();
        app.agents[0].adapter = AdapterKind::Acp {
            command: "agent".into(),
            args: vec![],
        };
        for command in [
            "/new",
            "/resume",
            "/thinking",
            "/quit",
            "/help",
            "/files",
            "/model",
            "/mux-custom",
        ] {
            app.input = command.into();
            assert!(
                matches!(submit(&mut app), AppAction::Submit { text, .. } if text == command),
                "{command}"
            );
        }
        app.input = "/mux help".into();
        assert_eq!(submit(&mut app), AppAction::None);
        assert_eq!(app.mode, InputMode::Help);
        app.mode = InputMode::Editing;
        app.input = "/mux resume".into();
        assert_eq!(submit(&mut app), AppAction::ResumeSession);
        app.input = "/mux quit".into();
        assert_eq!(submit(&mut app), AppAction::Quit);
    }
    #[test]
    fn selected_non_pi_completion_only_advertises_namespaced_workbench_commands() {
        let mut app = pi_app();
        app.agents[0].adapter = AdapterKind::Acp {
            command: "agent".into(),
            args: vec![],
        };
        app.input = "/".into();
        assert_eq!(
            suggestions(&app)
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["/mux"]
        );
        app.input = "/mux unknown".into();
        assert_eq!(submit(&mut app), AppAction::None);
        assert_eq!(app.input, "/mux unknown");
    }

    fn acp_app() -> App {
        let mut app = pi_app();
        app.agents[0].adapter = AdapterKind::Acp {
            command: "opencode".into(),
            args: vec!["acp".into()],
        };
        app
    }

    fn catalog(id: SessionId, seq: u64, commands: Value) -> Event {
        Event {
            session_id: id,
            seq,
            ts: chrono::Utc::now(),
            kind: EventKind::SessionUpdate(json!({
                "sessionId": "native-id", "update": {
                    "sessionUpdate": "available_commands_update", "availableCommands": commands,
                }
            })),
        }
    }

    #[test]
    fn acp_catalog_drives_completion_and_preserves_command_arguments() {
        let mut app = acp_app();
        let sid = app.selected_session_id().unwrap();
        app.handle_event(catalog(sid, 10, json!([
            {"name":"init", "description":"Create AGENTS.md"},
            {"name":"review", "description":"Review changes", "input":{"hint":"[commit|branch|pr]"}},
        ])));
        app.input = "/revi".into();
        let choices = suggestions(&app);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].name, "/review");
        assert!(choices[0].description.contains("[commit|branch|pr]"));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            AppAction::None
        );
        assert_eq!(app.input, "/review ");
        app.insert_text("branch main");
        assert!(
            matches!(submit(&mut app), AppAction::Submit {text, ..} if text == "/review branch main")
        );
    }

    #[test]
    fn acp_catalog_is_session_scoped_replaced_and_not_rolled_back_by_history() {
        let mut app = acp_app();
        let sid = app.selected_session_id().unwrap();
        let mut other = app.sessions[0].clone();
        other.session.id = SessionId::new();
        let other_id = other.session.id;
        app.add_session(other);
        app.handle_event(catalog(
            sid,
            20,
            json!([{"name":"review", "description":"Current"}]),
        ));
        app.handle_event(catalog(
            other_id,
            30,
            json!([{"name":"other", "description":"Other"}]),
        ));
        app.merge_history(
            sid,
            vec![catalog(sid, 10, json!([{"name":"obsolete"}]))],
            true,
            None,
            0,
        );
        app.input = "/review".into();
        assert_eq!(suggestions(&app)[0].description, "Current");
        app.select_session(1);
        app.input = "/other".into();
        assert_eq!(suggestions(&app)[0].name, "/other");
        app.input = "/review".into();
        assert!(suggestions(&app).is_empty());
        app.select_session(0);
        app.handle_event(catalog(sid, 40, json!([])));
        app.input = "/review".into();
        assert!(suggestions(&app).is_empty());
    }

    #[test]
    fn catalog_sanitizes_names_and_keeps_mux_reserved() {
        let mut app = acp_app();
        let sid = app.selected_session_id().unwrap();
        app.handle_event(catalog(
            sid,
            1,
            json!([
                {"name":"/review", "description":"Old"},
                {"name":"review", "description":"New"},
                {"name":"mux", "description":"Must not override workbench"},
                {"name":"bad name"}, {"name":""}, {},
            ]),
        ));
        let (_, commands) = &app.wb.acp_commands[&sid];
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "/review");
        assert_eq!(commands[0].description, "New");
        app.input = "/mux".into();
        assert_eq!(suggestions(&app).len(), 1);
        assert!(suggestions(&app)[0].description.contains("Workbench"));
    }

    #[test]
    fn history_restores_catalog_and_new_conversation_discards_old_commands() {
        let mut app = acp_app();
        let sid = app.selected_session_id().unwrap();
        let event = catalog(sid, 1, json!([{"name":"review", "description":"Restored"}]));
        let page = agentmux_core::rpc::SessionHistoryResult {
            conversation_start: 0,
            events: vec![],
            has_more: true,
            title: None,
            title_seq: 0,
            pending_permissions: vec![],
            event_refs: vec![],
            pending_permission_refs: vec![],
            available_commands: Some(event.clone()),
            available_commands_ref: None,
        };
        app.merge_history_page(sid, page);
        app.input = "/review".into();
        assert_eq!(suggestions(&app)[0].name, "/review");
        app.start_conversation(sid, 50);
        assert!(suggestions(&app).is_empty());
        app.merge_history(sid, vec![event], false, None, 0);
        assert!(suggestions(&app).is_empty());
    }

    #[test]
    fn opencode_native_commands_keep_drafts_and_do_not_become_model_prompts() {
        let mut app = acp_app();
        for command in [
            "/models",
            "/sessions",
            "/resume",
            "/undo",
            "/export",
            "/connect",
            "/help",
        ] {
            app.input = command.into();
            assert_eq!(suggestions(&app)[0].name, command);
            assert_eq!(submit(&mut app), AppAction::None);
            assert_eq!(app.input, command);
            assert!(app.status.as_deref().unwrap().contains("OpenCode (native)"));
        }
        app.input = "/compact".into();
        assert!(matches!(submit(&mut app), AppAction::Submit {text, ..} if text == "/compact"));
        app.input = "/compact ignored".into();
        assert_eq!(submit(&mut app), AppAction::None);
        assert_eq!(app.input, "/compact ignored");
        app.input = "/new".into();
        assert!(matches!(submit(&mut app), AppAction::CreateSession { .. }));
        app.input = "/quit".into();
        assert_eq!(submit(&mut app), AppAction::KillSession);
        app.input = "/mux quit".into();
        assert_eq!(submit(&mut app), AppAction::Quit);
        let sid = app.selected_session_id().unwrap();
        app.handle_event(catalog(
            sid,
            10,
            json!([{"name":"models", "description":"Custom command"}]),
        ));
        app.input = "/models custom".into();
        assert!(
            matches!(submit(&mut app), AppAction::Submit {text, ..} if text == "/models custom")
        );
    }
    #[test]
    fn pi_quit_stops_the_agent_instead_of_exiting_the_workbench() {
        let mut app = pi_app();
        app.input = "/quit".into();
        assert_eq!(submit(&mut app), AppAction::KillSession);
        app.input = "/mux quit".into();
        assert_eq!(submit(&mut app), AppAction::Quit);
    }
    #[test]
    fn model_selection_uses_provider_and_model_id() {
        let mut app = pi_app();
        let sid = app.selected_session_id().unwrap();
        app.wb.pi_panel = Some(PiPanel {
            native_catalog: false,
            native_mode: false,
            native_available: false,
            structured_available: false,
            session_id: sid,
            serial: 1,
            title: "Pi".into(),
            choices: vec![],
            query: String::new(),
            cursor: 0,
            loading: true,
        });
        apply_reply(
            &mut app,
            sid,
            1,
            &json!({"type":"get_available_models"}),
            Ok(json!({"models":[{"id":"deep/model","provider":"example","name":"Model"}]})),
        );
        assert_eq!(
            choose(&mut app, 0),
            AppAction::Pi(json!({"type":"set_model","provider":"example","modelId":"deep/model"}))
        );
        app.wb.pi_panel = None;
        apply_reply(
            &mut app,
            sid,
            1,
            &json!({"type":"get_available_models"}),
            Ok(json!({"models":[]})),
        );
        assert!(
            app.wb.pi_panel.is_none(),
            "late response must not reopen a dismissed panel"
        );
    }
    #[test]
    fn completion_and_model_picker_have_mouse_targets_at_narrow_widths() {
        use ratatui::{backend::TestBackend, Terminal};
        for width in [40, 80, 120] {
            let mut app = pi_app();
            app.wb.columns = width;
            app.insert_text("/");
            let mut term = Terminal::new(TestBackend::new(width, 24)).unwrap();
            term.draw(|f| crate::shell::draw(f, &app)).unwrap();
            assert!(app
                .wb
                .hits
                .borrow()
                .iter()
                .any(|h| matches!(h.target, Target::AgentChoice(0))));
            assert_eq!(
                crate::interaction::activate(&mut app, Target::AgentChoice(0)),
                AppAction::Pi(json!({"type":"get_available_models"}))
            );
        }
    }
}
