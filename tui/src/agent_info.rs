//! Display only adapter-reported runtime metadata; never estimate model or cost.
use crate::{
    app::{App, AppAction},
    interaction::{buttons, Target},
    theme::THEME,
};
use agentmux_core::capabilities::{AgentCapabilities, CapabilityProtocol};
use agentmux_core::{AdapterKind, Event, EventKind, SessionId, SessionState};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout},
    text::Line,
    widgets::{Paragraph, Wrap},
    Frame,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Metadata {
    pub model: Option<String>,
    pub mode: Option<String>,
    pub thinking: Option<String>,
    pub version: Option<String>,
    pub usage: Option<Value>,
    pub cost: Option<Value>,
    pub request: Option<u64>,
    pub checked: Option<Instant>,
    pub error: Option<String>,
    pub seq: u64,
}
#[derive(Default)]
pub struct Info {
    pub sessions: HashMap<SessionId, Metadata>,
    pub serial: u64,
    pub panel: Option<(SessionId, u16)>,
    /// Latest adapter-reported capability snapshot per session.
    pub capabilities: HashMap<SessionId, AgentCapabilities>,
}

fn string(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_u64().map(|value| value.to_string()))
    })
}

/// Human-readable capability summary lines; unknown means unsupported.
pub fn capability_lines(caps: &AgentCapabilities) -> Vec<String> {
    let yes = |b: bool| if b { "yes" } else { "no" };
    let mut out = Vec::new();
    let protocol = match &caps.protocol {
        CapabilityProtocol::Acp { version } => format!(
            "ACP{}",
            version.map(|v| format!(" v{v}")).unwrap_or_default()
        ),
        CapabilityProtocol::PiRpc => "Pi RPC".into(),
        CapabilityProtocol::NativeTerminal => "Native terminal".into(),
    };
    out.push(format!(
        "Protocol: {protocol}{}",
        if caps.stale {
            " (last known; agent unreachable)"
        } else {
            ""
        }
    ));
    if matches!(caps.protocol, CapabilityProtocol::NativeTerminal) {
        out.push("Structured features: none — use the agent's own interface".into());
        return out;
    }
    out.push(if caps.load_session {
        "Resume history: supported".into()
    } else {
        "Resume history: not supported by this adapter — use native hosting to continue old conversations".into()
    });
    out.push(format!(
        "Prompt input: images {} · audio {} · embedded context {}",
        yes(caps.prompt.image),
        yes(caps.prompt.audio),
        yes(caps.prompt.embedded_context)
    ));
    if !caps.session_modes.is_empty() {
        let names: Vec<_> = caps.session_modes.iter().map(|m| m.name.as_str()).collect();
        out.push(format!("Modes: {}", names.join(", ")));
    }
    if !caps.session_models.is_empty() {
        let names: Vec<_> = caps
            .session_models
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        out.push(format!("Models: {}", names.join(", ")));
    }
    out
}

pub fn observe(app: &mut App, event: &Event) {
    if let EventKind::CapabilitiesUpdated(caps) = &event.kind {
        app.wb
            .info
            .capabilities
            .insert(event.session_id, (**caps).clone());
        return;
    }
    let EventKind::SessionUpdate(value) = &event.kind else {
        return;
    };
    let update = value.get("update").unwrap_or(value);
    let data = update.get("pi").unwrap_or(update);
    let model = string(
        data.pointer("/message/model")
            .or_else(|| data.get("modelId")),
    );
    let mode = string(update.get("currentModeId"));
    let usage = data
        .pointer("/message/usage")
        .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
        .cloned();
    if model.is_none() && mode.is_none() && usage.is_none() {
        return;
    }
    let info = app.wb.info.sessions.entry(event.session_id).or_default();
    if event.seq < info.seq {
        return;
    }
    info.seq = event.seq;
    if model.is_some() {
        info.model = model;
    }
    if mode.is_some() {
        info.mode = mode;
    }
    if let Some(usage) = usage {
        info.cost = usage.get("cost").cloned();
        info.usage = Some(usage);
    }
}

