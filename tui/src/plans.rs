//! Delegation plans: propose a split of one goal across the agents of a
//! space, review and edit it, approve it, then follow and steer the run.
//!
//! State arrives as `PlanUpdated` events (and as RPC results); the panel is
//! a view over the newest plan of the selected space. Every action has a
//! button (`Target::Command`) so narrow screens never depend on key
//! memory; letters are shortcuts for the same commands.
use crate::{
    app::{App, AppAction},
    interaction::{buttons, Target},
    theme::THEME,
};
use agentmux_core::plan::{Plan, PlanState, PlanTaskSpec, TaskAction, TaskState};
use agentmux_core::{Event, EventKind, PlanId, SessionId, SessionState, WorkspaceId};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};
use std::collections::HashMap;

/// A daemon request issued on behalf of the plan panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanCall {
    Propose {
        workspace: WorkspaceId,
        session: SessionId,
        goal: String,
    },
    List(WorkspaceId),
    Control {
        method: &'static str,
        plan: PlanId,
    },
    Task {
        plan: PlanId,
        task: String,
        action: TaskAction,
    },
    Edit {
        plan: PlanId,
        tasks: Vec<PlanTaskSpec>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Prompt,
}

pub struct Edit {
    pub field: Field,
    pub text: String,
}

pub struct Panel {
    pub workspace: WorkspaceId,
    pub selected: usize,
    pub scroll: u16,
    pub edit: Option<Edit>,
}

#[derive(Default)]
pub struct Plans {
    pub all: HashMap<PlanId, Plan>,
    pub panel: Option<Panel>,
}

/// Record a plan snapshot, ignoring one older than what is held.
pub fn upsert(app: &mut App, plan: Plan) {
    let newer = app
        .wb
        .plans
        .all
        .get(&plan.id)
        .is_none_or(|old| plan.updated_at >= old.updated_at);
    if newer {
        app.wb.plans.all.insert(plan.id, plan);
    }
}

pub fn observe(app: &mut App, event: &Event) {
    if let EventKind::PlanUpdated(plan) = &event.kind {
        let finished = plan.state.is_terminal();
        let id = plan.id;
        upsert(app, (**plan).clone());
        if finished && app.wb.plans.panel.is_none() {
            let text = match app.wb.plans.all[&id].state {
                PlanState::Done => "Delegation plan finished — /plans to review.",
                PlanState::Failed => "Plan proposal failed — /plans to see why.",
                _ => return,
            };
            app.set_status(text);
        } else if app.wb.plans.all[&id].state == PlanState::Paused && app.wb.plans.panel.is_none() {
            app.set_status("A delegation plan needs you — /plans to continue.");
        }
    }
}

/// The newest plan of the space the panel shows.
fn current(app: &App) -> Option<&Plan> {
    let ws = app.wb.plans.panel.as_ref()?.workspace;
    app.wb
        .plans
        .all
        .values()
        .filter(|p| p.workspace_id == ws)
        .max_by_key(|p| (p.created_at, p.id))
}

fn workspace_of_selected(app: &App) -> Option<WorkspaceId> {
    let id = app.selected_session_id()?;
    app.sessions
        .iter()
        .find(|v| v.session.id == id)
        .map(|v| v.session.workspace_id)
}

/// Sessions of `ws` that can be given tasks.
fn candidates(app: &App, ws: WorkspaceId) -> Vec<SessionId> {
    app.sessions
        .iter()
        .filter(|v| {
            v.session.workspace_id == ws
                && !v.session.native_terminal
                && !matches!(v.session.state, SessionState::Done | SessionState::Error(_))
        })
        .map(|v| v.session.id)
        .collect()
}

fn label(app: &App, session: SessionId) -> String {
    app.sessions
        .iter()
        .find(|v| v.session.id == session)
        .map(|v| format!("{} {}", v.agent_name, &session.to_string()[..4]))
        .unwrap_or_else(|| "unknown".into())
}

