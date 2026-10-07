//! Pending work and persistent error feedback. History alone never creates completions.
use std::collections::HashMap;

use agentmux_core::{Event, EventKind, SessionId, SessionState};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::{
    app::{App, AppAction, InputMode},
    interaction::{buttons, hit, Target},
    shell::fit_text,
    theme::THEME,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Permission(SessionId, String),
    Failure(Option<SessionId>),
    Finished(SessionId),
    Overlap(SessionId),
}

#[derive(Clone)]
pub struct Feedback {
    pub serial: u64,
    pub session: Option<SessionId>,
    pub message: String,
}

pub enum Panel {
    Inbox { selected: Option<Key> },
    Error { feedback: Feedback, scroll: usize },
}

#[derive(Default)]
pub struct Attention {
    pub panel: Option<Panel>,
    finished: HashMap<SessionId, u64>,
    /// Latest live file-overlap notice per session: (seq, path, others).
    overlaps: HashMap<SessionId, (u64, std::path::PathBuf, Vec<SessionId>)>,
    state_seq: HashMap<SessionId, u64>,
    failure_seq: HashMap<SessionId, u64>,
    acknowledged: HashMap<SessionId, (u64, String)>,
    errors: HashMap<Option<SessionId>, Feedback>,
    reviewed_error: Option<Feedback>,
    serial: u64,
}

pub struct Item {
    pub key: Key,
    pub label: &'static str,
    pub session: Option<SessionId>,
    pub summary: String,
}

fn identity(app: &App, id: SessionId) -> String {
    if let Some(view) = app.sessions.iter().find(|view| view.session.id == id) {
        format!(
            "{} / {} / {}",
            app.agent_instance(id),
            view.workspace_name,
            app.session_title(id)
        )
    } else {
        format!("Unknown agent / {}", crate::app::short_id(&id.to_string()))
    }
}

impl Attention {
    pub fn viewed(&mut self, id: SessionId) {
        self.finished.remove(&id);
        self.overlaps.remove(&id);
    }

    pub fn reset(&mut self, id: SessionId, boundary: u64) {
        self.finished.remove(&id);
        self.overlaps.remove(&id);
        self.errors.remove(&Some(id));
        self.acknowledged.remove(&id);
        self.failure_seq.remove(&id);
        if self
            .reviewed_error
            .as_ref()
            .is_some_and(|error| error.session == Some(id))
        {
            self.reviewed_error = None;
        }
        self.state_seq
            .entry(id)
            .and_modify(|seq| *seq = (*seq).max(boundary))
            .or_insert(boundary);
    }

    pub fn latest_error(&self) -> Option<&Feedback> {
        self.errors.values().max_by_key(|error| error.serial)
    }

    pub fn state_seen(&self, id: SessionId, seq: u64) -> bool {
        seq != 0 && seq <= self.state_seq.get(&id).copied().unwrap_or(0)
    }

    pub fn baseline(&mut self, id: SessionId, seq: u64) {
        self.state_seq
            .entry(id)
            .and_modify(|latest| *latest = (*latest).max(seq))
            .or_insert(seq);
    }
}

impl App {
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.set_error_for(self.selected_session_id(), message);
    }

    pub fn set_error_for(&mut self, session: Option<SessionId>, message: impl Into<String>) {
        let message = message.into();
        self.status = Some(message.clone());
        if self
            .wb
            .attention
            .errors
            .get(&session)
            .is_some_and(|error| error.message == message)
        {
            return;
        }
        self.wb.attention.serial += 1;
        self.wb.attention.errors.insert(
            session,
            Feedback {
                serial: self.wb.attention.serial,
                session,
                message,
            },
        );
    }

    pub fn attention_items(&self) -> Vec<Item> {
        let mut items = vec![];
        for notice in self.permission.iter().chain(self.wb.permissions.iter()) {
            items.push(Item {
                key: Key::Permission(notice.session_id, notice.request_id.clone()),
                label: "Permissions",
                session: Some(notice.session_id),
                summary: notice.summary.clone(),
            });
        }
        if let Some(error) = self.wb.attention.errors.get(&None) {
            items.push(Item {
                key: Key::Failure(None),
                label: "Failed",
                session: None,
                summary: fit_text(error.message.lines().next().unwrap_or(""), 120),
            });
        }
        for view in &self.sessions {
            let id = view.session.id;
            let error = self.wb.attention.errors.get(&Some(id));
            let state_error = if let SessionState::Error(message) = &view.session.state {
                let seq = self.wb.attention.failure_seq.get(&id).copied().unwrap_or(0);
                (!self
                    .wb
                    .attention
                    .acknowledged
                    .get(&id)
                    .is_some_and(|ack| ack.0 == seq && &ack.1 == message))
                .then_some(message)
            } else {
                None
            };
            if let Some(message) = error.map(|error| &error.message).or(state_error) {
                items.push(Item {
                    key: Key::Failure(Some(id)),
                    label: "Failed",
                    session: Some(id),
                    summary: fit_text(message.lines().next().unwrap_or(""), 120),
                });
            }
        }
        let mut unknown: Vec<_> = self
            .wb
            .attention
            .errors
            .values()
            .filter(|error| {
                error.session.is_some()
                    && !self
                        .sessions
                        .iter()
                        .any(|view| Some(view.session.id) == error.session)
            })
            .collect();
        unknown.sort_by_key(|error| error.serial);
        for error in unknown {
            items.push(Item {
                key: Key::Failure(error.session),
                label: "Failed",
                session: error.session,
                summary: fit_text(error.message.lines().next().unwrap_or(""), 120),
            });
        }
        for view in &self.sessions {
            let id = view.session.id;
            if self.wb.attention.finished.contains_key(&id) {
                items.push(Item {
                    key: Key::Finished(id),
                    label: "Finished",
                    session: Some(id),
                    summary: if view.session.state == SessionState::Done {
                        "Turn ended; agent stopped"
                    } else {
                        "Turn ended"
                    }
                    .into(),
                });
            }
        }
        for view in &self.sessions {
            let id = view.session.id;
            if let Some((_, path, others)) = self.wb.attention.overlaps.get(&id) {
                let names: Vec<_> = others.iter().map(|o| self.agent_instance(*o)).collect();
                items.push(Item {
                    key: Key::Overlap(id),
                    label: "Overlaps",
                    session: Some(id),
                    summary: fit_text(
                        &format!(
                            "{} also edited recently by {}",
                            path.display(),
                            names.join(", ")
                        ),
                        120,
                    ),
                });
            }
        }
        items
    }
}

