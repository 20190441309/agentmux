//! Loaded-history navigation and exact raw-text inspection/copying.
use crate::{
    app::{App, AppAction},
    interaction::{buttons, hit, Target},
    shell::fit_text,
    theme::THEME,
};
use agentmux_core::{EventKind, SessionId};
use crossterm::event::{KeyCode, KeyEvent};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct Message {
    pub session: SessionId,
    pub seq: u64,
    pub last: u64,
    pub role: &'static str,
    pub text: String,
}

pub enum Panel {
    List {
        session: SessionId,
        cursor: usize,
    },
    Read {
        message: Message,
        scroll: usize,
        hit: usize,
        code: usize,
    },
}

#[derive(Default)]
pub struct Transcript {
    pub panel: Option<Panel>,
    pub queries: HashMap<SessionId, String>,
    pub tool_toggles: HashSet<(SessionId, String)>,
    pub copy_file: Option<tempfile::TempPath>,
    pub tools: HashMap<(SessionId, String), Tool>,
    pub read_width: std::cell::Cell<u16>,
}

#[derive(Clone)]
pub struct Tool {
    pub first: u64,
    pub last: u64,
    pub value: serde_json::Value,
}

pub fn observe(app: &mut App, event: &agentmux_core::Event) {
    let EventKind::SessionUpdate(value) = &event.kind else {
        return;
    };
    let update = value.get("update").unwrap_or(value);
    if !matches!(
        update
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str),
        Some("tool_call" | "tool_call_update")
    ) {
        return;
    }
    let Some(id) = update.get("toolCallId").and_then(serde_json::Value::as_str) else {
        return;
    };
    let tool = app
        .wb
        .transcript
        .tools
        .entry((event.session_id, id.into()))
        .or_insert(Tool {
            first: event.seq,
            last: 0,
            value: serde_json::json!({}),
        });
    if event.seq <= tool.last {
        return;
    }
    if let (Some(target), Some(source)) = (tool.value.as_object_mut(), update.as_object()) {
        for (key, value) in source {
            if !value.is_null() {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    tool.last = event.seq;
}

pub fn tool(app: &App, session: SessionId, id: &str) -> Option<Tool> {
    let tool = app.wb.transcript.tools.get(&(session, id.into()))?.clone();
    let anchor = app.wb.sessions.get(&session).and_then(|ui| ui.anchor);
    if anchor.is_none_or(|anchor| tool.last <= anchor) {
        return Some(tool);
    }
    let mut result = Tool {
        first: tool.first,
        last: 0,
        value: serde_json::json!({}),
    };
    let mut events: Vec<_> = app
        .events
        .iter()
        .filter(|event| event.session_id == session && event.seq <= anchor.unwrap())
        .collect();
    events.sort_by_key(|event| event.seq);
    for event in events {
        if let EventKind::SessionUpdate(value) = &event.kind {
            let update = value.get("update").unwrap_or(value);
            if update.get("toolCallId").and_then(serde_json::Value::as_str) == Some(id) {
                if let (Some(target), Some(source)) =
                    (result.value.as_object_mut(), update.as_object())
                {
                    target.extend(
                        source
                            .iter()
                            .filter(|(_, value)| !value.is_null())
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
                result.last = event.seq;
            }
        }
    }
    Some(result)
}

pub fn messages(app: &App, session: SessionId) -> Vec<Message> {
    let boundary = app
        .wb
        .sessions
        .get(&session)
        .map(|ui| ui.conversation_start)
        .unwrap_or(0);
    let mut events: Vec<_> = app
        .events
        .iter()
        .filter(|event| event.session_id == session && event.seq > boundary)
        .collect();
    events.sort_by_key(|event| event.seq);
    let mut messages = vec![];
    let mut user: Option<Message> = None;
    for event in events {
        let text = crate::workbench::prompt_text(event);
        if let Some(text) = text {
            let message = user.get_or_insert(Message {
                session,
                seq: event.seq,
                last: event.seq,
                role: "You",
                text: String::new(),
            });
            message.text.push_str(text);
            message.last = event.seq;
        } else if let Some(message) = user.take() {
            messages.push(message);
        }
    }
    if let Some(message) = user {
        messages.push(message);
    }
    if let Some(reasoning) = app.wb.reasoning.get(&session) {
        for (seq, reply) in &reasoning.replies {
            let last = reasoning
                .reply_events
                .iter()
                .filter_map(|(event, (id, _))| (id == seq).then_some(*event))
                .max()
                .unwrap_or(*seq);
            messages.push(Message {
                session,
                seq: *seq,
                last,
                role: "Assistant",
                text: reply.text.clone(),
            });
        }
        for (seq, thought) in &reasoning.blocks {
            let last = reasoning
                .events
                .iter()
                .filter_map(|(event, id)| (id == seq).then_some(*event))
                .max()
                .unwrap_or(*seq);
            messages.push(Message {
                session,
                seq: *seq,
                last,
                role: "Thought",
                text: thought.text.clone(),
            });
        }
    }
    messages.sort_by_key(|message| message.seq);
    messages
}

pub fn code_blocks(text: &str) -> Vec<String> {
    let mut blocks = vec![];
    let mut code = None;
    for event in Parser::new_ext(text, pulldown_cmark::Options::all()) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code = Some(String::new()),
            Event::Text(text) if code.is_some() => code.as_mut().unwrap().push_str(&text),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(code) = code.take() {
                    blocks.push(code);
                }
            }
            _ => {}
        }
    }
    blocks
}

pub fn query(app: &App, session: SessionId) -> &str {
    app.wb
        .transcript
        .queries
        .get(&session)
        .map(String::as_str)
        .unwrap_or("")
}

fn filtered(app: &App, session: SessionId) -> Vec<Message> {
    let query = query(app, session);
    messages(app, session)
        .into_iter()
        .filter(|message| query.is_empty() || message.text.contains(query))
        .collect()
}

pub fn open(app: &mut App) {
    if let Some(session) = app.selected_session_id() {
        app.wb.menu = false;
        app.wb.control_focus = None;
        app.wb.transcript.panel = Some(Panel::List { session, cursor: 0 });
    }
}

pub fn close(app: &mut App) {
    app.wb.transcript.panel = None;
    app.wb.control_focus = None;
}

pub fn choose(app: &mut App, session: SessionId, seq: u64) {
    if let Some(message) = messages(app, session)
        .into_iter()
        .find(|message| message.seq == seq)
    {
        app.wb.control_focus = None;
        app.wb.transcript.panel = Some(Panel::Read {
            message,
            scroll: 0,
            hit: 0,
            code: 0,
        });
    }
}

pub fn jump(app: &mut App, message: &Message) {
    if app.selected_session_id() != Some(message.session) {
        return;
    }
    close(app);
    app.wb.inspection = None;
    let ui = app.wb.sessions.entry(message.session).or_default();
    ui.anchor = Some(message.last);
    ui.scroll = 0;
    ui.scroll_limit.set(None);
    crate::focus::select(app, crate::focus::Pane::Reading);
}

pub fn user_message(app: &mut App, next: bool) {
    let Some(session) = app.selected_session_id() else {
        return;
    };
    let point = app
        .wb
        .sessions
        .get(&session)
        .and_then(|ui| ui.anchor)
        .unwrap_or(u64::MAX);
    let users: Vec<_> = messages(app, session)
        .into_iter()
        .filter(|message| message.role == "You")
        .collect();
    let message = if next {
        users.into_iter().find(|message| message.last > point)
    } else {
        users.into_iter().rev().find(|message| message.last < point)
    };
    if let Some(message) = message {
        jump(app, &message);
    }
}

pub fn highlight(line: Line<'static>, query: &str) -> Line<'static> {
    if query.is_empty() {
        return line;
    }
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let hits: Vec<_> = text
        .match_indices(query)
        .map(|(start, _)| start..start + query.len())
        .collect();
    if hits.is_empty() {
        return line;
    }
    let mut spans = vec![];
    let mut offset = 0;
    for span in line.spans {
        let mut cuts = vec![0, span.content.len()];
        for range in &hits {
            if range.start < offset + span.content.len() && range.end > offset {
                cuts.push(range.start.saturating_sub(offset));
                cuts.push(range.end.saturating_sub(offset).min(span.content.len()));
            }
        }
        cuts.sort_unstable();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let active = hits
                .iter()
                .any(|range| range.start < offset + pair[1] && range.end > offset + pair[0]);
            spans.push(Span::styled(
                span.content[pair[0]..pair[1]].to_owned(),
                if active {
                    span.style
                        .patch(THEME.selection)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    span.style
                },
            ));
        }
        offset += span.content.len();
    }
    Line { spans, ..line }
}

pub fn action(app: &mut App, action: &'static str) -> AppAction {
    match action {
        "history" => return AppAction::LoadHistory,
        "clear" => {
            if let Some(session) = app.selected_session_id() {
                app.wb.transcript.queries.remove(&session);
            }
        }
        "list" => {
            open(app);
        }
        "prev" | "next" => {
            let Some(Panel::Read { message, .. }) = &app.wb.transcript.panel else {
                return AppAction::None;
            };
            let (session, seq) = (message.session, message.seq);
            let messages = filtered(app, session);
            let current = messages
                .iter()
                .position(|message| message.seq == seq)
                .unwrap_or(0);
            let next = if action == "prev" {
                current.saturating_sub(1)
            } else {
                (current + 1).min(messages.len().saturating_sub(1))
            };
            if let Some(message) = messages.get(next) {
                choose(app, session, message.seq);
            }
        }
        "hit-prev" | "hit-next" => {
            let Some(Panel::Read { message, hit, .. }) = &app.wb.transcript.panel else {
                return AppAction::None;
            };
            let text = message.text.clone();
            let query = query(app, message.session).to_owned();
            let hits: Vec<_> = if query.is_empty() {
                vec![]
            } else {
                text.match_indices(&query)
                    .map(|(offset, _)| offset)
                    .collect()
            };
            if hits.is_empty() {
                return AppAction::None;
            }
            let next = if action == "hit-prev" {
                hit.saturating_sub(1)
            } else {
                (hit + 1).min(hits.len() - 1)
            };
            if let Some(Panel::Read { scroll, hit, .. }) = &mut app.wb.transcript.panel {
                *hit = next;
                *scroll = Paragraph::new(text[..hits[next]].to_owned())
                    .wrap(Wrap { trim: false })
                    .line_count(app.wb.transcript.read_width.get().max(1))
                    .saturating_sub(1);
            }
        }
        "copy" | "copy-code" => {
            if let Some(Panel::Read { message, code, .. }) = &app.wb.transcript.panel {
                let text = if action == "copy" {
                    Some(message.text.clone())
                } else {
                    code_blocks(&message.text).get(*code).cloned()
                };
                if let Some(text) = text {
                    return AppAction::CopyText {
                        session: message.session,
                        text,
                    };
                }
            }
        }
        "code-next" => {
            if let Some(Panel::Read { message, code, .. }) = &mut app.wb.transcript.panel {
                let count = code_blocks(&message.text).len();
                if count > 0 {
                    *code = (*code + 1) % count;
                }
            }
        }
        "jump" => {
            if let Some(Panel::Read { message, .. }) = &app.wb.transcript.panel {
                let message = message.clone();
                jump(app, &message);
            }
        }
        _ => {}
    }
    AppAction::None
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    app.wb.transcript.panel.as_ref()?;
    match key.code {
        KeyCode::Esc => close(app),
        KeyCode::Tab | KeyCode::BackTab => return crate::interaction::keyboard(app, key),
        KeyCode::Enter if app.wb.control_focus.is_some() => {
            return crate::interaction::keyboard(app, key)
        }
        _ => {
            let session = match &app.wb.transcript.panel {
                Some(Panel::List { session, .. }) => Some(*session),
                _ => None,
            };
            if let Some(session) = session {
                match key.code {
                    KeyCode::Backspace => {
                        app.wb.transcript.queries.entry(session).or_default().pop();
                    }
                    KeyCode::Up => {
                        if let Some(Panel::List { cursor, .. }) = &mut app.wb.transcript.panel {
                            *cursor = cursor.saturating_sub(1);
                        }
                    }
                    KeyCode::Down => {
                        let count = filtered(app, session).len();
                        if let Some(Panel::List { cursor, .. }) = &mut app.wb.transcript.panel {
                            *cursor = (*cursor + 1).min(count.saturating_sub(1));
                        }
                    }
                    KeyCode::Enter => {
                        let cursor =
                            if let Some(Panel::List { cursor, .. }) = &app.wb.transcript.panel {
                                *cursor
                            } else {
                                0
                            };
                        if let Some(message) = filtered(app, session).get(cursor) {
                            choose(app, session, message.seq);
                        }
                    }
                    _ => {
                        if let Some(character) = crate::input::plain_char(&key) {
                            app.wb
                                .transcript
                                .queries
                                .entry(session)
                                .or_default()
                                .push(character);
                            if let Some(Panel::List { cursor, .. }) = &mut app.wb.transcript.panel {
                                *cursor = 0;
                            }
                        }
                    }
                }
            } else if let Some(Panel::Read { scroll, .. }) = &mut app.wb.transcript.panel {
                match key.code {
                    KeyCode::Up | KeyCode::PageUp => *scroll = scroll.saturating_sub(8),
                    KeyCode::Down | KeyCode::PageDown => *scroll = scroll.saturating_add(8),
                    KeyCode::Home => *scroll = 0,
                    KeyCode::End => *scroll = usize::MAX,
                    _ => {}
                }
            }
        }
    }
    Some(AppAction::None)
}

pub fn paste(app: &mut App, text: &str) -> bool {
    if let Some(Panel::List { session, cursor }) = &mut app.wb.transcript.panel {
        app.wb
            .transcript
            .queries
            .entry(*session)
            .or_default()
            .push_str(text.trim());
        *cursor = 0;
        return true;
    }
    app.wb.transcript.panel.is_some()
}

pub fn wheel(app: &mut App, up: bool) {
    match &mut app.wb.transcript.panel {
        Some(Panel::Read { scroll, .. }) => {
            *scroll = if up {
                scroll.saturating_sub(3)
            } else {
                scroll.saturating_add(3)
            }
        }
        Some(Panel::List { cursor, .. }) => {
            *cursor = if up {
                cursor.saturating_sub(3)
            } else {
                cursor.saturating_add(3)
            }
        }
        None => {}
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let Some(panel) = &app.wb.transcript.panel else {
        return;
    };
    app.wb.hits.borrow_mut().clear();
    let inner = crate::shell::modal(
        frame,
        96,
        frame.area().height.saturating_sub(2).max(8),
        " Conversation ",
    );
    match panel {
        Panel::List { session, cursor } => {
            let parts = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(inner);
            let ui = app.wb.sessions.get(session);
            let scope = if ui.is_some_and(|ui| ui.loading) {
                "Loading older history…"
            } else if ui.is_some_and(|ui| ui.older) {
                "Loaded history · older messages not searched"
            } else {
                "Loaded conversation history"
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(format!(" / {}", query(app, *session)), THEME.accent),
                    Line::styled(scope, THEME.dim),
                ]),
                parts[0],
            );
            let messages = filtered(app, *session);
            let mut state = ListState::default()
                .with_selected((!messages.is_empty()).then(|| (*cursor).min(messages.len() - 1)));
            let items: Vec<_> = messages
                .iter()
                .map(|message| {
                    ListItem::new(vec![
                        Line::styled(format!("{} · #{}", message.role, message.seq), THEME.accent),
                        Line::styled(
                            fit_text(
                                message.text.lines().next().unwrap_or(""),
                                usize::from(parts[1].width.saturating_sub(2)),
                            ),
                            THEME.text,
                        ),
                    ])
                })
                .collect();
            if items.is_empty() {
                frame.render_widget(
                    Paragraph::new("No matches in loaded history").style(THEME.dim),
                    parts[1],
                );
            } else {
                frame.render_stateful_widget(
                    List::new(items)
                        .highlight_style(THEME.selection)
                        .highlight_symbol("› "),
                    parts[1],
                    &mut state,
                );
            }
            for (row, message) in messages
                .iter()
                .skip(state.offset())
                .take(usize::from(parts[1].height) / 2)
                .enumerate()
            {
                hit(
                    app,
                    Rect::new(parts[1].x, parts[1].y + row as u16 * 2, parts[1].width, 2),
                    Target::Message(message.session, message.seq),
                );
            }
            buttons(
                frame,
                app,
                parts[2],
                &[
                    ("Close", Target::Close),
                    ("Older", Target::Transcript("history")),
                    ("Clear", Target::Transcript("clear")),
                ],
            );
        }
        Panel::Read {
            message,
            scroll,
            hit,
            code,
        } => {
            let parts = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(if inner.width < 70 { 5 } else { 3 }),
            ])
            .split(inner);
            let query = query(app, message.session);
            app.wb.transcript.read_width.set(parts[1].width);
            let matches = if query.is_empty() {
                0
            } else {
                message.text.match_indices(query).count()
            };
            let codes = code_blocks(&message.text);
            frame.render_widget(
                Paragraph::new(fit_text(
                    &format!(
                        "{} · #{} · match {}/{} · code {}/{}",
                        message.role,
                        message.seq,
                        if matches > 0 { hit + 1 } else { 0 },
                        matches,
                        if !codes.is_empty() { code + 1 } else { 0 },
                        codes.len()
                    ),
                    usize::from(parts[0].width),
                ))
                .style(THEME.accent),
                parts[0],
            );
            let lines: Vec<_> = message
                .text
                .lines()
                .map(|line| highlight(Line::raw(line.to_owned()), query))
                .collect();
            let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
            let max = paragraph
                .line_count(parts[1].width)
                .saturating_sub(usize::from(parts[1].height));
            frame.render_widget(
                paragraph.scroll(((*scroll).min(max).min(u16::MAX as usize) as u16, 0)),
                parts[1],
            );
            let mut choices = vec![
                ("Close", Target::Close),
                ("Messages", Target::Transcript("list")),
                ("Copy", Target::Transcript("copy")),
                ("Jump", Target::Transcript("jump")),
                ("Prev", Target::Transcript("prev")),
                ("Next", Target::Transcript("next")),
                ("Prev hit", Target::Transcript("hit-prev")),
                ("Next hit", Target::Transcript("hit-next")),
            ];
            if !codes.is_empty() {
                choices.push(("Copy code", Target::Transcript("copy-code")));
                choices.push(("Next code", Target::Transcript("code-next")));
            }
            buttons(frame, app, parts[2], &choices);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SessionView;
    use agentmux_core::{AgentId, Session, SessionState, WorkspaceId};
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};
    fn app() -> App {
        App::new(
            vec![],
            vec![],
            vec![SessionView {
                session: Session {
                    id: SessionId::new(),
                    workspace_id: WorkspaceId::new(),
                    agent_id: AgentId::new("mock"),
                    state: SessionState::Ready,
                    acp_session_id: None,
                    native_session_file: None,
                    native_terminal: false,
                    references: vec![],
                    created_at: Utc::now(),
                },
                agent_name: "Mock".into(),
                workspace_name: "space".into(),
            }],
            vec![],
        )
    }
    fn event(app: &mut App, seq: u64, value: serde_json::Value) {
        app.handle_event(agentmux_core::Event {
            session_id: app.selected_session_id().unwrap(),
            seq,
            ts: Utc::now(),
            kind: EventKind::SessionUpdate(value),
        });
    }
    #[test]
    fn streamed_messages_search_copy_and_code_keep_complete_source() {
        let mut app = app();
        let id = app.selected_session_id().unwrap();
        event(
            &mut app,
            1,
            serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"text":"first prompt"}}),
        );
        event(
            &mut app,
            2,
            serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"text":"中文查"}}),
        );
        event(
            &mut app,
            3,
            serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"text":"询\n```rust\n    literal();\n```\n"}}),
        );
        let messages = messages(&app, id);
        assert_eq!(messages.len(), 2);
        assert!(messages[1].text.starts_with("中文查询"));
        assert_eq!(code_blocks(&messages[1].text), vec!["    literal();\n"]);
        app.wb.transcript.queries.insert(id, "查询".into());
        assert_eq!(filtered(&app, id).len(), 1);
        choose(&mut app, id, messages[1].seq);
        assert!(
            matches!(action(&mut app, "copy"), AppAction::CopyText { text, .. } if text == messages[1].text)
        );
        assert!(
            matches!(action(&mut app, "copy-code"), AppAction::CopyText { text, .. } if text == "    literal();\n")
        );
    }
    #[test]
    fn empty_search_and_readonly_panel_do_not_panic_or_change_draft() {
        let mut app = app();
        app.insert_text("draft");
        open(&mut app);
        paste(&mut app, "not found");
        for (width, height) in [(20, 8), (40, 16), (80, 24), (120, 30), (160, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("No matches") || width == 20);
        }
        keyboard(
            &mut app,
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.input, "draft");
    }
    #[test]
    fn highlights_cross_span_boundaries_and_user_jumps_freeze_history() {
        let line = highlight(
            Line::from(vec![Span::raw("中文"), Span::raw("查询")]),
            "文查",
        );
        assert!(line
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|span| span.content.as_ref())
            .collect::<String>()
            .contains("文查"));
        let mut app = app();
        event(
            &mut app,
            1,
            serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"text":"one"}}),
        );
        event(
            &mut app,
            2,
            serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"text":"reply"}}),
        );
        event(
            &mut app,
            3,
            serde_json::json!({"sessionUpdate":"user_message_chunk","content":{"text":"two"}}),
        );
        app.insert_text("kept draft");
        user_message(&mut app, false);
        let id = app.selected_session_id().unwrap();
        assert_eq!(app.wb.sessions[&id].anchor, Some(3));
        user_message(&mut app, false);
        assert_eq!(app.wb.sessions[&id].anchor, Some(1));
        user_message(&mut app, true);
        assert_eq!(app.wb.sessions[&id].anchor, Some(3));
        assert_eq!(app.input, "kept draft");
    }
    #[test]
    fn tool_updates_merge_and_history_anchor_excludes_future_output() {
        let mut app = app();
        let id = app.selected_session_id().unwrap();
        event(
            &mut app,
            1,
            serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"t","title":"Run","rawInput":{"cmd":"test"},"status":"in_progress"}),
        );
        event(
            &mut app,
            2,
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"t","rawOutput":"finished","status":"completed"}),
        );
        let record = tool(&app, id, "t").unwrap();
        assert_eq!(record.value["rawInput"]["cmd"], "test");
        assert_eq!(record.value["rawOutput"], "finished");
        app.wb.sessions.entry(id).or_default().anchor = Some(1);
        assert!(tool(&app, id, "t")
            .unwrap()
            .value
            .get("rawOutput")
            .is_none());
        assert!(app.wb.transcript.tool_toggles.insert((id, "t".into())));
        assert!(!app.wb.tools_expanded);
    }
}
