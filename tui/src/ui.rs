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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{short_id, App, InputMode, RelayStage, SessionView};
use crate::newsession::WizardStep;

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
    draw_body(frame, app, body[1]);
    draw_status(frame, app, vertical[1]);
    draw_input(frame, app, vertical[2]);

    // Modal overlays draw last, on top of the panes.
    match app.mode {
        InputMode::NewSession => draw_wizard(frame, app),
        InputMode::Permission => draw_permission(frame, app),
        _ => {}
    }
}

/// The right pane: relay event picker, touched-files list, or the
/// bottom-anchored event stream.
fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    if let (InputMode::RelayPick, Some(pick)) = (app.mode, app.relay.as_ref()) {
        if pick.stage == RelayStage::Event {
            draw_relay_events(frame, app, area, pick.event_cursor);
            return;
        }
    }
    if app.show_diff {
        draw_files(frame, app, area);
        return;
    }
    draw_events(frame, app, area);
}

/// Left pane: session list grouped by workspace, state badge per row.
///
/// During the relay pick's `Session` stage the highlight follows the
/// picker's `session_cursor` instead of `app.selected` — the relayed
/// reference will go to the cursor's session, so that's what must be
/// visually hot.
fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row: Option<usize> = None;
    let mut covered = vec![false; app.sessions.len()];
    let highlight = match (app.mode, app.relay.as_ref()) {
        (InputMode::RelayPick, Some(pick)) if pick.stage == RelayStage::Session => {
            pick.session_cursor
        }
        _ => app.selected,
    };

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
            if i == highlight {
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
            if i == highlight {
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

    let title = match (app.mode, app.relay.as_ref()) {
        (InputMode::RelayPick, Some(pick)) if pick.stage == RelayStage::Session => {
            "sessions — pick relay target"
        }
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
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    // Bound per-frame work to the visible region: scan the log backwards
    // and stop once the collected lines cover the viewport in *wrapped*
    // rows — each event contributes at least one.
    let mut rows = 0usize;
    let mut lines: Vec<Line> = Vec::new();
    for ev in app.events_for_selected().rev() {
        let line = event_line(ev);
        rows += wrapped_rows(&line, inner_width);
        lines.push(line);
        if rows >= inner_height.max(1) {
            break;
        }
    }
    lines.reverse();
    // Bottom-anchor in wrapped rows: `.scroll` offsets count *post-wrap*
    // rows, so subtracting `lines.len()` (source lines) under-scrolls
    // whenever any line wraps and the newest events end up below the
    // fold. `rows` is the wrapped-row estimate instead.
    let scroll = rows.saturating_sub(inner_height) as u16;
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// Rows `line` occupies under `Wrap { trim: false }` at `width` columns —
/// a greedy word-wrap estimate: words pack into a row until the next
/// word would overflow, over-wide words split mid-word. Also floored at
/// `ceil(display-width / width)` so wide-grapheme (CJK/emoji) text — for
/// which the char-count simulation undercounts — still wraps right.
fn wrapped_rows(line: &Line, width: usize) -> usize {
    let width = width.max(1);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut rows = 1usize;
    let mut col = 0usize;
    for (i, word) in text.split(' ').enumerate() {
        let w = word.chars().count();
        let sep = usize::from(i > 0);
        if col > 0 && col + sep + w > width {
            // The word moves to a fresh row; its separator stays behind.
            rows += 1;
            col = 0;
        } else {
            col += sep;
        }
        col += w;
        while col > width {
            rows += 1;
            col -= width;
        }
    }
    rows.max(line.width().div_ceil(width))
}

/// RelayPick stage `Event`: the selected session's event log as a
/// highlightable list (newest at the bottom, cursor pre-placed there).
fn draw_relay_events(frame: &mut Frame, app: &App, area: Rect, cursor: usize) {
    let items: Vec<ListItem> = app
        .session_events()
        .map(|ev| ListItem::new(event_line(ev)))
        .collect();
    let items = if items.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no events yet",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        items
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("relay — pick event to reference · enter · esc"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("›");
    let mut state = ListState::default();
    state.select(Some(cursor));
    frame.render_stateful_widget(list, area, &mut state);
}

/// `Tab` diff/files panel: paths the selected session touched
/// (`FileEdited` events + `tool_call` locations), first-touch order.
/// v1: path list only — no real diff highlighting.
fn draw_files(frame: &mut Frame, app: &App, area: Rect) {
    let files = app.touched_files();
    let title = match app.selected_session() {
        Some(view) => format!(
            "files touched — {}·{}",
            view.agent_name,
            short_id(&view.session.id.to_string())
        ),
        None => "files touched".to_string(),
    };
    let items: Vec<ListItem> = if files.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no file activity yet",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        files
            .iter()
            .map(|f| {
                ListItem::new(Line::from(vec![
                    Span::styled("  ✎ ", Style::default().fg(Color::Blue)),
                    Span::raw(f.clone()),
                ]))
            })
            .collect()
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_bottom(" tab — back to events "),
    );
    frame.render_widget(list, area);
}

/// A centered modal rect — for the wizard and the permission notice.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let v = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .split(v[1])[1]
}

/// The `n` wizard overlay: a step-titled picker list (project →
/// workspace → agent) with the create-new-workspace name input inline.
fn draw_wizard(frame: &mut Frame, app: &App) {
    let Some(wiz) = &app.wizard else {
        return;
    };
    let plain = Style::default().fg(Color::DarkGray);
    let (title, items, cursor): (&str, Vec<ListItem>, Option<usize>) = match wiz.step {
        WizardStep::Project => (
            "pick project",
            app.projects
                .iter()
                .map(|p| {
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("  {}", p.name)),
                        Span::styled(format!("  {}", p.root_path.display()), plain),
                    ]))
                })
                .collect(),
            Some(wiz.project_cursor),
        ),
        WizardStep::Workspace => {
            let mut items: Vec<ListItem> = wiz
                .workspace_options(app)
                .map(|w| ListItem::new(Line::from(format!("  {}", w.name))))
                .collect();
            items.push(ListItem::new(Line::from(Span::styled(
                "  + create new workspace…",
                Style::default().fg(Color::Cyan),
            ))));
            ("pick workspace", items, Some(wiz.workspace_cursor))
        }
        WizardStep::WorkspaceName => (
            "name the new workspace",
            vec![
                ListItem::new(Line::from(vec![
                    Span::styled("  name: ", plain),
                    Span::raw(format!("{}▌", wiz.name)),
                ])),
                ListItem::new(Line::from(Span::styled(
                    "  (git worktree under the project)",
                    plain,
                ))),
            ],
            None,
        ),
        WizardStep::Agent => (
            "pick agent",
            wiz.agent_options(app)
                .map(|a| {
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("  {}", a.name)),
                        Span::styled(format!("  ({})", a.id), plain),
                    ]))
                })
                .collect(),
            Some(wiz.agent_cursor),
        ),
    };
    let height = (items.len() as u16 + 4).min(frame.area().height.saturating_sub(2));
    let rect = centered(frame.area(), 52, height.max(5));
    frame.render_widget(Clear, rect);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" new session — {title} "))
                .title_bottom(" enter select · esc back "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("›");
    let mut state = ListState::default();
    state.select(cursor);
    frame.render_stateful_widget(list, rect, &mut state);
}