pub fn open(app: &mut App) -> AppAction {
    let Some(ws) = workspace_of_selected(app) else {
        app.set_status("Select an agent conversation first.");
        return AppAction::None;
    };
    app.wb.menu = false;
    app.wb.control_focus = None;
    app.wb.plans.panel = Some(Panel {
        workspace: ws,
        selected: 0,
        scroll: 0,
        edit: None,
    });
    AppAction::Plan(PlanCall::List(ws))
}

pub fn close(app: &mut App) {
    app.wb.plans.panel = None;
    app.wb.control_focus = None;
}

/// `/delegate <goal>`: ask the selected session to propose a plan.
pub fn delegate(app: &mut App, goal: &str) -> AppAction {
    let Some(view) = app.sessions.get(app.selected) else {
        app.set_status("Select an agent conversation first.");
        return AppAction::None;
    };
    if view.session.native_terminal {
        app.set_error("A native terminal cannot propose plans; pick a structured agent.");
        return AppAction::None;
    }
    if view.session.state != SessionState::Ready {
        app.set_error("The proposing agent must be idle.");
        return AppAction::None;
    }
    let (workspace, session) = (view.session.workspace_id, view.session.id);
    app.wb.plans.panel = Some(Panel {
        workspace,
        selected: 0,
        scroll: 0,
        edit: None,
    });
    app.wb.control_focus = None;
    AppAction::Plan(PlanCall::Propose {
        workspace,
        session,
        goal: goal.into(),
    })
}

fn specs(plan: &Plan) -> Vec<PlanTaskSpec> {
    plan.tasks
        .iter()
        .map(|t| PlanTaskSpec {
            id: t.id.clone(),
            title: t.title.clone(),
            prompt: t.prompt.clone(),
            assignee: t.assignee,
            depends_on: t.depends_on.clone(),
            scope_paths: t.scope_paths.clone(),
        })
        .collect()
}

/// The next candidate after `current` (wrapping), for cycling an assignee.
fn next_assignee(cands: &[SessionId], current: Option<SessionId>) -> Option<SessionId> {
    if cands.is_empty() {
        return None;
    }
    let at = current.and_then(|c| cands.iter().position(|s| *s == c));
    Some(cands[at.map_or(0, |i| (i + 1) % cands.len())])
}

