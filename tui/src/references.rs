//! Explicit review/removal of references belonging to one recipient's draft.
use crate::{
    app::{App, AppAction, PendingRelay},
    interaction::{buttons, hit, Target},
    shell::fit_text,
    theme::THEME,
};
use agentmux_core::{Event, SessionId};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct References {
    pub panel: Option<Panel>,
    pub cache: HashMap<(SessionId, u64), Result<Event, String>>,
    pub loading: HashSet<(SessionId, u64)>,
    pub markers: HashMap<(SessionId, SessionId, u64), String>,
}
pub struct Panel {
    pub target: SessionId,
    pub index: usize,
    pub scroll: usize,
}

pub fn items(app: &App, target: SessionId) -> Vec<PendingRelay> {
    if app.selected_session_id() == Some(target) {
        app.pending_relays.clone()
    } else {
        app.wb
            .sessions
            .get(&target)
            .map(|ui| ui.relays.clone())
            .unwrap_or_default()
    }
}
pub fn open(app: &mut App) {
    if let Some(target) = app.selected_session_id() {
        app.wb.menu = false;
        app.wb.control_focus = None;
        app.wb.references.panel = Some(Panel {
            target,
            index: 0,
            scroll: 0,
        });
    }
}
pub fn close(app: &mut App) {
    app.wb.references.panel = None;
    app.wb.control_focus = None;
}

pub fn needed(app: &mut App) -> Option<(SessionId, u64)> {
    let panel = app.wb.references.panel.as_ref()?;
    let reference = items(app, panel.target).get(panel.index)?.clone();
    let key = (reference.source, reference.seq);
    if app
        .events
        .iter()
        .any(|event| event.session_id == key.0 && event.seq == key.1)
        || app.wb.references.cache.contains_key(&key)
        || !app.wb.connected
    {
        return None;
    }
    app.wb.references.loading.insert(key).then_some(key)
}

fn remove_marker(text: &mut String, cursor: &mut usize, marker: &str) {
    if let Some(start) = text.find(marker) {
        text.replace_range(start..start + marker.len(), "");
        *cursor = if *cursor > start {
            cursor
                .saturating_sub(marker.len())
                .max(start)
                .min(text.len())
        } else {
            (*cursor).min(text.len())
        };
    }
}

