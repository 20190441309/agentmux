//! Layout + drawing. Reads [`App`](crate::app::App) only — never mutates.
//!
//! ```text
//! ┌ sessions ──┬─ events ───────────────────────────┐
//! │ ▸ alpha    │  12:01:03 ●→◐ prompting            │  body
//! │  ● claude… │  12:01:04 hello world              │
//! ├────────────┴────────────────────────────────────┤
//! │ normal  q quit · j/k select · i prompt …        │  status bar
//! ├─────────────────────────────────────────────────┤
//! │ prompt  …                                       │  input box
//! └─────────────────────────────────────────────────┘
//! ```

use agentmux_core::{Event, EventKind, SessionState};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, InputMode, SessionView};

/// Width of the left session-list column — wide enough for
/// `● agent·id prompting` plus borders and the highlight symbol.
const LIST_WIDTH: u16 = 36;

/// Render the whole UI.
pub fn draw(frame: &mut Frame, app: &App) {
    let vertical = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1), // status bar
        Constraint::Length(3), // input box
    ])
    .split(frame.area());
    let body = Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Min(20)])
        .split(vertical[0]);

    draw_sessions(frame, app, body[0]);
    draw_events(frame, app, body[1]);
    draw_status(frame, app, vertical[1]);
    draw_input(frame, app, vertical[2]);
}

/// Left pane: session list grouped by workspace, state badge per row.
fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row: Option<usize> = None;
    let mut covered = vec![false; app.sessions.len()];

    for ws in &app.workspaces {
        let mut header = false;
        for (i, view) in app.sessions.iter().enumerate() {
            if view.session.workspace_id != ws.id {
                continue;
            }
            if !header {
                items.push(workspace_header(&ws.name));
                header = true;
            }
            covered[i] = true;
            if i == app.selected {
                selected_row = Some(items.len());
            }
            items.push(session_line(view));
        }
        // Workspaces with no sessions still render — `n` can land there.
        if !header {
            items.push(workspace_header(&ws.name));
        }
    }

    // Sessions whose workspace isn't in `app.workspaces` (created while
    // the list was stale, …) group under their recorded workspace names.
    let mut extra: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, view) in app.sessions.iter().enumerate() {
        if covered[i] {
            continue;
        }
        match extra
            .iter_mut()
            .find(|(name, _)| *name == view.workspace_name)
        {
            Some((_, group)) => group.push(i),
            None => extra.push((view.workspace_name.as_str(), vec![i])),
        }
    }
    for (name, group) in extra {
        items.push(workspace_header(name));
        for i in group {
            if i == app.selected {
                selected_row = Some(items.len());
            }
            items.push(session_line(&app.sessions[i]));
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  no sessions — press n",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let title = match app.mode {
        InputMode::RelayPick => "sessions — pick relay target",
        _ => "sessions",
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("›");
    let mut state = ListState::default();
    state.select(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
}

fn workspace_header(name: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("▸ {name}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )))
}

fn session_line(view: &SessionView) -> ListItem<'static> {
    let (glyph, label) = App::badge(&view.session.state);
    let id = view.session.id.to_string();
    ListItem::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph.to_string(), badge_color(&view.session.state)),
        Span::raw(format!(" {}·{} ", view.agent_name, short_id(&id))),
        Span::styled(label, Style::default().fg(Color::DarkGray)),
    ]))
}

/// First 8 chars of a session id — enough to tell sessions apart.
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn badge_color(state: &SessionState) -> Style {
    let color = match state {
        SessionState::Created => Color::DarkGray,
        SessionState::Connecting => Color::Yellow,
        SessionState::Ready => Color::Green,
        SessionState::Prompting => Color::Yellow,
        SessionState::WaitingPermission => Color::Magenta,
        SessionState::Done => Color::Blue,
        SessionState::Error(_) => Color::Red,
    };
    Style::default().fg(color)
}