/// Handle the `/plan-*` commands the panel's buttons issue.
pub fn command(app: &mut App, name: &str) -> AppAction {
    let Some(plan) = current(app).cloned() else {
        app.set_status("No delegation plan yet — use /delegate <goal>.");
        return AppAction::None;
    };
    let selected = app
        .wb
        .plans
        .panel
        .as_ref()
        .map_or(0, |p| p.selected)
        .min(plan.tasks.len().saturating_sub(1));
    let task = plan.tasks.get(selected).cloned();
    let control = |method| {
        AppAction::Plan(PlanCall::Control {
            method,
            plan: plan.id,
        })
    };
    match name {
        "/plan-approve" if plan.state == PlanState::Proposed => {
            control(agentmux_core::rpc::M_PLAN_APPROVE)
        }
        "/plan-cancel" if !plan.state.is_terminal() => control(agentmux_core::rpc::M_PLAN_CANCEL),
        "/plan-resume" if plan.state == PlanState::Paused => {
            control(agentmux_core::rpc::M_PLAN_RESUME)
        }
        "/plan-retry" | "/plan-skip" | "/plan-reassign" | "/plan-delete" | "/plan-title"
        | "/plan-prompt" => {
            let Some(task) = task else {
                return AppAction::None;
            };
            let editable = plan.state == PlanState::Proposed;
            let active = matches!(plan.state, PlanState::Running | PlanState::Paused);
            match name {
                "/plan-retry" if active => AppAction::Plan(PlanCall::Task {
                    plan: plan.id,
                    task: task.id,
                    action: TaskAction::Retry,
                }),
                "/plan-skip" if active => AppAction::Plan(PlanCall::Task {
                    plan: plan.id,
                    task: task.id,
                    action: TaskAction::Skip,
                }),
                "/plan-reassign" if editable || active => {
                    let cands = candidates(app, plan.workspace_id);
                    let Some(next) = next_assignee(&cands, task.assignee) else {
                        app.set_error("No structured agent is available in this space.");
                        return AppAction::None;
                    };
                    if editable {
                        let mut tasks = specs(&plan);
                        tasks[selected].assignee = Some(next);
                        AppAction::Plan(PlanCall::Edit {
                            plan: plan.id,
                            tasks,
                        })
                    } else {
                        AppAction::Plan(PlanCall::Task {
                            plan: plan.id,
                            task: task.id,
                            action: TaskAction::Reassign { session_id: next },
                        })
                    }
                }
                "/plan-delete" if editable => {
                    let mut tasks = specs(&plan);
                    let gone = tasks.remove(selected).id;
                    for t in &mut tasks {
                        t.depends_on.retain(|d| *d != gone);
                    }
                    AppAction::Plan(PlanCall::Edit {
                        plan: plan.id,
                        tasks,
                    })
                }
                "/plan-title" | "/plan-prompt" if editable => {
                    let (field, text) = if name == "/plan-title" {
                        (Field::Title, task.title)
                    } else {
                        (Field::Prompt, task.prompt)
                    };
                    if let Some(panel) = app.wb.plans.panel.as_mut() {
                        panel.edit = Some(Edit { field, text });
                    }
                    AppAction::None
                }
                _ => {
                    app.set_status("That action is not available for this plan right now.");
                    AppAction::None
                }
            }
        }
        _ => {
            app.set_status("That action is not available for this plan right now.");
            AppAction::None
        }
    }
}

pub fn paste(app: &mut App, text: &str) {
    if let Some(edit) = app.wb.plans.panel.as_mut().and_then(|p| p.edit.as_mut()) {
        edit.text.extend(text.chars().filter(|c| !c.is_control()));
    }
}

fn commit_edit(app: &mut App) -> AppAction {
    let Some(plan) = current(app).cloned() else {
        return AppAction::None;
    };
    let Some(panel) = app.wb.plans.panel.as_mut() else {
        return AppAction::None;
    };
    let Some(edit) = panel.edit.take() else {
        return AppAction::None;
    };
    let selected = panel.selected.min(plan.tasks.len().saturating_sub(1));
    let text = edit.text.trim().to_owned();
    if text.is_empty() {
        app.set_error("A task title/prompt must not be empty.");
        return AppAction::None;
    }
    let mut tasks = specs(&plan);
    match edit.field {
        Field::Title => tasks[selected].title = text,
        Field::Prompt => tasks[selected].prompt = text,
    }
    AppAction::Plan(PlanCall::Edit {
        plan: plan.id,
        tasks,
    })
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    let panel = app.wb.plans.panel.as_mut()?;
    if let Some(edit) = panel.edit.as_mut() {
        match key.code {
            KeyCode::Esc => panel.edit = None,
            KeyCode::Enter => return Some(commit_edit(app)),
            KeyCode::Backspace => {
                edit.text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                edit.text.clear()
            }
            _ => {
                if let Some(c) = crate::input::plain_char(&key).filter(|c| !c.is_control()) {
                    edit.text.push(c);
                }
            }
        }
        return Some(AppAction::None);
    }
    let count = current(app).map_or(0, |p| p.tasks.len());
    let panel = app.wb.plans.panel.as_mut()?;
    match key.code {
        KeyCode::Esc => close(app),
        KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter => {
            return crate::interaction::keyboard(app, key).or(Some(AppAction::None))
        }
        KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
        KeyCode::Down => panel.selected = (panel.selected + 1).min(count.saturating_sub(1)),
        KeyCode::PageUp => panel.scroll = panel.scroll.saturating_sub(8),
        KeyCode::PageDown => panel.scroll = panel.scroll.saturating_add(8),
        _ => {
            let name = match crate::input::plain_char(&key) {
                Some('y') => "/plan-approve",
                Some('x') => "/plan-cancel",
                Some('c') => "/plan-resume",
                Some('r') => "/plan-retry",
                Some('s') => "/plan-skip",
                Some('a') => "/plan-reassign",
                Some('d') => "/plan-delete",
                Some('t') => "/plan-title",
                Some('p') => "/plan-prompt",
                _ => return Some(AppAction::None),
            };
            return Some(command(app, name));
        }
    }
    Some(AppAction::None)
}