pub fn remove(app: &mut App) {
    let Some(panel) = &app.wb.references.panel else {
        return;
    };
    let (target, index) = (panel.target, panel.index);
    let Some(reference) = items(app, target).get(index).cloned() else {
        return;
    };
    let marker = app
        .wb
        .references
        .markers
        .remove(&(target, reference.source, reference.seq));
    if app.selected_session_id() == Some(target) {
        app.pending_relays.remove(index);
        if let Some(marker) = marker {
            remove_marker(&mut app.input, &mut app.wb.cursor, &marker);
        }
    } else if let Some(ui) = app.wb.sessions.get_mut(&target) {
        ui.relays.remove(index);
        if let Some(marker) = marker {
            remove_marker(&mut ui.draft, &mut ui.cursor, &marker);
        }
    }
    let count = items(app, target).len();
    if let Some(panel) = &mut app.wb.references.panel {
        panel.index = panel.index.min(count.saturating_sub(1));
        panel.scroll = 0;
    }
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    app.wb.references.panel.as_ref()?;
    match key.code {
        KeyCode::Esc => close(app),
        KeyCode::Tab | KeyCode::BackTab => return crate::interaction::keyboard(app, key),
        KeyCode::Enter if app.wb.control_focus.is_some() => {
            return crate::interaction::keyboard(app, key)
        }
        KeyCode::Up | KeyCode::Down => {
            let target = app.wb.references.panel.as_ref().unwrap().target;
            let count = items(app, target).len();
            let panel = app.wb.references.panel.as_mut().unwrap();
            panel.index = if key.code == KeyCode::Up {
                panel.index.saturating_sub(1)
            } else {
                (panel.index + 1).min(count.saturating_sub(1))
            };
            panel.scroll = 0;
        }
        KeyCode::PageUp | KeyCode::PageDown => {
            let panel = app.wb.references.panel.as_mut().unwrap();
            panel.scroll = if key.code == KeyCode::PageUp {
                panel.scroll.saturating_sub(8)
            } else {
                panel.scroll.saturating_add(8)
            };
        }
        _ => {}
    }
    Some(AppAction::None)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let Some(panel) = &app.wb.references.panel else {
        return;
    };
    app.wb.hits.borrow_mut().clear();
    let inner = crate::shell::modal(
        frame,
        92,
        frame.area().height.saturating_sub(2).max(8),
        " Draft references ",
    );
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(fit_text(
            &format!(
                "To: {} / {}",
                app.agent_instance(panel.target),
                app.session_title(panel.target)
            ),
            usize::from(parts[0].width),
        ))
        .style(THEME.accent),
        parts[0],
    );
    let refs = items(app, panel.target);
    let items: Vec<_> = refs
        .iter()
        .map(|reference| {
            ListItem::new(format!(
                "{} · #{}",
                app.agent_instance(reference.source),
                reference.seq
            ))
        })
        .collect();
    let mut state = ListState::default()
        .with_selected((!refs.is_empty()).then(|| panel.index.min(refs.len() - 1)));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(THEME.selection),
        parts[1],
        &mut state,
    );
    for (row, _) in refs
        .iter()
        .enumerate()
        .skip(state.offset())
        .take(usize::from(parts[1].height))
    {
        hit(
            app,
            Rect::new(
                parts[1].x,
                parts[1].y + (row - state.offset()) as u16,
                parts[1].width,
                1,
            ),
            Target::Reference(row),
        );
    }
    let text = if let Some(reference) = refs.get(panel.index) {
        let key = (reference.source, reference.seq);
        let event = app
            .events
            .iter()
            .find(|event| event.session_id == key.0 && event.seq == key.1)
            .cloned()
            .or_else(|| {
                app.wb
                    .references
                    .cache
                    .get(&key)
                    .and_then(|result| result.as_ref().ok())
                    .cloned()
            });
        if let Some(event) = event {
            serde_json::to_string_pretty(&event).unwrap_or_default()
        } else if let Some(Err(error)) = app.wb.references.cache.get(&key) {
            format!("Reference unavailable: {error}")
        } else if !app.wb.connected {
            "Disconnected; reference contents not loaded".into()
        } else {
            "Loading reference contents…".into()
        }
    } else {
        "No references attached".into()
    };
    let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
    let max = paragraph
        .line_count(parts[2].width)
        .saturating_sub(usize::from(parts[2].height));
    frame.render_widget(
        paragraph.scroll((panel.scroll.min(max).min(u16::MAX as usize) as u16, 0)),
        parts[2],
    );
    buttons(
        frame,
        app,
        parts[3],
        &[
            ("Close", Target::Close),
            ("Remove", Target::Command("/remove-reference")),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SessionView;
    use agentmux_core::{AgentId, Session, SessionState, WorkspaceId};
    use chrono::Utc;
    #[test]
    fn removal_is_bound_to_original_recipient_after_selection_changes() {
        let mut app = App::new(
            vec![],
            vec![],
            (0..2)
                .map(|_| SessionView {
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
                })
                .collect(),
            vec![],
        );
        let target = app.sessions[0].session.id;
        let source = app.sessions[1].session.id;
        let marker = "[@source#1: quoted]";
        app.insert_text(&format!("target draft {marker}"));
        app.pending_relays.push(PendingRelay {
            source,
            target,
            seq: 1,
        });
        app.wb
            .references
            .markers
            .insert((target, source, 1), marker.into());
        open(&mut app);
        app.select_session(1);
        app.insert_text("other draft");
        remove(&mut app);
        assert_eq!(app.input, "other draft");
        assert!(app.wb.sessions[&target].relays.is_empty());
        assert_eq!(app.wb.sessions[&target].draft, "target draft ");
    }
}