/// Right pane: the selected session's event stream, bottom-anchored.
fn draw_events(frame: &mut Frame, app: &App, area: Rect) {
    let title = match app.selected_session() {
        Some(view) => {
            let (glyph, label) = App::badge(&view.session.state);
            format!(
                "{} {}·{} ({})",
                glyph,
                view.agent_name,
                short_id(&view.session.id.to_string()),
                label
            )
        }
        None => "events".to_string(),
    };
    let inner_height = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = app.events_for_selected().map(event_line).collect();
    // Bottom-anchor: hide the oldest lines, keep the newest visible.
    // `scroll` applies before wrapping, so long lines still wrap below.
    let scroll = lines.len().saturating_sub(inner_height) as u16;
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// One event → one rendered line (wrapping handles overflow).
fn event_line(ev: &Event) -> Line<'static> {
    let ts = Span::styled(
        ev.ts.format("%H:%M:%S").to_string(),
        Style::default().fg(Color::DarkGray),
    );
    let body: Vec<Span> = match &ev.kind {
        EventKind::StateChanged { from, to } => {
            let (from_g, _) = App::badge(from);
            let (to_g, to_label) = App::badge(to);
            vec![
                Span::styled(format!("{from_g}→{to_g}"), badge_color(to)),
                Span::styled(format!(" {to_label}"), Style::default().fg(Color::DarkGray)),
            ]
        }
        EventKind::SessionUpdate(v) => {
            vec![Span::raw(session_update_text(v))]
        }
        EventKind::FileEdited { path } => vec![
            Span::styled("edited ", Style::default().fg(Color::Blue)),
            Span::raw(path.display().to_string()),
        ],
        EventKind::AgentExited { code } => vec![Span::styled(
            format!("agent exited (code {code:?})"),
            Style::default().fg(Color::Red),
        )],
        EventKind::Orchestrator(msg) => vec![Span::styled(
            msg.clone(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )],
    };
    let mut spans = vec![ts, Span::raw(" ")];
    spans.extend(body);
    Line::from(spans)
}

/// Pull human-readable text out of an opaque ACP `session/update` JSON
/// blob: known shapes get their `text` payload, everything else falls
/// back to a compact, truncated dump.
fn session_update_text(value: &serde_json::Value) -> String {
    let update = value.get("update").unwrap_or(value);
    let tag = update.get("sessionUpdate").and_then(|t| t.as_str());
    if let Some(text) = update
        .pointer("/content/text")
        .or_else(|| update.get("text"))
        .and_then(|t| t.as_str())
    {
        return text.to_string();
    }
    let dump = serde_json::to_string(update).unwrap_or_else(|_| "<?>".to_string());
    let dump = if dump.chars().count() > 160 {
        let truncated: String = dump.chars().take(160).collect();
        format!("{truncated}…")
    } else {
        dump
    };
    match tag {
        Some(tag) => format!("[{tag}] {dump}"),
        None => dump,
    }
}

/// Status bar: mode indicator + transient status message or key hints.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let (mode_label, mode_color) = match app.mode {
        InputMode::Normal => ("normal", Color::Green),
        InputMode::Editing => ("editing", Color::Yellow),
        InputMode::RelayPick => ("relay", Color::Magenta),
    };
    let hints = match app.mode {
        InputMode::Normal => "q quit · j/k select · i prompt · n new · @ relay · ^c cancel",
        InputMode::Editing => "enter send · esc normal",
        InputMode::RelayPick => "j/k target · enter relay · esc abort",
    };
    let mut spans = vec![
        Span::styled(
            format!(" {mode_label} "),
            Style::default()
                .fg(Color::Black)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    match &app.status {
        Some(status) => spans.push(Span::styled(
            status.clone(),
            Style::default().fg(Color::Yellow),
        )),
        None => spans.push(Span::styled(hints, Style::default().fg(Color::DarkGray))),
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Bottom input box; shows the editing cursor in `Editing` mode.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("prompt");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Horizontal tail-scroll: show the last `inner.width` chars.
    let width = inner.width as usize;
    let shown: String = if app.input.chars().count() > width && width > 0 {
        app.input
            .chars()
            .skip(app.input.chars().count() - width)
            .collect()
    } else {
        app.input.clone()
    };
    frame.render_widget(Paragraph::new(shown.as_str()), inner);
    if app.mode == InputMode::Editing {
        frame.set_cursor_position(Position::new(
            inner.x + shown.chars().count() as u16,
            inner.y,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmux_core::{AgentId, ProjectId, Session, SessionId, Workspace, WorkspaceId};
    use chrono::Utc;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn workspace(name: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            name: name.to_string(),
            worktree_path: format!("/wt/{name}").into(),
            branch: name.to_string(),
            created_at: Utc::now(),
        }
    }

    fn session(workspace_id: WorkspaceId, state: SessionState) -> Session {
        Session {
            id: SessionId::new(),
            workspace_id,
            agent_id: AgentId::new("claude-code"),
            state,
            acp_session_id: None,
            references: vec![],
            created_at: Utc::now(),
        }
    }

    fn buffer_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Ratatui `TestBackend` buffer assertion: rendered UI contains the
    /// workspace groups, badge glyphs and the event text.
    #[test]
    fn draw_renders_groups_badges_and_events() {
        let alpha = workspace("alpha");
        let beta = workspace("beta");
        let ready = session(alpha.id, SessionState::Ready);
        let prompting = session(beta.id, SessionState::Prompting);
        let ready_id = ready.id;
        let views = vec![
            SessionView {
                session: ready,
                agent_name: "claude".into(),
                workspace_name: "alpha".into(),
            },
            SessionView {
                session: prompting,
                agent_name: "codex".into(),
                workspace_name: "beta".into(),
            },
        ];
        let mut app = App::new(vec![alpha, beta], views, vec![]);
        app.handle_event(Event {
            session_id: ready_id,
            seq: 1,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("hello world".into()),
        });

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(terminal.backend());
        assert!(text.contains("alpha"), "workspace group header: {text}");
        assert!(text.contains("beta"));
        assert!(text.contains("●"), "ready badge glyph: {text}");
        assert!(text.contains("◐"), "prompting badge glyph: {text}");
        assert!(text.contains("ready"));
        assert!(text.contains("prompting"));
        assert!(text.contains("claude"));
        assert!(text.contains("hello world"), "event text: {text}");
        assert!(text.contains("prompt"), "input box: {text}");
        assert!(text.contains("normal"), "status bar: {text}");
    }

    #[test]
    fn draw_empty_app_shows_hint() {
        let app = App::new(vec![], vec![], vec![]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("no sessions"), "{text}");
        assert!(text.contains("events"));
    }

    #[test]
    fn session_update_renders_message_text() {
        let v = serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi there"}
        });
        assert_eq!(session_update_text(&v), "hi there");
        let wrapped = serde_json::json!({
            "update": {"sessionUpdate": "tool_call", "title": "Read"}
        });
        assert!(session_update_text(&wrapped).contains("[tool_call]"));
    }
}