/// The permission notice overlay. `AcpConn` answers
/// `session/request_permission` itself (deny-by-default) before the
/// event reaches the TUI and no permission-response RPC exists, so this
/// is strictly observe-only — any key dismisses it.
fn draw_permission(frame: &mut Frame, app: &App) {
    let Some(notice) = &app.permission else {
        return;
    };
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(Span::styled(
            notice.summary.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "the daemon auto-denied this request (v1 is observe-only)",
            dim,
        )),
        Line::from(Span::styled("press any key to dismiss", dim)),
    ];
    let rect = centered(frame.area(), 60, 7);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " permission requested — session {} ",
            short_id(&notice.session_id.to_string())
        ))
        .border_style(Style::default().fg(Color::Magenta));
    frame.render_widget(Paragraph::new(lines).block(block), rect);
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
        InputMode::NewSession => ("new session", Color::Cyan),
        InputMode::Permission => ("permission", Color::Magenta),
    };
    let hints = match app.mode {
        InputMode::Normal => {
            "q quit · j/k select · i prompt · n new · @ relay · x kill · r resume · tab files · ^c cancel"
        }
        InputMode::Editing => "enter send · esc normal",
        InputMode::RelayPick => match app.relay.as_ref().map(|r| r.stage) {
            Some(RelayStage::Event) => "j/k pick event · enter next · esc abort",
            _ => "j/k pick target · enter relay · esc abort",
        },
        InputMode::NewSession => "enter select · esc back",
        InputMode::Permission => "auto-denied by the daemon · any key dismisses",
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
    use crate::app::PermissionNotice;
    use agentmux_core::{
        AgentId, AgentProfile, Project, ProjectId, Session, SessionId, Workspace, WorkspaceId,
    };
    use chrono::Utc;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn project(name: &str) -> Project {
        Project {
            id: ProjectId::new(),
            root_path: format!("/repos/{name}").into(),
            name: name.to_string(),
        }
    }

    fn workspace_in(project_id: ProjectId, name: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            project_id,
            name: name.to_string(),
            worktree_path: format!("/wt/{name}").into(),
            branch: name.to_string(),
            created_at: Utc::now(),
        }
    }

    fn workspace(name: &str) -> Workspace {
        workspace_in(ProjectId::new(), name)
    }

    fn agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: AgentId::new(id),
            name: id.to_string(),
            adapter: agentmux_core::AdapterKind::Acp {
                command: format!("/bin/{id}").into(),
                args: vec![],
            },
            env: Default::default(),
            available: true,
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
        let mut app = App::new(vec![], vec![alpha, beta], views, vec![]);
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
        let app = App::new(vec![], vec![], vec![], vec![]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("no sessions"), "{text}");
        assert!(text.contains("events"));
    }

    /// A nil-session-id Orchestrator notice (the client's lag warning)
    /// renders in the event pane even though it belongs to no session.
    #[test]
    fn draw_shows_nil_id_global_notice() {
        let ws = workspace("w");
        let s = session(ws.id, SessionState::Ready);
        let app_views = vec![SessionView {
            session: s,
            agent_name: "claude".into(),
            workspace_name: "w".into(),
        }];
        let mut app = App::new(vec![], vec![ws], app_views, vec![]);
        app.handle_event(Event {
            session_id: SessionId(uuid::Uuid::nil()),
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("client event stream lagged".into()),
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(
            buffer_text(terminal.backend()).contains("client event stream lagged"),
            "global notice must render in the event pane"
        );
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

    // --- Task 14 panes/overlays ----------------------------------------------

    /// One session of workspace `w`, its events preloaded.
    fn app_with_events(kinds: Vec<EventKind>) -> App {
        let ws = workspace("w");
        let s = session(ws.id, SessionState::Ready);
        let sid = s.id;
        let views = vec![SessionView {
            session: s,
            agent_name: "claude".into(),
            workspace_name: "w".into(),
        }];
        let mut app = App::new(vec![], vec![ws], views, vec![]);
        for (seq, kind) in kinds.into_iter().enumerate() {
            app.handle_event(Event {
                session_id: sid,
                seq: seq as u64 + 1,
                ts: Utc::now(),
                kind,
            });
        }
        app
    }

    #[test]
    fn wizard_overlay_lists_projects() {
        let proj = project("smoke");
        let ws = workspace_in(proj.id, "ws1");
        let mut app = App::new(vec![proj], vec![ws], vec![], vec![agent("mock")]);
        app.start_wizard();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("new session — pick project"), "{text}");
        assert!(text.contains("smoke"), "{text}");
    }

    #[test]
    fn permission_overlay_renders_observe_only_notice() {
        let mut app = app_with_events(vec![]);
        app.permission = Some(PermissionNotice {
            session_id: SessionId(uuid::Uuid::nil()),
            summary: "agent asks: Write src/x.rs (options: allow, reject)".into(),
            resume: InputMode::Normal,
        });
        app.mode = InputMode::Permission;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("permission requested"), "{text}");
        assert!(text.contains("Write src/x.rs"), "{text}");
        assert!(text.contains("auto-denied"), "{text}");
    }

    #[test]
    fn relay_event_stage_lists_events() {
        let mut app = app_with_events(vec![
            EventKind::Orchestrator("first".into()),
            EventKind::FileEdited {
                path: "src/lib.rs".into(),
            },
        ]);
        app.start_relay();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("pick event"), "{text}");
        assert!(text.contains("edited src/lib.rs"), "{text}");
    }

    #[test]
    fn diff_panel_lists_touched_paths() {
        let mut app = app_with_events(vec![EventKind::FileEdited {
            path: "src/lib.rs".into(),
        }]);
        app.show_diff = true;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("files touched"), "{text}");
        assert!(text.contains("src/lib.rs"), "{text}");
    }

    // --- wrap-aware scroll (the reviewer-flagged under-scroll fix) -----------

    #[test]
    fn wrapped_rows_estimates_word_wrap() {
        // Fits: no wrap.
        assert_eq!(wrapped_rows(&Line::from("hello world"), 20), 1);
        // 100 chars of one word at width 20 → 5 rows.
        assert_eq!(wrapped_rows(&Line::from("a".repeat(100)), 20), 5);
        // Word-wrap waste: two 12-char words at width 20 can't share a
        // row (12+1+12 > 20) → 2 rows, where naive ceil(25/20)=2 too.
        assert_eq!(
            wrapped_rows(&Line::from("aaaaaaaaaaaa bbbbbbbbbbbb"), 20),
            2
        );
        // Four 12-char words at width 20: ceil(51/20)=3, but word
        // boundaries force one word per row → 4.
        assert_eq!(
            wrapped_rows(
                &Line::from("aaaaaaaaaaaa bbbbbbbbbbbb cccccccccccc dddddddddddd"),
                20
            ),
            4
        );
        // Empty line still occupies a row.
        assert_eq!(wrapped_rows(&Line::from(""), 20), 1);
    }

    /// Regression: with `Wrap` on, `.scroll` counts wrapped rows — so a
    /// stream of wrapping lines must still bottom-anchor on the newest
    /// event, not leave it hidden below the fold.
    #[test]
    fn events_pane_bottom_anchors_under_wrap() {
        let mut app = app_with_events(vec![
            EventKind::Orchestrator(format!("old {}", "x".repeat(300))),
            EventKind::Orchestrator("NEWEST-TAIL".into()),
        ]);
        app.mode = InputMode::Normal;
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(
            text.contains("NEWEST-TAIL"),
            "newest event must be visible: {text}"
        );
    }
}