/// The user is looking at `id`'s live conversation right now.
fn reading_live(app: &App, id: SessionId) -> bool {
    app.selected_session_id() == Some(id)
        && matches!(app.mode, InputMode::Editing | InputMode::Normal)
        && app.wb.inspection.is_none()
        && !app.show_diff
        && !app.wb.menu
        && app.wb.attention.panel.is_none()
        && app.wb.naming.is_none()
        && app.wb.pi_panel.is_none()
        && !app.wb.drawer
        && app
            .wb
            .sessions
            .get(&id)
            .is_none_or(|ui| ui.anchor.is_none())
}

pub fn observe(app: &mut App, event: &Event) {
    let id = event.session_id;
    // Live events only: history baselines `state_seq`, and duplicates
    // never re-raise a notice.
    if event.seq == 0 || event.seq <= app.wb.attention.state_seq.get(&id).copied().unwrap_or(0) {
        return;
    }
    if let EventKind::FileOverlap { path, others } = &event.kind {
        if !reading_live(app, id)
            && app
                .wb
                .attention
                .overlaps
                .get(&id)
                .is_none_or(|(seq, _, _)| *seq < event.seq)
        {
            app.wb
                .attention
                .overlaps
                .insert(id, (event.seq, path.clone(), others.clone()));
        }
        return;
    }
    let EventKind::StateChanged { from, to } = &event.kind else {
        return;
    };
    app.wb.attention.state_seq.insert(id, event.seq);
    let reading_live = reading_live(app, id);
    match to {
        SessionState::Error(message) => {
            app.wb.attention.finished.remove(&id);
            app.wb.attention.failure_seq.insert(id, event.seq);
            app.set_error_for(Some(id), message.clone());
        }
        SessionState::Ready | SessionState::Done
            if matches!(
                from,
                SessionState::Prompting | SessionState::WaitingPermission
            ) =>
        {
            if reading_live {
                app.wb.attention.viewed(id);
            } else {
                app.wb.attention.finished.insert(id, event.seq);
            }
        }
        SessionState::Created | SessionState::Connecting | SessionState::Prompting => {
            app.wb.attention.finished.remove(&id);
        }
        _ => {}
    }
}

pub fn open(app: &mut App) {
    app.wb.menu = false;
    app.wb.control_focus = None;
    app.wb.attention.panel = Some(Panel::Inbox {
        selected: app.attention_items().first().map(|item| item.key.clone()),
    });
}

pub fn close(app: &mut App) {
    app.wb.attention.panel = None;
    app.wb.control_focus = None;
}

fn acknowledge(app: &mut App, feedback: &Feedback) {
    if let Some(id) = feedback.session {
        if let Some(SessionState::Error(message)) = app
            .sessions
            .iter()
            .find(|view| view.session.id == id)
            .map(|view| &view.session.state)
        {
            let seq = app.wb.attention.failure_seq.get(&id).copied().unwrap_or(0);
            app.wb
                .attention
                .acknowledged
                .insert(id, (seq, message.clone()));
        } else if !app.sessions.iter().any(|view| view.session.id == id) {
            let seq = app.wb.attention.failure_seq.get(&id).copied().unwrap_or(0);
            app.wb
                .attention
                .acknowledged
                .insert(id, (seq, feedback.message.clone()));
        }
    }
    if app
        .wb
        .attention
        .errors
        .get(&feedback.session)
        .is_some_and(|error| error.serial == feedback.serial)
    {
        app.wb.attention.errors.remove(&feedback.session);
    }
    if app.status.as_ref() == Some(&feedback.message) {
        app.status = None;
    }
}

pub fn error_details(app: &mut App) {
    if let Some(feedback) = app
        .wb
        .attention
        .latest_error()
        .or(app.wb.attention.reviewed_error.as_ref())
        .cloned()
    {
        show_error(app, feedback);
    }
}