pub fn execution(app: &App, session: SessionId) -> &'static str {
    let Some(view) = app.sessions.iter().find(|view| view.session.id == session) else {
        return "Unknown";
    };
    if view.session.native_terminal {
        return "Native PTY";
    }
    match app
        .agents
        .iter()
        .find(|agent| agent.id == view.session.agent_id)
        .map(|agent| &agent.adapter)
    {
        Some(AdapterKind::PiRpc { .. }) => "Structured Pi RPC",
        Some(AdapterKind::Acp { .. }) => "Structured ACP",
        _ => "Structured · adapter unknown",
    }
}

impl App {
    pub fn info_refresh(&mut self, force: bool) -> Option<(SessionId, u64)> {
        if !self.wb.connected
            || !self.selected_is_pi()
            || self.selected_is_native()
            || !self.selected_session().is_some_and(|view| {
                matches!(
                    view.session.state,
                    SessionState::Ready | SessionState::Prompting | SessionState::WaitingPermission
                )
            })
        {
            return None;
        }
        let id = self.selected_session_id()?;
        let info = self.wb.info.sessions.entry(id).or_default();
        if info.request.is_some()
            || (!force
                && info
                    .checked
                    .is_some_and(|time| time.elapsed() < Duration::from_secs(10)))
        {
            return None;
        }
        self.wb.info.serial += 1;
        info.request = Some(self.wb.info.serial);
        Some((id, self.wb.info.serial))
    }
    pub fn info_reply(&mut self, session: SessionId, request: u64, result: Result<Value, String>) {
        let info = self.wb.info.sessions.entry(session).or_default();
        if info.request != Some(request) {
            return;
        }
        info.request = None;
        info.checked = Some(Instant::now());
        match result {
            Ok(value) => {
                info.model = string(value.pointer("/state/model/id"));
                info.mode = string(value.pointer("/state/mode"));
                info.thinking = string(value.pointer("/state/thinkingLevel"));
                info.version = string(value.pointer("/state/version"));
                info.usage = value
                    .pointer("/stats/tokens")
                    .or_else(|| value.pointer("/stats/usage"))
                    .cloned();
                info.cost = value.pointer("/stats/totalCost").cloned();
                info.error = None;
            }
            Err(error) => info.error = Some(error),
        }
    }
}