fn task_mark(state: TaskState) -> (&'static str, ratatui::style::Style) {
    match state {
        TaskState::Done => ("✓", THEME.accent),
        TaskState::Running => ("▶", THEME.accent_bold),
        TaskState::Failed => ("✗", THEME.error),
        TaskState::Skipped => ("–", THEME.faint),
        TaskState::Pending | TaskState::Ready => ("·", THEME.dim),
    }
}

/// Button set for the plan's state; kept as data so tests can inspect it.
pub fn actions(state: PlanState) -> Vec<(&'static str, Target)> {
    let mut out = Vec::new();
    match state {
        PlanState::Proposed => out.extend([
            ("Approve", Target::Command("/plan-approve")),
            ("Assignee", Target::Command("/plan-reassign")),
            ("Title", Target::Command("/plan-title")),
            ("Prompt", Target::Command("/plan-prompt")),
            ("Remove", Target::Command("/plan-delete")),
        ]),
        PlanState::Paused => out.extend([
            ("Resume", Target::Command("/plan-resume")),
            ("Retry", Target::Command("/plan-retry")),
            ("Skip", Target::Command("/plan-skip")),
            ("Assignee", Target::Command("/plan-reassign")),
        ]),
        PlanState::Running => out.extend([
            ("Retry", Target::Command("/plan-retry")),
            ("Skip", Target::Command("/plan-skip")),
            ("Assignee", Target::Command("/plan-reassign")),
        ]),
        _ => {}
    }
    if !state.is_terminal() && state != PlanState::Proposing {
        out.push(("Cancel plan", Target::Command("/plan-cancel")));
    }
    if state == PlanState::Proposing {
        out.push(("Cancel plan", Target::Command("/plan-cancel")));
    }
    out.push(("Close", Target::Close));
    out
}

