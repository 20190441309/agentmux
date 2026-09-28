//! The `n` new-session wizard: a three-step picker inside
//! [`InputMode::NewSession`] — project → workspace → agent.
//!
//! Pure UI state held on [`App::wizard`]; [`wizard_key`] translates keys
//! into step moves, and completing the last step emits
//! [`AppAction::CreateSession`] for `main.rs` to turn into
//! `workspace/create` (only for a fresh workspace) + `session/create`.
//!
//! ```text
//!  ┌ new session — pick workspace ────────┐
//!  │ ›  ws1                               │
//!  │    + create new workspace…           │  ← name input follows
//!  └──────────────────────────────────────┘
//! ```

use agentmux_core::{AgentId, AgentProfile, ProjectId, Workspace, WorkspaceId};
use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{App, AppAction, InputMode};

/// Which wizard step is collecting input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WizardStep {
    /// Step 1: pick a project.
    #[default]
    Project,
    /// Step 2: pick one of the project's workspaces, or the
    /// "+ create new workspace" sentinel.
    Workspace,
    /// Step 2b: inline name for the workspace being created.
    WorkspaceName,
    /// Step 3: pick an available agent.
    Agent,
}

/// What the wizard picked for the session's workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspacePick {
    /// Reuse an existing workspace.
    Existing { id: WorkspaceId, name: String },
    /// `workspace/create` this project first, then create the session
    /// in the fresh workspace.
    New { project_id: ProjectId, name: String },
}

/// Wizard state — `Some` iff `app.mode == InputMode::NewSession`.
#[derive(Debug, Clone, Default)]
pub struct NewSessionWizard {
    pub step: WizardStep,
    /// Cursor over `app.projects` (step `Project`).
    pub project_cursor: usize,
    /// Cursor over the picked project's workspaces plus the
    /// "+ create new workspace" sentinel at the end (step `Workspace`).
    pub workspace_cursor: usize,
    /// Cursor over the *available* agents (step `Agent`).
    pub agent_cursor: usize,
    /// The project picked at step `Project`.
    pub project_id: Option<ProjectId>,
    /// Inline name being typed for [`WorkspacePick::New`].
    pub name: String,
    /// The workspace picked when leaving the workspace steps.
    pub workspace: Option<WorkspacePick>,
}

impl NewSessionWizard {
    /// Workspaces of the picked project, in `app.workspaces` order.
    pub fn workspace_options<'a>(&'a self, app: &'a App) -> impl Iterator<Item = &'a Workspace> {
        let project_id = self.project_id;
        app.workspaces
            .iter()
            .filter(move |w| Some(w.project_id) == project_id)
    }

    /// Number of `Workspace`-step rows: the project's workspaces plus the
    /// "+ create new workspace" sentinel.
    fn workspace_row_count(&self, app: &App) -> usize {
        self.workspace_options(app).count() + 1
    }

    /// Pickable agents — available only; the wizard never offers a
    /// profile the daemon would refuse.
    pub fn agent_options<'a>(&'a self, app: &'a App) -> impl Iterator<Item = &'a AgentProfile> {
        app.agents.iter().filter(|a| a.available)
    }
}