pub fn open(app: &mut App) {
    if let Some(id) = app.selected_session_id() {
        app.wb.menu = false;
        app.wb.control_focus = None;
        app.wb.info.panel = Some((id, 0));
    }
}
pub fn close(app: &mut App) {
    app.wb.info.panel = None;
    app.wb.control_focus = None;
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    let (_, scroll) = app.wb.info.panel.as_mut()?;
    match key.code {
        KeyCode::Esc => close(app),
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter => {
            return crate::interaction::keyboard(app, key).or(Some(AppAction::None))
        }
        KeyCode::PageUp | KeyCode::Up => *scroll = scroll.saturating_sub(8),
        KeyCode::PageDown | KeyCode::Down => *scroll = scroll.saturating_add(8),
        _ => {}
    }
    Some(AppAction::None)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let Some((session, scroll)) = app.wb.info.panel else {
        return;
    };
    app.wb.hits.borrow_mut().clear();
    let inner = crate::shell::modal(frame, 86, 24, " Agent details ");
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);
    let info = app.wb.info.sessions.get(&session);
    let view = app.sessions.iter().find(|view| view.session.id == session);
    let unknown = |value: Option<&str>| value.unwrap_or("Unknown").to_owned();
    let mut lines = vec![
        Line::styled(
            format!(
                "{} / {}",
                app.agent_instance(session),
                app.session_title(session)
            ),
            THEME.accent,
        ),
        Line::raw(format!("Execution: {}", execution(app, session))),
        Line::raw(format!(
            "Daemon: {}",
            if app.wb.connected {
                "Connected"
            } else {
                "Disconnected"
            }
        )),
        Line::raw(format!(
            "Agent: {}",
            view.map(|view| format!("{:?}", view.session.state))
                .unwrap_or_else(|| "Unknown".into())
        )),
        Line::raw(format!(
            "Model: {}",
            unknown(info.and_then(|info| info.model.as_deref()))
        )),
        Line::raw(format!(
            "Mode: {}",
            unknown(info.and_then(|info| info.mode.as_deref()))
        )),
        Line::raw(format!(
            "Thinking: {}",
            unknown(info.and_then(|info| info.thinking.as_deref()))
        )),
        Line::raw(format!(
            "Version: {}",
            unknown(info.and_then(|info| info.version.as_deref()))
        )),
        Line::raw(format!(
            "Usage: {}",
            info.and_then(|info| info.usage.as_ref())
                .map(Value::to_string)
                .unwrap_or_else(|| "Unknown".into())
        )),
        Line::raw(format!(
            "Cost: {}",
            info.and_then(|info| info.cost.as_ref())
                .map(Value::to_string)
                .unwrap_or_else(|| "Unknown".into())
        )),
        Line::default(),
        Line::styled("Capabilities · reported by the adapter", THEME.section),
    ];
    match app.wb.info.capabilities.get(&session) {
        Some(caps) => lines.extend(capability_lines(caps).into_iter().map(Line::raw)),
        None => lines.push(Line::raw("Not reported yet")),
    }
    lines.extend([
        Line::default(),
        Line::styled("Workbench · /mux commands", THEME.section),
        Line::raw("new · add-agent · resume · attention · search · references"),
        Line::styled("Agent · unqualified commands", THEME.section),
    ]);
    if let Some((_, commands)) = app.wb.acp_commands.get(&session) {
        lines.extend(
            commands
                .iter()
                .map(|command| Line::raw(format!("{} · {}", command.name, command.description))),
        );
    }
    if let Some(commands) = app.wb.pi_commands.get(&session) {
        lines.extend(
            commands
                .iter()
                .map(|command| Line::raw(format!("{} · {}", command.name, command.description))),
        );
    }
    lines.push(Line::styled(
        "Native-only controls · agent's native interface",
        THEME.section,
    ));
    if let Some(error) = info.and_then(|info| info.error.as_ref()) {
        lines.push(Line::styled(
            format!("Metadata unavailable: {error}"),
            THEME.error,
        ));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max = paragraph
        .line_count(parts[0].width)
        .saturating_sub(usize::from(parts[0].height))
        .min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.scroll((scroll.min(max), 0)), parts[0]);
    buttons(
        frame,
        app,
        parts[1],
        &[
            ("Close", Target::Close),
            ("Refresh", Target::Command("/refresh-info")),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_usage_is_not_estimated_and_late_metadata_does_not_overwrite() {
        let mut app = App::new(vec![], vec![], vec![], vec![]);
        let id = SessionId::new();
        app.wb.info.sessions.entry(id).or_default().request = Some(2);
        app.info_reply(
            id,
            1,
            Ok(serde_json::json!({"state":{"model":{"id":"wrong"}},"stats":{"totalCost":42}})),
        );
        assert!(app.wb.info.sessions[&id].model.is_none());
        app.info_reply(id,2,Ok(serde_json::json!({"state":{"model":{"id":"reported"},"thinkingLevel":"high"},"stats":{"messageCount":3}})));
        assert_eq!(app.wb.info.sessions[&id].model.as_deref(), Some("reported"));
        assert!(app.wb.info.sessions[&id].cost.is_none());
        assert!(app.wb.info.sessions[&id].usage.is_none());
        assert_eq!(execution(&app, id), "Unknown");
    }

    #[test]
    fn capability_event_is_stored_and_summarised_without_guessing() {
        let mut app = App::new(vec![], vec![], vec![], vec![]);
        let id = SessionId::new();
        let caps = AgentCapabilities::from_acp_initialize(&serde_json::json!({}));
        let ev = Event {
            session_id: id,
            seq: 1,
            ts: chrono::Utc::now(),
            kind: EventKind::CapabilitiesUpdated(Box::new(caps)),
        };
        observe(&mut app, &ev);
        let lines = capability_lines(&app.wb.info.capabilities[&id]);
        assert!(lines
            .iter()
            .any(|l| l.contains("Resume history: not supported")));
        assert!(lines.iter().any(|l| l.contains("images no")));
        let native = capability_lines(&AgentCapabilities::native(None));
        assert!(native.iter().any(|l| l.contains("none")));
    }
}
