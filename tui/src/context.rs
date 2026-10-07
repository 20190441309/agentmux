//! Workspace-scoped blackboard previews and unsaved, conflict-checked editor drafts.
use crate::{
    app::{App, AppAction},
    external::EditRequest,
    interaction::{buttons, Target},
    theme::THEME,
    workbench::SideTab,
};
use agentmux_core::{
    rpc::{WorkspaceContextResult, WorkspaceContextSaveParams},
    WorkspaceId,
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    widgets::{Paragraph, Wrap},
    Frame,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum View {
    #[default]
    Context,
    Activity,
    Draft,
}
pub struct Draft {
    pub expected: Option<String>,
    pub text: String,
}
#[derive(Default)]
pub struct Snapshot {
    pub data: Option<WorkspaceContextResult>,
    pub error: Option<String>,
    pub save_error: Option<String>,
    pub request: Option<u64>,
    pub checked: Option<Instant>,
    pub draft: Option<Draft>,
}
#[derive(Default)]
pub struct Context {
    pub snapshots: HashMap<WorkspaceId, Snapshot>,
    pub serial: u64,
    pub view: View,
    pub panel: Option<WorkspaceId>,
}

pub fn visible(app: &App) -> bool {
    app.wb.context.panel.is_some() || (app.wb.tab == SideTab::Context && app.sidebar_visible())
}

pub fn open(app: &mut App) {
    app.wb.context.panel = app.file_workspace();
    app.wb.menu = false;
    app.wb.control_focus = None;
    app.wb.context_scroll = 0;
}
pub fn close(app: &mut App) {
    app.wb.context.panel = None;
    app.wb.control_focus = None;
}
pub fn keyboard(app: &mut App, key: crossterm::event::KeyEvent) -> Option<AppAction> {
    use crossterm::event::KeyCode;
    app.wb.context.panel?;
    match key.code {
        KeyCode::Esc => close(app),
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter => {
            return crate::interaction::keyboard(app, key).or(Some(AppAction::None))
        }
        KeyCode::Up | KeyCode::PageUp => {
            app.wb.context_scroll = app.wb.context_scroll.saturating_sub(8)
        }
        KeyCode::Down | KeyCode::PageDown => {
            app.wb.context_scroll = app.wb.context_scroll.saturating_add(8)
        }
        KeyCode::Home => app.wb.context_scroll = 0,
        KeyCode::End => app.wb.context_scroll = u16::MAX,
        _ => {}
    }
    Some(AppAction::None)
}

impl App {
    pub fn context_refresh(&mut self, force: bool) -> Option<(WorkspaceId, u64)> {
        if !self.wb.connected {
            return None;
        }
        let workspace = self.wb.context.panel.or(self.file_workspace())?;
        let snapshot = self.wb.context.snapshots.entry(workspace).or_default();
        if snapshot.request.is_some()
            || (!force
                && snapshot
                    .checked
                    .is_some_and(|time| time.elapsed() < Duration::from_secs(5)))
        {
            return None;
        }
        self.wb.context.serial += 1;
        snapshot.request = Some(self.wb.context.serial);
        Some((workspace, self.wb.context.serial))
    }
    pub fn context_reply(
        &mut self,
        workspace: WorkspaceId,
        request: u64,
        saving: bool,
        result: Result<WorkspaceContextResult, String>,
    ) {
        let snapshot = self.wb.context.snapshots.entry(workspace).or_default();
        if snapshot.request != Some(request) {
            return;
        }
        snapshot.request = None;
        snapshot.checked = Some(Instant::now());
        match result {
            Ok(data) => {
                snapshot.data = Some(data);
                snapshot.error = None;
                if saving {
                    snapshot.draft = None;
                    snapshot.save_error = None;
                }
            }
            Err(error) => {
                if saving {
                    snapshot.save_error = Some(error.clone());
                } else {
                    snapshot.error = Some(error.clone());
                }
                self.set_error_for(
                    None,
                    format!("Shared context: {error}. Any unsaved edit is retained."),
                );
            }
        }
    }
}

pub fn edit(app: &mut App) -> AppAction {
    let Some(workspace) = app.file_workspace() else {
        return AppAction::None;
    };
    let Some(snapshot) = app.wb.context.snapshots.get(&workspace) else {
        return AppAction::RefreshContext;
    };
    let Some(data) = &snapshot.data else {
        app.set_error("Shared context is not loaded. Refresh before editing.");
        return AppAction::RefreshContext;
    };
    if data.context_truncated {
        app.set_error(
            "Shared context preview is truncated; editing is disabled to preserve the full file.",
        );
        return AppAction::None;
    }
    let text = if let Some(draft) = &snapshot.draft {
        if draft.expected != data.context {
            format!(
                "<<<<<<< LOCAL EDIT\n{}\n=======\n{}\n>>>>>>> CURRENT SHARED CONTEXT\n",
                draft.text,
                data.context.as_deref().unwrap_or("")
            )
        } else {
            draft.text.clone()
        }
    } else {
        data.context.clone().unwrap_or_default()
    };
    app.wb.external_edit = Some(EditRequest::Context {
        workspace,
        original: data.context.clone(),
        text,
    });
    AppAction::None
}

pub fn save(app: &mut App) -> AppAction {
    let Some(workspace) = app.file_workspace() else {
        return AppAction::None;
    };
    let Some(snapshot) = app.wb.context.snapshots.get(&workspace) else {
        return AppAction::None;
    };
    if snapshot.request.is_some() {
        app.set_status("Shared context request already running.");
        return AppAction::None;
    }
    let Some(draft) = &snapshot.draft else {
        return AppAction::None;
    };
    if draft.text.contains("<<<<<<< LOCAL EDIT")
        || draft.text.contains(">>>>>>> CURRENT SHARED CONTEXT")
    {
        app.set_error("Resolve the shared-context merge markers before saving. Edit retained.");
        return AppAction::None;
    }
    AppAction::SaveContext(WorkspaceContextSaveParams {
        workspace_id: workspace,
        expected: draft.expected.clone(),
        text: draft.text.clone(),
    })
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);
    buttons(
        frame,
        app,
        parts[0],
        &[
            ("Shared", Target::ContextView(View::Context)),
            ("Activity", Target::ContextView(View::Activity)),
            ("Draft", Target::ContextView(View::Draft)),
        ],
    );
    let snapshot = app
        .file_workspace()
        .and_then(|id| app.wb.context.snapshots.get(&id));
    let text = match snapshot {
        None => "Loading shared context…".into(),
        Some(snapshot) if snapshot.data.is_none() => snapshot
            .error
            .clone()
            .unwrap_or_else(|| "Loading shared context…".into()),
        Some(snapshot) => {
            let data = snapshot.data.as_ref().unwrap();
            let (text, truncated) = match app.wb.context.view {
                View::Context => (
                    data.context
                        .as_deref()
                        .unwrap_or("Shared context file is missing"),
                    data.context_truncated,
                ),
                View::Activity => (
                    data.activity.as_deref().unwrap_or("No activity file"),
                    data.activity_truncated,
                ),
                View::Draft => (
                    snapshot
                        .draft
                        .as_ref()
                        .map(|draft| draft.text.as_str())
                        .unwrap_or("No unsaved context edit"),
                    false,
                ),
            };
            format!(
                "{}{}{}{}{}",
                snapshot
                    .save_error
                    .as_ref()
                    .map(|error| format!("Save failed: {error}\n\n"))
                    .unwrap_or_default(),
                if snapshot.error.is_some() {
                    "Stale preview · refresh failed\n\n"
                } else {
                    ""
                },
                if snapshot.draft.is_some() {
                    "Unsaved context edit\n\n"
                } else {
                    ""
                },
                text,
                if truncated {
                    "\n[Preview truncated]"
                } else {
                    ""
                }
            )
        }
    };
    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .style(THEME.text);
    let limit = paragraph
        .line_count(parts[1].width)
        .saturating_sub(usize::from(parts[1].height))
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        paragraph.scroll((app.wb.context_scroll.min(limit), 0)),
        parts[1],
    );
    crate::interaction::hit(app, parts[1], Target::ContextBody);
    buttons(
        frame,
        app,
        parts[2],
        &[
            ("Open", Target::Command("/context-preview")),
            ("Refresh", Target::Command("/refresh-context")),
            ("Edit", Target::Command("/edit-context")),
            ("Save", Target::Command("/save-context")),
        ],
    );
}