pub fn draw(frame: &mut Frame, app: &App) {
    let Some(panel) = app.wb.plans.panel.as_ref() else {
        return;
    };
    app.wb.hits.borrow_mut().clear();
    let inner = crate::shell::modal(frame, 96, 28, " Delegation plan ");
    let plan = current(app);
    let button_rows = if inner.width < 70 { 4 } else { 3 };
    let parts =
        Layout::vertical([Constraint::Min(1), Constraint::Length(button_rows)]).split(inner);
    let mut lines: Vec<Line> = Vec::new();
    match plan {
        None => lines.push(Line::styled(
            "No plan yet. Use /delegate <goal> to ask this agent to split a goal.",
            THEME.dim,
        )),
        Some(plan) => {
            lines.push(Line::styled(format!("Goal: {}", plan.goal), THEME.accent));
            let state = match plan.state {
                PlanState::Proposing => "Waiting for the proposal…",
                PlanState::Proposed => "Review, edit, then Approve — nothing runs before you do.",
                PlanState::Running => "Running",
                PlanState::Paused => "Paused — needs your decision",
                PlanState::Done => "Done",
                PlanState::Cancelled => "Cancelled",
                PlanState::Failed => "Proposal failed",
            };
            lines.push(Line::raw(format!("State: {state}")));
            if let Some(e) = &plan.error {
                lines.push(Line::styled(format!("! {e}"), THEME.error));
            }
            for w in &plan.warnings {
                lines.push(Line::styled(format!("⚠ {w}"), THEME.warning));
            }
            if let Some(raw) = &plan.raw_reply {
                lines.push(Line::styled("Agent reply:", THEME.section));
                lines.push(Line::raw(raw.clone()));
            }
            lines.push(Line::default());
            let selected = panel.selected.min(plan.tasks.len().saturating_sub(1));
            for (i, t) in plan.tasks.iter().enumerate() {
                let (mark, style) = task_mark(t.state);
                let who = t
                    .assignee
                    .map(|s| label(app, s))
                    .unwrap_or_else(|| "unassigned".into());
                let after = if t.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" · after {}", t.depends_on.join(","))
                };
                let row = Line::from(vec![
                    Span::styled(if i == selected { "› " } else { "  " }, THEME.accent),
                    Span::styled(format!("{mark} "), style),
                    Span::raw(format!("{} {} — {who}{after}", t.id, t.title)),
                ]);
                lines.push(if i == selected {
                    row.style(THEME.selection)
                } else {
                    row
                });
            }
            if let Some(t) = plan.tasks.get(selected) {
                lines.push(Line::default());
                lines.push(Line::styled(format!("Task {}", t.id), THEME.section));
                lines.push(Line::raw(format!("Prompt: {}", t.prompt)));
                if !t.scope_paths.is_empty() {
                    lines.push(Line::raw(format!("Scope: {}", t.scope_paths.join(", "))));
                }
                if let Some(r) = &t.result {
                    lines.push(Line::raw(format!("Result: {r}")));
                }
                if let Some(e) = &t.error {
                    lines.push(Line::styled(format!("Error: {e}"), THEME.error));
                }
                if !t.files_changed.is_empty() {
                    lines.push(Line::raw(format!("Files: {}", t.files_changed.join(", "))));
                }
            }
            if let Some(s) = &plan.summary {
                lines.push(Line::default());
                lines.push(Line::styled("Summary", THEME.section));
                lines.extend(s.lines().map(|l| Line::raw(l.to_owned())));
            }
        }
    }
    if let Some(edit) = &panel.edit {
        let what = match edit.field {
            Field::Title => "title",
            Field::Prompt => "prompt",
        };
        lines.push(Line::default());
        lines.push(Line::styled(
            format!("Editing {what} (Enter saves · Esc cancels · Ctrl+U clears):"),
            THEME.section,
        ));
        lines.push(Line::raw(format!("{}▏", edit.text)));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max = paragraph
        .line_count(parts[0].width)
        .saturating_sub(usize::from(parts[0].height))
        .min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.scroll((panel.scroll.min(max), 0)), parts[0]);
    let choices = actions(plan.map_or(PlanState::Done, |p| p.state));
    buttons(frame, app, parts[1], &choices);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignee_cycles_through_candidates_and_wraps() {
        let (a, b) = (SessionId::new(), SessionId::new());
        assert_eq!(next_assignee(&[a, b], None), Some(a));
        assert_eq!(next_assignee(&[a, b], Some(a)), Some(b));
        assert_eq!(next_assignee(&[a, b], Some(b)), Some(a));
        assert_eq!(next_assignee(&[a, b], Some(SessionId::new())), Some(a));
        assert_eq!(next_assignee(&[], Some(a)), None);
    }

    fn app_with_plan(state: PlanState) -> (App, Plan) {
        let mut app = App::new(vec![], vec![], vec![], vec![]);
        let ws = WorkspaceId::new();
        let mut plan = Plan::new(ws, SessionId::new(), "goal".into());
        plan.state = state;
        let t1 = agentmux_core::plan::PlanTask::new("t1", "first", "p1");
        let mut t2 = agentmux_core::plan::PlanTask::new("t2", "second", "p2");
        t2.depends_on = vec!["t1".into()];
        plan.tasks = vec![t1, t2];
        upsert(&mut app, plan.clone());
        app.wb.plans.panel = Some(Panel {
            workspace: ws,
            selected: 0,
            scroll: 0,
            edit: None,
        });
        (app, plan)
    }

    #[test]
    fn approve_is_offered_only_while_reviewing() {
        let (mut app, plan) = app_with_plan(PlanState::Proposed);
        assert_eq!(
            command(&mut app, "/plan-approve"),
            AppAction::Plan(PlanCall::Control {
                method: agentmux_core::rpc::M_PLAN_APPROVE,
                plan: plan.id
            })
        );
        let (mut app, _) = app_with_plan(PlanState::Running);
        assert_eq!(command(&mut app, "/plan-approve"), AppAction::None);
    }

    #[test]
    fn removing_a_task_drops_dependencies_on_it() {
        let (mut app, plan) = app_with_plan(PlanState::Proposed);
        let AppAction::Plan(PlanCall::Edit { plan: id, tasks }) = command(&mut app, "/plan-delete")
        else {
            panic!("expected an edit");
        };
        assert_eq!(id, plan.id);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t2");
        assert!(
            tasks[0].depends_on.is_empty(),
            "dangling dependency left behind"
        );
    }

    #[test]
    fn title_edit_round_trips_through_the_keyboard() {
        let (mut app, plan) = app_with_plan(PlanState::Proposed);
        assert_eq!(command(&mut app, "/plan-title"), AppAction::None);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        keyboard(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        for c in "new".chars() {
            keyboard(&mut app, key(c));
        }
        let Some(AppAction::Plan(PlanCall::Edit { plan: id, tasks })) =
            keyboard(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("Enter should submit the edit");
        };
        assert_eq!(id, plan.id);
        assert_eq!(tasks[0].title, "new");
        assert_eq!(tasks[0].prompt, "p1", "other fields untouched");
        // An empty title is refused locally.
        command(&mut app, "/plan-title");
        keyboard(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert_eq!(
            keyboard(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(AppAction::None)
        );
    }

    #[test]
    fn retry_and_skip_apply_only_to_active_plans_and_target_the_selection() {
        let (mut app, plan) = app_with_plan(PlanState::Paused);
        app.wb.plans.panel.as_mut().unwrap().selected = 1;
        assert_eq!(
            command(&mut app, "/plan-skip"),
            AppAction::Plan(PlanCall::Task {
                plan: plan.id,
                task: "t2".into(),
                action: TaskAction::Skip
            })
        );
        let (mut app, _) = app_with_plan(PlanState::Proposed);
        assert_eq!(command(&mut app, "/plan-retry"), AppAction::None);
    }

    #[test]
    fn newer_snapshots_win_and_stale_ones_are_ignored() {
        let (mut app, plan) = app_with_plan(PlanState::Proposed);
        let mut newer = plan.clone();
        newer.state = PlanState::Running;
        newer.updated_at = plan.updated_at + chrono::Duration::seconds(1);
        upsert(&mut app, newer);
        upsert(&mut app, plan.clone());
        assert_eq!(app.wb.plans.all[&plan.id].state, PlanState::Running);
    }

    #[test]
    fn every_state_offers_close_and_only_review_offers_approve() {
        for state in [
            PlanState::Proposing,
            PlanState::Proposed,
            PlanState::Running,
            PlanState::Paused,
            PlanState::Done,
            PlanState::Cancelled,
            PlanState::Failed,
        ] {
            let a = actions(state);
            assert!(a.iter().any(|(_, t)| *t == Target::Close));
            assert_eq!(
                a.iter()
                    .any(|(_, t)| *t == Target::Command("/plan-approve")),
                state == PlanState::Proposed
            );
        }
        assert!(actions(PlanState::Paused)
            .iter()
            .any(|(_, t)| *t == Target::Command("/plan-resume")));
    }
}