fn show_error(app: &mut App, feedback: Feedback) {
    close(app);
    app.wb.menu = false;
    if let Some(index) = app
        .sessions
        .iter()
        .position(|view| Some(view.session.id) == feedback.session)
    {
        app.select_session(index);
        app.mode = InputMode::Editing;
        app.wb.drawer = false;
    }
    acknowledge(app, &feedback);
    app.wb.attention.reviewed_error = Some(feedback.clone());
    app.wb.attention.panel = Some(Panel::Error {
        feedback,
        scroll: 0,
    });
}

pub fn activate(app: &mut App, key: Key) -> AppAction {
    let Some(item) = app
        .attention_items()
        .into_iter()
        .find(|item| item.key == key)
    else {
        return AppAction::None;
    };
    match key {
        Key::Failure(session) => {
            let feedback = app
                .wb
                .attention
                .errors
                .get(&session)
                .cloned()
                .unwrap_or_else(|| Feedback {
                    serial: 0,
                    session,
                    message: session
                        .and_then(|id| app.sessions.iter().find(|view| view.session.id == id))
                        .and_then(|view| {
                            if let SessionState::Error(message) = &view.session.state {
                                Some(message.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(item.summary),
                });
            show_error(app, feedback);
        }
        Key::Finished(id) | Key::Overlap(id) => {
            app.wb.attention.overlaps.remove(&id);
            close(app);
            if let Some(index) = app.sessions.iter().position(|view| view.session.id == id) {
                app.select_session(index);
                app.mode = InputMode::Editing;
                app.wb.drawer = false;
                app.follow_latest();
            }
        }
        Key::Permission(id, request) => {
            close(app);
            if !app
                .permission
                .as_ref()
                .is_some_and(|notice| notice.session_id == id && notice.request_id == request)
            {
                if let Some(index) = app
                    .wb
                    .permissions
                    .iter()
                    .position(|notice| notice.session_id == id && notice.request_id == request)
                {
                    let notice = app.wb.permissions.remove(index).unwrap();
                    if let Some(previous) = app.permission.replace(notice) {
                        app.wb.permissions.push_front(previous);
                    }
                }
            }
            if let Some(index) = app.sessions.iter().position(|view| view.session.id == id) {
                app.select_session(index);
            }
            app.mode = InputMode::Editing;
            app.wb.drawer = false;
            app.open_permissions();
        }
    }
    AppAction::None
}

pub fn action(app: &mut App, command: &'static str) -> AppAction {
    if command == "/permissions" {
        if let Some(Panel::Error { feedback, .. }) = &app.wb.attention.panel {
            let notice = app
                .permission
                .iter()
                .chain(app.wb.permissions.iter())
                .find(|notice| Some(notice.session_id) == feedback.session);
            if let Some(notice) = notice {
                return activate(
                    app,
                    Key::Permission(notice.session_id, notice.request_id.clone()),
                );
            }
        }
    }
    close(app);
    app.command(command)
}

pub fn move_cursor(app: &mut App, up: bool, rows: usize) {
    let items = app.attention_items();
    match app.wb.attention.panel.as_mut() {
        Some(Panel::Inbox { selected }) => {
            let current = items
                .iter()
                .position(|item| Some(&item.key) == selected.as_ref())
                .unwrap_or(0);
            let next = if up {
                current.saturating_sub(rows)
            } else {
                (current + rows).min(items.len().saturating_sub(1))
            };
            *selected = items.get(next).map(|item| item.key.clone());
        }
        Some(Panel::Error { scroll, .. }) => {
            *scroll = if up {
                scroll.saturating_sub(rows)
            } else {
                scroll.saturating_add(rows).min(u16::MAX as usize)
            };
        }
        None => {}
    }
    app.wb.control_focus = None;
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    app.wb.attention.panel.as_ref()?;
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => return crate::interaction::keyboard(app, key),
        KeyCode::Esc => close(app),
        KeyCode::Up | KeyCode::PageUp => {
            move_cursor(app, true, if key.code == KeyCode::PageUp { 8 } else { 1 })
        }
        KeyCode::Down | KeyCode::PageDown => move_cursor(
            app,
            false,
            if key.code == KeyCode::PageDown { 8 } else { 1 },
        ),
        KeyCode::Enter => {
            if app.wb.control_focus.is_some() {
                return crate::interaction::keyboard(app, key);
            }
            if let Some(Panel::Inbox { selected }) = &app.wb.attention.panel {
                let items = app.attention_items();
                let item = if let Some(selected) = selected {
                    items.iter().find(|item| &item.key == selected)
                } else {
                    items.first()
                };
                if let Some(item) = item {
                    return Some(activate(app, item.key.clone()));
                }
            }
        }
        _ => {}
    }
    Some(AppAction::None)
}

pub fn banner(frame: &mut Frame, app: &App, area: Rect) {
    let Some(error) = app.wb.attention.latest_error() else {
        return;
    };
    let label = if area.width < 40 {
        "Error"
    } else {
        "Error details"
    };
    let control_width = label.len() as u16 + 2;
    let text_width = area.width.saturating_sub(control_width + 1);
    let source = error
        .session
        .map(|id| {
            if app.sessions.iter().any(|view| view.session.id == id) {
                format!("{}: ", app.agent_instance(id))
            } else {
                format!("{}: ", identity(app, id))
            }
        })
        .unwrap_or_default();
    let message = error.message.lines().next().unwrap_or("");
    frame.render_widget(
        Paragraph::new(fit_text(
            &format!("{source}{message}"),
            usize::from(text_width),
        ))
        .style(THEME.error),
        Rect::new(area.x, area.y, text_width, 1),
    );
    let control = Rect::new(
        area.right().saturating_sub(control_width),
        area.y,
        control_width,
        1,
    );
    buttons(frame, app, control, &[(label, Target::Command("/error"))]);
    frame.buffer_mut().set_style(control, THEME.error);
}

pub fn draw(frame: &mut Frame, app: &App) {
    let Some(panel) = &app.wb.attention.panel else {
        return;
    };
    app.wb.hits.borrow_mut().clear();
    let inner = crate::shell::modal(
        frame,
        76,
        22,
        match panel {
            Panel::Inbox { .. } => " Pending ",
            Panel::Error { .. } => " Error details ",
        },
    );
    match panel {
        Panel::Inbox { selected } => {
            let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
            buttons(frame, app, parts[1], &[("Close", Target::Close)]);
            let items = app.attention_items();
            if items.is_empty() {
                frame.render_widget(Paragraph::new("Nothing pending").style(THEME.dim), parts[0]);
                return;
            }
            let selected = items
                .iter()
                .position(|item| Some(&item.key) == selected.as_ref())
                .unwrap_or(0);
            let mut lines = vec![];
            let mut targets = vec![];
            let mut group = None;
            let mut selected_row = 0;
            for (index, item) in items.iter().enumerate() {
                if group != Some(item.label) {
                    lines.push(ListItem::new(Line::styled(item.label, THEME.section)));
                    targets.push((1, None));
                    group = Some(item.label);
                }
                if selected == index {
                    selected_row = lines.len();
                }
                let identity = item
                    .session
                    .map(|id| identity(app, id))
                    .unwrap_or_else(|| "Workbench".into());
                let width = usize::from(parts[0].width.saturating_sub(2));
                lines.push(ListItem::new(vec![
                    Line::styled(fit_text(&identity, width), THEME.text),
                    Line::styled(
                        fit_text(item.summary.lines().next().unwrap_or(""), width),
                        if item.label == "Failed" {
                            THEME.error
                        } else {
                            THEME.dim
                        },
                    ),
                ]));
                targets.push((2, Some(item.key.clone())));
            }
            let mut state = ListState::default().with_selected(Some(selected_row));
            frame.render_stateful_widget(
                List::new(lines)
                    .highlight_style(THEME.selection)
                    .highlight_symbol("› "),
                parts[0],
                &mut state,
            );
            let mut y = parts[0].y;
            for (height, key) in targets.into_iter().skip(state.offset()) {
                if y + height > parts[0].bottom() {
                    break;
                }
                if let Some(key) = key {
                    hit(
                        app,
                        Rect::new(parts[0].x, y, parts[0].width, height),
                        Target::AttentionItem(key),
                    );
                }
                y += height;
            }
        }
        Panel::Error { feedback, scroll } => {
            let parts = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(if inner.width < 38 { 3 } else { 1 }),
            ])
            .split(inner);
            let identity = feedback
                .session
                .map(|id| identity(app, id))
                .unwrap_or_else(|| "Workbench".into());
            frame.render_widget(
                Paragraph::new(fit_text(&identity, usize::from(parts[0].width)))
                    .style(THEME.accent),
                parts[0],
            );
            let paragraph = Paragraph::new(feedback.message.clone())
                .wrap(Wrap { trim: false })
                .style(THEME.error);
            let max = paragraph
                .line_count(parts[1].width)
                .saturating_sub(usize::from(parts[1].height));
            frame.render_widget(
                paragraph.scroll(((*scroll).min(max).min(u16::MAX as usize) as u16, 0)),
                parts[1],
            );
            let mut choices = vec![("Close", Target::Close)];
            if let Some(id) = feedback
                .session
                .filter(|id| Some(*id) == app.selected_session_id())
            {
                if app.selected_session().is_some_and(|view| {
                    matches!(
                        view.session.state,
                        SessionState::Done | SessionState::Error(_)
                    )
                }) {
                    choices.push(("Resume", Target::AttentionAction("/resume")));
                }
                if app
                    .permission
                    .iter()
                    .chain(app.wb.permissions.iter())
                    .any(|notice| notice.session_id == id)
                {
                    choices.push(("Permissions", Target::AttentionAction("/permissions")));
                }
                if app
                    .wb
                    .failed
                    .get(&id)
                    .is_some_and(|messages| !messages.is_empty())
                {
                    choices.push(("Recover message", Target::AttentionAction("/recover")));
                }
            }
            buttons(frame, app, parts[2], &choices);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{PendingRelay, SessionView};
    use agentmux_core::{AgentId, Session, WorkspaceId};
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};

    fn app(count: usize) -> App {
        let workspace_id = WorkspaceId::new();
        let sessions = (0..count)
            .map(|_| SessionView {
                session: Session {
                    id: SessionId::new(),
                    workspace_id,
                    agent_id: AgentId::new("mock"),
                    state: SessionState::Ready,
                    acp_session_id: None,
                    native_session_file: None,
                    native_terminal: false,
                    references: vec![],
                    created_at: Utc::now(),
                },
                agent_name: "Agent".into(),
                workspace_name: "space".into(),
            })
            .collect();
        App::new(vec![], vec![], sessions, vec![])
    }

    fn state(app: &mut App, index: usize, seq: u64, from: SessionState, to: SessionState) -> Event {
        let event = Event {
            session_id: app.sessions[index].session.id,
            seq,
            ts: Utc::now(),
            kind: EventKind::StateChanged { from, to },
        };
        app.handle_event(event.clone());
        event
    }

    fn permission(app: &mut App, index: usize, seq: u64) -> Event {
        let event = Event {
            session_id: app.sessions[index].session.id,
            seq,
            ts: Utc::now(),
            kind: EventKind::PermissionRequest {
                request_id: "same-request".into(),
                request: serde_json::json!({"toolCall":{"title":"Run tests"}, "options":[{"kind":"allow_once"}]}),
            },
        };
        app.handle_event(event.clone());
        event
    }

    /// A live overlap on a background session becomes one pending item
    /// naming the other agent; duplicates, history and viewing do not
    /// inflate or keep it.
    #[test]
    fn live_file_overlap_is_pending_once_and_clears_when_viewed() {
        let mut app = app(2);
        let (first, second) = (app.sessions[0].session.id, app.sessions[1].session.id);
        let overlap = Event {
            session_id: second,
            seq: 5,
            ts: Utc::now(),
            kind: EventKind::FileOverlap {
                path: "src/lib.rs".into(),
                others: vec![first],
            },
        };
        app.handle_event(overlap.clone());
        app.handle_event(overlap.clone());
        let items = app.attention_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "Overlaps");
        assert_eq!(
            items[0].summary,
            "src/lib.rs also edited recently by Agent #1"
        );
        activate(&mut app, Key::Overlap(second));
        assert_eq!(app.selected_session_id(), Some(second));
        assert!(app.attention_items().is_empty());

        // Replayed history never raises the notice again.
        app.wb.attention.baseline(first, 9);
        app.handle_event(Event {
            session_id: first,
            seq: 8,
            ts: Utc::now(),
            kind: EventKind::FileOverlap {
                path: "src/lib.rs".into(),
                others: vec![second],
            },
        });
        assert!(app.attention_items().is_empty());
    }

    fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        app.wb.columns = width;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::shell::draw(frame, app))
            .unwrap();
        terminal
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn background_permissions_failures_and_completions_do_not_steal_focus() {
        let mut app = app(4);
        app.insert_text("中文草稿👩‍💻");
        let selected = app.selected_session_id();
        let cursor = app.wb.cursor;
        permission(&mut app, 1, 1);
        state(
            &mut app,
            2,
            2,
            SessionState::Prompting,
            SessionState::Error("adapter failed".into()),
        );
        state(&mut app, 3, 3, SessionState::Prompting, SessionState::Ready);
        let items = app.attention_items();
        assert_eq!(
            items.iter().map(|item| item.label).collect::<Vec<_>>(),
            vec!["Permissions", "Failed", "Finished"]
        );
        assert_eq!(app.selected_session_id(), selected);
        assert_eq!(app.mode, InputMode::Editing);
        assert_eq!(app.wb.cursor, cursor);
        assert_eq!(app.input, "中文草稿👩‍💻");
        assert!(app.wb.attention.panel.is_none());
    }

    #[test]
    fn duplicate_events_and_text_chunks_do_not_inflate_pending_counts() {
        let mut app = app(4);
        let request = permission(&mut app, 1, 1);
        let failure = state(
            &mut app,
            2,
            2,
            SessionState::Prompting,
            SessionState::Error("failed".into()),
        );
        let finished = state(&mut app, 3, 3, SessionState::Prompting, SessionState::Ready);
        for seq in 10..30 {
            app.handle_event(request.clone());
            app.handle_event(failure.clone());
            app.handle_event(finished.clone());
            app.handle_event(Event { session_id: finished.session_id, seq, ts: Utc::now(), kind: EventKind::SessionUpdate(serde_json::json!({"sessionUpdate":"agent_message_chunk","content":{"text":"chunk"}})) });
        }
        assert_eq!(app.attention_items().len(), 3);
        activate(&mut app, Key::Finished(finished.session_id));
        app.handle_event(finished);
        assert_eq!(app.attention_items().len(), 2);
    }

    #[test]
    fn failure_before_session_metadata_is_visible_and_not_reopened_on_hydration() {
        let mut app = app(1);
        let id = SessionId::new();
        app.handle_event(Event {
            session_id: id,
            seq: 1,
            ts: Utc::now(),
            kind: EventKind::StateChanged {
                from: SessionState::Connecting,
                to: SessionState::Error("setup failed".into()),
            },
        });
        assert_eq!(app.attention_items().len(), 1);
        assert!(identity(&app, id).contains("Unknown agent"));
        let selected = app.selected_session_id();
        activate(&mut app, Key::Failure(Some(id)));
        assert_eq!(app.selected_session_id(), selected);
        assert!(app.attention_items().is_empty());
        let mut view = app.sessions[0].clone();
        view.session.id = id;
        view.session.state = SessionState::Error("setup failed".into());
        app.add_session(view);
        assert!(app.attention_items().is_empty());
    }

    #[test]
    fn finished_navigation_keeps_drafts_references_and_jumps_to_latest() {
        let mut app = app(2);
        let first = app.sessions[0].session.id;
        let second = app.sessions[1].session.id;
        app.insert_text("first draft");
        let reference = PendingRelay {
            source: second,
            target: first,
            seq: 7,
        };
        app.pending_relays.push(reference.clone());
        app.wb.sessions.entry(second).or_default().anchor = Some(1);
        state(&mut app, 1, 2, SessionState::Prompting, SessionState::Ready);
        activate(&mut app, Key::Finished(second));
        assert_eq!(app.selected_session_id(), Some(second));
        assert_eq!(app.wb.sessions[&first].draft, "first draft");
        assert_eq!(app.wb.sessions[&first].relays, vec![reference.clone()]);
        assert!(app.wb.sessions[&second].anchor.is_none());
        assert!(app.attention_items().is_empty());
        app.select_session(0);
        assert_eq!(app.input, "first draft");
        assert_eq!(app.pending_relays, vec![reference]);
    }

    #[test]
    fn only_unviewed_turn_endings_create_finished_items() {
        let mut app = app(2);
        state(&mut app, 0, 1, SessionState::Prompting, SessionState::Ready);
        state(
            &mut app,
            1,
            1,
            SessionState::Connecting,
            SessionState::Ready,
        );
        assert!(app.attention_items().is_empty());
        app.wb
            .sessions
            .entry(app.sessions[0].session.id)
            .or_default()
            .anchor = Some(1);
        state(&mut app, 0, 2, SessionState::Prompting, SessionState::Ready);
        assert_eq!(app.attention_items().len(), 1);
        app.follow_latest();
        assert!(app.attention_items().is_empty());
        state(&mut app, 1, 2, SessionState::Prompting, SessionState::Ready);
        state(&mut app, 1, 3, SessionState::Ready, SessionState::Prompting);
        assert!(app.attention_items().is_empty());
    }

    #[test]
    fn history_does_not_reopen_reviewed_failures_or_create_completions() {
        let mut app = app(2);
        let id = app.sessions[1].session.id;
        app.sessions[1].session.state = SessionState::Error("stored failure\nfull detail".into());
        activate(&mut app, Key::Failure(Some(id)));
        assert!(
            matches!(&app.wb.attention.panel, Some(Panel::Error { feedback, .. }) if feedback.message.ends_with("full detail"))
        );
        close(&mut app);
        let event = Event {
            session_id: id,
            seq: 11,
            ts: Utc::now(),
            kind: EventKind::StateChanged {
                from: SessionState::Prompting,
                to: SessionState::Error("stored failure\nfull detail".into()),
            },
        };
        app.merge_history(id, vec![event.clone()], true, None, 0);
        app.handle_event(event);
        assert!(app.attention_items().is_empty());
        state(
            &mut app,
            1,
            12,
            SessionState::Error("stored failure".into()),
            SessionState::Prompting,
        );
        state(
            &mut app,
            1,
            13,
            SessionState::Prompting,
            SessionState::Error("stored failure\nfull detail".into()),
        );
        assert_eq!(
            app.attention_items().len(),
            1,
            "a genuinely new failure must notify again"
        );
        let mut other = self::app(2);
        let id = other.sessions[1].session.id;
        other.merge_history(
            id,
            vec![Event {
                session_id: id,
                seq: 5,
                ts: Utc::now(),
                kind: EventKind::StateChanged {
                    from: SessionState::Prompting,
                    to: SessionState::Ready,
                },
            }],
            false,
            None,
            0,
        );
        assert!(other.attention_items().is_empty());
    }

    #[test]
    fn permission_selection_routes_same_request_ids_to_the_right_agent() {
        let mut app = app(3);
        let first = app.sessions[0].session.id;
        let second = app.sessions[1].session.id;
        let third = app.sessions[2].session.id;
        app.insert_text("source draft");
        permission(&mut app, 1, 1);
        permission(&mut app, 2, 1);
        open(&mut app);
        activate(&mut app, Key::Permission(third, "same-request".into()));
        assert_eq!(app.permission.as_ref().unwrap().session_id, third);
        assert_eq!(app.wb.permissions.front().unwrap().session_id, second);
        assert_eq!(app.selected_session_id(), Some(third));
        assert_eq!(app.wb.sessions[&first].draft, "source draft");
        assert_eq!(app.mode, InputMode::Permission);
        app.handle_event(Event {
            session_id: third,
            seq: 2,
            ts: Utc::now(),
            kind: EventKind::PermissionResolved {
                request_id: "same-request".into(),
                outcome: "allowed".into(),
            },
        });
        assert_eq!(app.permission_count(), 1);
        assert_eq!(app.mode, InputMode::Editing);
        assert_eq!(app.selected_session_id(), Some(third));
    }

    #[test]
    fn pending_permission_snapshots_can_hydrate_already_loaded_history() {
        let mut app = app(2);
        let request = permission(&mut app, 1, 4);
        app.permission = None;
        app.handle_event(request.clone());
        app.handle_event(request);
        assert_eq!(app.permission_count(), 1);
        assert_eq!(app.events.len(), 1);
    }

    #[test]
    fn stale_item_targets_and_old_states_do_not_route_or_recreate_work() {
        let mut app = app(2);
        let id = app.sessions[1].session.id;
        permission(&mut app, 1, 1);
        open(&mut app);
        app.handle_event(Event {
            session_id: id,
            seq: 2,
            ts: Utc::now(),
            kind: EventKind::PermissionResolved {
                request_id: "same-request".into(),
                outcome: "allowed".into(),
            },
        });
        activate(&mut app, Key::Permission(id, "same-request".into()));
        assert_eq!(app.selected, 0);
        assert_eq!(app.mode, InputMode::Editing);
        assert!(app.attention_items().is_empty());
        close(&mut app);
        let failure = state(
            &mut app,
            1,
            3,
            SessionState::Prompting,
            SessionState::Error("old failure".into()),
        );
        activate(&mut app, Key::Failure(Some(id)));
        close(&mut app);
        state(
            &mut app,
            1,
            5,
            SessionState::Connecting,
            SessionState::Ready,
        );
        app.events.clear();
        app.handle_event(failure);
        assert_eq!(app.sessions[1].session.state, SessionState::Ready);
        assert!(app.attention_items().is_empty());
    }

    #[test]
    fn failures_stay_visible_while_busy_even_after_success_statuses() {
        let mut app = app(1);
        app.insert_text("draft");
        state(&mut app, 0, 1, SessionState::Ready, SessionState::Prompting);
        app.set_error("operation failed: full error\nadditional details");
        app.set_status("turn finished");
        let terminal = render(&mut app, 40, 16);
        let output = text(&terminal);
        assert!(output.contains("operation failed"), "{output}");
        assert!(output.contains("Waiting"), "{output}");
        assert!(output.contains("1 pending"), "{output}");
        assert!(app
            .wb
            .attention
            .latest_error()
            .unwrap()
            .message
            .contains("additional details"));
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn details_are_readonly_scrollable_and_do_not_acknowledge_new_errors() {
        let mut app = app(1);
        app.insert_text("中文草稿");
        app.set_error(format!("FIRST\n{}LAST", "detail row\n".repeat(60)));
        error_details(&mut app);
        let terminal = render(&mut app, 40, 16);
        assert!(text(&terminal).contains("FIRST"));
        app.paste_text("should not replace draft");
        app.handle_key(KeyEvent::new(
            KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        ));
        move_cursor(&mut app, false, 100);
        let terminal = render(&mut app, 40, 16);
        assert!(text(&terminal).contains("LAST"));
        app.set_error("new failure");
        assert!(
            matches!(&app.wb.attention.panel, Some(Panel::Error { feedback, .. }) if feedback.message.starts_with("FIRST"))
        );
        close(&mut app);
        assert_eq!(app.input, "中文草稿");
        assert_eq!(app.attention_items().len(), 1);
        assert_eq!(
            app.wb.attention.latest_error().unwrap().message,
            "new failure"
        );
    }

    #[test]
    fn reviewed_error_can_be_reopened_without_creating_another_notice() {
        let mut app = app(1);
        let id = app.sessions[0].session.id;
        app.set_error("full error\nretained details");
        error_details(&mut app);
        close(&mut app);
        error_details(&mut app);
        assert!(
            matches!(&app.wb.attention.panel, Some(Panel::Error { feedback, .. }) if feedback.message.ends_with("retained details"))
        );
        assert!(app.attention_items().is_empty());
        close(&mut app);
        app.start_conversation(id, 5);
        error_details(&mut app);
        assert!(
            app.wb.attention.panel.is_none(),
            "a new conversation must not reopen the old error"
        );
    }

    #[test]
    fn multiline_state_errors_render_without_control_characters_in_cells() {
        let mut app = app(1);
        let message = format!("FIRST\n{}LAST", "detail row\n".repeat(70));
        state(
            &mut app,
            0,
            1,
            SessionState::Prompting,
            SessionState::Error(message.clone()),
        );
        let terminal = render(&mut app, 120, 30);
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .all(|cell| !cell.symbol().contains(['\n', '\r'])));
        assert_eq!(app.wb.attention.latest_error().unwrap().message, message);
        error_details(&mut app);
        move_cursor(&mut app, false, 200);
        let terminal = render(&mut app, 40, 16);
        assert!(text(&terminal).contains("LAST"));
    }

    #[test]
    fn modal_borders_are_not_erased_by_background_wide_characters() {
        let mut app = app(1);
        state(
            &mut app,
            0,
            1,
            SessionState::Prompting,
            SessionState::Error(format!("FIRST\n{}\n{}", "界".repeat(40), "界".repeat(40))),
        );
        error_details(&mut app);
        let terminal = render(&mut app, 120, 30);
        let rect = crate::ui::centered(Rect::new(0, 0, 120, 30), 76, 22);
        for row in rect.y + 1..rect.bottom() - 1 {
            assert_eq!(
                terminal.backend().buffer()[(rect.x, row)].symbol(),
                "│",
                "left border row {row}"
            );
            assert_eq!(
                terminal.backend().buffer()[(rect.right() - 1, row)].symbol(),
                "│",
                "right border row {row}"
            );
        }
    }

    #[test]
    fn pending_controls_and_overlay_hits_fit_tiny_and_wide_windows() {
        for (width, height) in [(20, 8), (40, 16), (80, 24), (120, 30), (160, 40)] {
            let mut app = app(4);
            permission(&mut app, 1, 1);
            state(
                &mut app,
                2,
                2,
                SessionState::Prompting,
                SessionState::Error("failed".into()),
            );
            state(&mut app, 3, 3, SessionState::Prompting, SessionState::Ready);
            render(&mut app, width, height);
            assert!(app
                .wb
                .hits
                .borrow()
                .iter()
                .any(|hit| hit.target == Target::Command("/attention")));
            open(&mut app);
            render(&mut app, width, height);
            let hits = app.wb.hits.borrow();
            assert!(hits
                .iter()
                .all(|hit| matches!(hit.target, Target::AttentionItem(_) | Target::Close)));
            let frame = Rect::new(0, 0, width, height);
            assert!(hits
                .iter()
                .all(|hit| hit.area.intersection(frame) == hit.area));
            drop(hits);
            app.handle_key(KeyEvent::new(
                KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(app.mode, InputMode::Editing);
        }
    }

    #[test]
    fn failure_details_offer_correct_agent_resume_and_permission_retry() {
        let mut app = app(3);
        let second = app.sessions[1].session.id;
        let third = app.sessions[2].session.id;
        permission(&mut app, 1, 1);
        permission(&mut app, 2, 1);
        app.permission_answer_failed(third, "same-request", "transport failed".into());
        activate(&mut app, Key::Failure(Some(third)));
        render(&mut app, 40, 16);
        assert!(app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|hit| hit.target == Target::AttentionAction("/permissions")));
        action(&mut app, "/permissions");
        assert_eq!(app.permission.as_ref().unwrap().session_id, third);
        assert_eq!(app.wb.permissions.front().unwrap().session_id, second);
        app.mode = InputMode::Editing;
        state(
            &mut app,
            1,
            2,
            SessionState::Prompting,
            SessionState::Error("stopped".into()),
        );
        activate(&mut app, Key::Failure(Some(second)));
        render(&mut app, 40, 16);
        assert!(app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|hit| hit.target == Target::AttentionAction("/resume")));
        assert_eq!(action(&mut app, "/resume"), AppAction::ResumeSession);
        assert_eq!(app.selected_session_id(), Some(second));
    }

    #[test]
    #[ignore = "manual pending/error workbench preview"]
    fn print_attention_frame() {
        let mut app = app(4);
        let workspace_id = app.sessions[0].session.workspace_id;
        app.workspaces.push(agentmux_core::Workspace {
            id: workspace_id,
            project_id: agentmux_core::ProjectId::new(),
            name: "ui-review".into(),
            branch: "agentmux/ui-review".into(),
            managed_worktree: true,
            worktree_path: "/repo/.agentmux/worktrees/ui-review".into(),
            created_at: Utc::now(),
        });
        for (index, (name, title)) in [
            ("Pi", "改进多 Agent 工作台"),
            ("Claude", "审批测试命令"),
            ("Codex", "检查工具配置"),
            ("Pi", "回归检查"),
        ]
        .iter()
        .enumerate()
        {
            app.sessions[index].agent_name = (*name).into();
            app.sessions[index].session.agent_id = AgentId::new(name.to_lowercase());
            app.sessions[index].workspace_name = "ui-review".into();
            app.apply_title(app.sessions[index].session.id, (*title).into(), 1);
        }
        state(&mut app, 0, 1, SessionState::Ready, SessionState::Prompting);
        app.handle_event(Event { session_id: app.sessions[0].session.id, seq: 2, ts: Utc::now(), kind: EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"text":"正在检查待处理事项和运行状态。\n后台 Agent 的授权和错误不会打断当前草稿。"}
        })) });
        permission(&mut app, 1, 1);
        state(
            &mut app,
            1,
            2,
            SessionState::Prompting,
            SessionState::WaitingPermission,
        );
        state(&mut app, 2, 1, SessionState::Prompting, SessionState::Error("测试工具启动失败：找不到配置文件。\n\n请求：检查工具配置\n目录：/repo/.agentmux/worktrees/ui-review\n\n原对话和未发送消息均已保留。\n请检查配置后重试连接。".into()));
        state(&mut app, 3, 1, SessionState::Prompting, SessionState::Ready);
        app.insert_text("保留这份草稿，先检查其他 Agent 的结果。");
        match std::env::var("AGENTMUX_PREVIEW_VIEW").as_deref() {
            Ok("workbench") => {}
            Ok("error") => error_details(&mut app),
            _ => open(&mut app),
        }
        let width = std::env::var("AGENTMUX_PREVIEW_WIDTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(120);
        let height = std::env::var("AGENTMUX_PREVIEW_HEIGHT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(30);
        let terminal = render(&mut app, width, height);
        let buffer = terminal.backend().buffer();
        if let Ok(path) = std::env::var("AGENTMUX_PREVIEW_PATH") {
            let cells: Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({
                "text":cell.symbol(), "fg":format!("{:?}", cell.fg), "bg":format!("{:?}", cell.bg),
                "bold":cell.modifier.contains(ratatui::style::Modifier::BOLD),
            })).collect();
            std::fs::write(
                path,
                serde_json::to_vec(
                    &serde_json::json!({"width":width,"height":height,"cells":cells}),
                )
                .unwrap(),
            )
            .unwrap();
        }
        for row in buffer.content.chunks(usize::from(width)) {
            println!(
                "{}",
                row.iter().map(|cell| cell.symbol()).collect::<String>()
            );
        }
    }
}