pub fn draw_panel(frame: &mut Frame, app: &App) {
    if app.wb.context.panel.is_none() {
        return;
    }
    app.wb.hits.borrow_mut().clear();
    let inner = crate::shell::modal(
        frame,
        96,
        frame.area().height.saturating_sub(2).max(8),
        " Workspace context ",
    );
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    draw(frame, app, parts[0]);
    buttons(frame, app, parts[1], &[("Close", Target::Close)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SessionView;
    use agentmux_core::{AgentId, Session, SessionId, SessionState};
    use chrono::Utc;
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
    #[test]
    fn context_replies_are_workspace_bound_and_conflicts_keep_both_drafts() {
        let mut app = app();
        app.insert_text("chat draft");
        let (workspace, request) = app.context_refresh(true).unwrap();
        app.context_reply(
            workspace,
            request,
            false,
            Ok(WorkspaceContextResult {
                context: Some("original".into()),
                ..Default::default()
            }),
        );
        app.wb.context.snapshots.get_mut(&workspace).unwrap().draft = Some(Draft {
            expected: Some("original".into()),
            text: "my edit".into(),
        });
        let (workspace, request) = app.context_refresh(true).unwrap();
        app.context_reply(workspace, request, true, Err("conflict".into()));
        assert_eq!(
            app.wb.context.snapshots[&workspace]
                .draft
                .as_ref()
                .unwrap()
                .text,
            "my edit"
        );
        assert_eq!(app.input, "chat draft");
        app.sessions[0].session.workspace_id = WorkspaceId::new();
        app.context_reply(
            workspace,
            request,
            false,
            Ok(WorkspaceContextResult::default()),
        );
        assert!(!app
            .wb
            .context
            .snapshots
            .contains_key(&app.file_workspace().unwrap()));
        assert_eq!(app.input, "chat draft");
    }
}