/// Keystroke in [`InputMode::NewSession`]. `j`/`k`/`↓`/`↑` move within
/// the current step's list, `Enter` confirms and advances, `Esc` steps
/// back (`Project` aborts), `q` aborts from list steps. In
/// `WorkspaceName` all plain chars type into the name buffer.
pub(crate) fn wizard_key(app: &mut App, key: KeyEvent) -> AppAction {
    // `take` + explicit put-back: handlers own the wizard while editing
    // it, so a stale `&mut` can never observe a half-advanced step.
    let Some(mut wiz) = app.wizard.take() else {
        // Mode without state — recover to Normal rather than wedging.
        app.mode = InputMode::Normal;
        return AppAction::None;
    };
    match wiz.step {
        WizardStep::Project => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => abort(app),
            KeyCode::Enter => {
                match app.projects.get(wiz.project_cursor) {
                    Some(p) => {
                        wiz.project_id = Some(p.id);
                        wiz.step = WizardStep::Workspace;
                        wiz.workspace_cursor = 0;
                    }
                    None => app.set_status("no project selected"),
                }
                app.wizard = Some(wiz);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                wiz.project_cursor = clamped(wiz.project_cursor + 1, app.projects.len());
                app.wizard = Some(wiz);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                wiz.project_cursor = wiz.project_cursor.saturating_sub(1);
                app.wizard = Some(wiz);
            }
            _ => app.wizard = Some(wiz),
        },
        WizardStep::Workspace => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                wiz.step = WizardStep::Project;
                app.wizard = Some(wiz);
            }
            KeyCode::Enter => {
                // Materialise the pick first — `workspace_options`
                // borrows `wiz`+`app`, the mutation needs them mutable.
                let picked = wiz
                    .workspace_options(app)
                    .nth(wiz.workspace_cursor)
                    .map(|ws| (ws.id, ws.name.clone()));
                if let Some((id, name)) = picked {
                    wiz.workspace = Some(WorkspacePick::Existing { id, name });
                    wiz.step = WizardStep::Agent;
                    wiz.agent_cursor = 0;
                } else {
                    // Past the last workspace row: the create-new sentinel.
                    wiz.step = WizardStep::WorkspaceName;
                }
                app.wizard = Some(wiz);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                wiz.workspace_cursor =
                    clamped(wiz.workspace_cursor + 1, wiz.workspace_row_count(app));
                app.wizard = Some(wiz);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                wiz.workspace_cursor = wiz.workspace_cursor.saturating_sub(1);
                app.wizard = Some(wiz);
            }
            _ => app.wizard = Some(wiz),
        },
        WizardStep::WorkspaceName => match key.code {
            KeyCode::Esc => {
                wiz.step = WizardStep::Workspace;
                app.wizard = Some(wiz);
            }
            KeyCode::Enter => {
                let name = wiz.name.trim().to_string();
                if name.is_empty() {
                    app.set_status("workspace name required");
                    app.wizard = Some(wiz);
                } else if let Some(project_id) = wiz.project_id {
                    wiz.workspace = Some(WorkspacePick::New { project_id, name });
                    wiz.step = WizardStep::Agent;
                    wiz.agent_cursor = 0;
                    app.wizard = Some(wiz);
                } else {
                    // Unreachable: WorkspaceName follows a project pick.
                    abort(app);
                }
            }
            KeyCode::Backspace => {
                wiz.name.pop();
                app.wizard = Some(wiz);
            }
            _ => {
                if let Some(c) = crate::input::plain_char(&key) {
                    wiz.name.push(c);
                }
                app.wizard = Some(wiz);
            }
        },
        WizardStep::Agent => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                wiz.step = WizardStep::Workspace;
                app.wizard = Some(wiz);
            }
            KeyCode::Enter => {
                let agent = wiz.agent_options(app).nth(wiz.agent_cursor);
                match (wiz.workspace.clone(), agent) {
                    (Some(workspace), Some(agent)) => {
                        let agent_id: AgentId = agent.id.clone();
                        app.wizard = None;
                        app.mode = InputMode::Normal;
                        return AppAction::CreateSession {
                            workspace,
                            agent_id,
                        };
                    }
                    (workspace, None) => {
                        // Agents vanished mid-wizard — stay put with a hint.
                        wiz.workspace = workspace;
                        app.set_status("no available agent");
                        app.wizard = Some(wiz);
                    }
                    // Unreachable: Agent follows a workspace pick.
                    (None, _) => abort(app),
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                wiz.agent_cursor = clamped(wiz.agent_cursor + 1, wiz.agent_options(app).count());
                app.wizard = Some(wiz);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                wiz.agent_cursor = wiz.agent_cursor.saturating_sub(1);
                app.wizard = Some(wiz);
            }
            _ => app.wizard = Some(wiz),
        },
    }
    AppAction::None
}

/// `index + 1` clamped to `count - 1` (no-op when the list is empty).
fn clamped(next: usize, count: usize) -> usize {
    if count == 0 {
        0
    } else {
        next.min(count - 1)
    }
}

/// Drop the wizard and return to Normal mode.
fn abort(app: &mut App) {
    app.wizard = None;
    app.mode = InputMode::Normal;
}
