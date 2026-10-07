//! Hit regions are rebuilt from the rendered frame, including list offsets.
use crate::{
    app::{App, AppAction, InputMode, RelayStage},
    newsession::WizardStep,
    theme::THEME,
    workbench::SideTab,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Position, Rect},
    widgets::Paragraph,
    Frame,
};

use unicode_width::UnicodeWidthStr;

#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    ContextBody,
    ContextView(crate::context::View),
    Reference(usize),
    Tool(agentmux_core::SessionId, String),
    Message(agentmux_core::SessionId, u64),
    Transcript(&'static str),
    AgentFilter(crate::workbench::AgentFilter),
    PickerSpace,
    WorkspaceGroup(agentmux_core::WorkspaceId),
    FileSource(crate::files::Source),
    WorkspaceFile(agentmux_core::WorkspaceId, Vec<u8>),
    DiffScope(agentmux_core::rpc::DiffScope),
    Hunk(bool),
    AttentionItem(crate::attention::Key),
    AttentionAction(&'static str),
    NameInput,
    Reasoning(agentmux_core::SessionId, u64),
    AgentChoice(usize),
    Command(&'static str),
    Menu,
    MenuStep(bool),
    Close,
    Key(KeyCode),
    Session(agentmux_core::SessionId),
    File(String),
    Tab(SideTab),
    Wizard(usize),
    Relay(usize),
    Editor { area: Rect, offset: usize },
    Conversation,
    Blocked,
}
#[derive(Clone)]
pub struct Hit {
    pub area: Rect,
    pub target: Target,
}

pub fn hit(app: &App, area: Rect, target: Target) {
    if area.width > 0 && area.height > 0 {
        app.wb.hits.borrow_mut().push(Hit { area, target });
    }
}
pub fn buttons(frame: &mut Frame, app: &App, area: Rect, choices: &[(&str, Target)]) {
    let mut x = area.x;
    let mut y = area.y;
    for (label, target) in choices {
        let text = format!(" {label} ");
        let width = UnicodeWidthStr::width(text.as_str()) as u16;
        if x + width > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.bottom() || width > area.width {
            break;
        }
        let rect = Rect::new(x, y, width, 1);
        let focused = app.wb.control_focus.as_ref() == Some(target);
        let active = matches!(target, Target::ContextView(view) if *view == app.wb.context.view)
            || matches!(target, Target::Tab(tab) if *tab == app.wb.tab)
            || matches!(target, Target::FileSource(source) if *source == app.wb.files.source)
            || matches!(target, Target::DiffScope(scope) if *scope == app.wb.files.diff_scope)
            || matches!(target, Target::AgentFilter(filter) if *filter == app.wb.picker_filter)
            || matches!(target, Target::PickerSpace if app.wb.picker_current_space);
        // View toggles that are on read brighter, without a tab's accent.
        let toggled = matches!(target, Target::Command("/thinking") if app.wb.thoughts)
            || matches!(target, Target::Command("/tools") if app.wb.tools_expanded);
        frame.render_widget(
            Paragraph::new(text).style(if focused {
                THEME.selection
            } else if *target == Target::Command("/attention") {
                THEME.warning_bold
            } else if matches!(*label, "Send" | "Resume & send" | "Queue" | "Allow once") {
                THEME.primary
            } else if toggled {
                THEME.text
            } else if active {
                THEME
                    .accent_bold
                    .add_modifier(ratatui::style::Modifier::UNDERLINED)
            } else {
                THEME.control
            }),
            rect,
        );
        hit(app, rect, target.clone());
        x += width + 1;
    }
}
pub fn activate(app: &mut App, target: Target) -> AppAction {
    app.wb.control_focus = None;
    match target {
        Target::ContextBody => {}
        Target::ContextView(view) => {
            app.wb.context.view = view;
            app.wb.context_scroll = 0;
        }
        Target::Reference(index) => {
            if let Some(panel) = &mut app.wb.references.panel {
                panel.index = index;
                panel.scroll = 0;
            }
        }
        Target::Tool(session, id) => {
            let key = (session, id);
            if !app.wb.transcript.tool_toggles.insert(key.clone()) {
                app.wb.transcript.tool_toggles.remove(&key);
            }
        }
        Target::Message(session, seq) => crate::transcript::choose(app, session, seq),
        Target::Transcript(action) => return crate::transcript::action(app, action),
        Target::AgentFilter(filter) => app.set_picker_filter(filter),
        Target::PickerSpace => {
            app.wb.picker_current_space = !app.wb.picker_current_space;
            app.wb.picker_cursor = 0;
        }
        Target::WorkspaceGroup(id) => {
            if !app.wb.collapsed_spaces.insert(id) {
                app.wb.collapsed_spaces.remove(&id);
            }
        }
        Target::FileSource(source) => crate::files::select_source(app, source),
        Target::WorkspaceFile(workspace, key) => {
            if app.file_workspace() == Some(workspace) {
                if let Some(index) = app
                    .changed_files()
                    .iter()
                    .position(|file| file.key() == key)
                {
                    app.wb.file_cursor = index;
                    return app.inspect_selected_file();
                }
            }
        }
        Target::DiffScope(scope) => return crate::files::scope(app, scope),
        Target::Hunk(next) => crate::files::hunk(app, next),
        Target::AttentionItem(key) => return crate::attention::activate(app, key),
        Target::AttentionAction(command) => return crate::attention::action(app, command),
        Target::NameInput => {}
        Target::Reasoning(id, seq) => {
            if !app.wb.reasoning_toggles.insert((id, seq)) {
                app.wb.reasoning_toggles.remove(&(id, seq));
            }
            if let Some(ui) = app.wb.sessions.get_mut(&id) {
                ui.scroll_limit.set(None);
            }
        }
        Target::AgentChoice(index) => return crate::commands::choose(app, index),
        Target::Command(command) => {
            app.wb.menu = false;
            return app.command(command);
        }
        Target::MenuStep(up) => {
            app.wb.menu_cursor = if up {
                app.wb.menu_cursor.saturating_sub(1)
            } else {
                (app.wb.menu_cursor + 1).min(crate::shell::menu_actions().len().saturating_sub(1))
            };
        }
        Target::Menu => {
            app.wb.menu = !app.wb.menu;
        }
        Target::Close => {
            if app.wb.context.panel.is_some() {
                crate::context::close(app);
                return AppAction::None;
            }
            if app.wb.info.panel.is_some() {
                crate::agent_info::close(app);
            } else if app.wb.references.panel.is_some() {
                crate::references::close(app);
            } else if app.wb.transcript.panel.is_some() {
                crate::transcript::close(app);
            } else if app.wb.attention.panel.is_some() {
                crate::attention::close(app);
            } else if app.wb.pi_panel.is_some() {
                app.wb.pi_panel = None;
            } else if !crate::commands::suggestions(app).is_empty() {
                app.wb.slash_dismissed = Some(app.input.clone());
            } else if app.wb.menu {
                app.wb.menu = false;
            } else {
                return app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
        }
        Target::Key(code) => {
            if matches!(app.mode, InputMode::Normal | InputMode::Sidebar) {
                app.mode = InputMode::Editing;
            }
            return app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        Target::Tab(tab) => {
            app.wb.tab = tab;
            app.mode = InputMode::Sidebar;
        }
        Target::Session(id) => {
            if let Some(i) = app.sessions.iter().position(|s| s.session.id == id) {
                if let Some(pick) = app
                    .relay
                    .as_mut()
                    .filter(|p| app.mode == InputMode::RelayPick && p.stage == RelayStage::Session)
                {
                    pick.session_cursor = i;
                    return crate::input::relay_pick_key(
                        app,
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    );
                }
                app.select_session(i);
                app.mode = InputMode::Editing;
                app.wb.drawer = false;
            }
        }
        Target::File(path) => {
            if let Some(i) = app.activity_files().iter().position(|f| f == &path) {
                app.wb.file_cursor = i;
            }
            return AppAction::InspectFile(path);
        }
        Target::Wizard(i) => {
            if let Some(wiz) = app.wizard.as_mut() {
                match wiz.step {
                    WizardStep::Project => wiz.project_cursor = i,
                    WizardStep::Workspace => wiz.workspace_cursor = i,
                    WizardStep::Agent => wiz.agent_cursor = i,
                    _ => return AppAction::None,
                }
                return crate::newsession::wizard_key(
                    app,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                );
            }
        }
        Target::Relay(i) => {
            if let Some(pick) = app.relay.as_mut() {
                pick.event_cursor = i;
            }
            return crate::input::relay_pick_key(
                app,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
        }
        Target::Editor { .. } => {
            app.mode = InputMode::Editing;
            app.relay = None;
            app.wb.drawer = false;
        }
        Target::Conversation => crate::focus::select(app, crate::focus::Pane::Reading),
        Target::Blocked => {}
    }
    AppAction::None
}

/// Keyboard focus obeys the same overlay occlusion as mouse hit testing.
pub fn focusable(app: &App) -> Vec<Hit> {
    let hits = app.wb.hits.borrow();
    hits.iter()
        .enumerate()
        .filter(|(i, h)| {
            !matches!(
                h.target,
                Target::Conversation | Target::ContextBody | Target::Blocked
            ) && !hits[i + 1..].iter().any(|later| {
                later.target == Target::Blocked && !h.area.intersection(later.area).is_empty()
            })
        })
        .map(|(_, h)| h.clone())
        .collect()
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        let targets: Vec<_> = focusable(app).into_iter().map(|h| h.target).collect();
        if !targets.is_empty() {
            let current = app
                .wb
                .control_focus
                .as_ref()
                .and_then(|t| targets.iter().position(|v| v == t));
            let reverse =
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
            let i = match current {
                Some(i) if reverse => (i + targets.len() - 1) % targets.len(),
                Some(i) => (i + 1) % targets.len(),
                None if reverse => targets.len() - 1,
                None => 0,
            };
            app.wb.control_focus = Some(targets[i].clone());
        }
        return Some(AppAction::None);
    }
    if key.code == KeyCode::Enter {
        if let Some(target) = app.wb.control_focus.take() {
            return Some(if focusable(app).iter().any(|h| h.target == target) {
                activate(app, target)
            } else {
                AppAction::None
            });
        }
    }
    if app.wb.menu {
        if matches!(
            key.code,
            KeyCode::Home | KeyCode::End | KeyCode::PageUp | KeyCode::PageDown
        ) {
            let last = crate::shell::menu_actions().len().saturating_sub(1);
            app.wb.menu_cursor = match key.code {
                KeyCode::Home => 0,
                KeyCode::End => last,
                KeyCode::PageUp => app.wb.menu_cursor.saturating_sub(8),
                _ => (app.wb.menu_cursor + 8).min(last),
            };
            app.wb.control_focus = None;
            return Some(AppAction::None);
        }
        if matches!(key.code, KeyCode::Up | KeyCode::Down) {
            return Some(activate(app, Target::MenuStep(key.code == KeyCode::Up)));
        }
        if key.code == KeyCode::Enter {
            return Some(activate(
                app,
                crate::shell::menu_actions()[app.wb.menu_cursor].1.clone(),
            ));
        }
        if key.code == KeyCode::Esc {
            app.wb.menu = false;
            app.wb.control_focus = None;
        }
        return Some(AppAction::None);
    }
    app.wb.control_focus = None;
    None
}

pub fn mouse(app: &mut App, event: MouseEvent) -> AppAction {
    let target = app
        .wb
        .hits
        .borrow()
        .iter()
        .rev()
        .find(|h| h.area.contains(Position::new(event.column, event.row)))
        .map(|h| h.target.clone());
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.wb.interaction_epoch += 1;
            if app.wb.inspection.is_none() {
                app.wb.pending_diff = None;
            }
            if let Some(target) = target {
                if let Target::Editor { area, offset } = target {
                    app.wb.cursor = cursor_at(
                        &app.input,
                        area.width,
                        offset + usize::from(event.row - area.y),
                        usize::from(event.column - area.x),
                    );
                }
                return activate(app, target);
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            app.wb.interaction_epoch += 1;
            let up = event.kind == MouseEventKind::ScrollUp;
            if matches!(target, Some(Target::ContextBody)) {
                app.wb.context_scroll = if up {
                    app.wb.context_scroll.saturating_sub(3)
                } else {
                    app.wb.context_scroll.saturating_add(3)
                };
                return AppAction::None;
            }
            if app.wb.transcript.panel.is_some() {
                crate::transcript::wheel(app, up);
                return AppAction::None;
            }
            if app.wb.attention.panel.is_some() {
                crate::attention::move_cursor(app, up, 3);
                return AppAction::None;
            }
            if app.wb.pi_panel.is_some() || matches!(target, Some(Target::AgentChoice(_))) {
                return crate::commands::keyboard(
                    app,
                    KeyEvent::new(
                        if up { KeyCode::Up } else { KeyCode::Down },
                        KeyModifiers::NONE,
                    ),
                )
                .unwrap_or(AppAction::None);
            }
            if app.wb.menu {
                return activate(app, Target::MenuStep(up));
            }
            if app.mode == InputMode::Permission {
                return crate::input::permission_key(
                    app,
                    KeyEvent::new(
                        if up {
                            KeyCode::PageUp
                        } else {
                            KeyCode::PageDown
                        },
                        KeyModifiers::NONE,
                    ),
                );
            }
            if matches!(
                target,
                Some(
                    Target::Session(_)
                        | Target::File(_)
                        | Target::WorkspaceFile(..)
                        | Target::Wizard(_)
                        | Target::Relay(_)
                )
            ) {
                let key = KeyEvent::new(
                    if up { KeyCode::Up } else { KeyCode::Down },
                    KeyModifiers::NONE,
                );
                return match app.mode {
                    InputMode::NewSession | InputMode::TaskPicker | InputMode::RelayPick => {
                        app.handle_key(key)
                    }
                    _ => {
                        app.mode = InputMode::Sidebar;
                        crate::input::sidebar_key(app, key)
                    }
                };
            }
            if matches!(target, Some(Target::Conversation | Target::Reasoning(..))) {
                app.scroll_by(up, 3);
                if up && app.needs_older_history() {
                    return AppAction::LoadHistory;
                }
            }
        }
        _ => {}
    }
    AppAction::None
}

fn cursor_at(text: &str, width: u16, row: usize, col: usize) -> usize {
    crate::editor::EditorLayout::new(text, width).cursor_at(row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{PermissionNotice, SessionView};
    use agentmux_core::*;
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};

    fn fixture(count: usize) -> App {
        let project = Project {
            id: ProjectId::new(),
            name: "sample".into(),
            root_path: "/repo".into(),
        };
        let ws = Workspace {
            id: WorkspaceId::new(),
            project_id: project.id,
            name: "main".into(),
            branch: "main".into(),
            managed_worktree: true,
            worktree_path: "/repo".into(),
            created_at: Utc::now(),
        };
        let sessions = (0..count)
            .map(|i| SessionView {
                session: Session {
                    id: SessionId::new(),
                    workspace_id: ws.id,
                    agent_id: AgentId::new("mock"),
                    state: SessionState::Ready,
                    acp_session_id: None,
                    native_session_file: None,
                    native_terminal: false,
                    references: vec![],
                    created_at: Utc::now(),
                },
                agent_name: format!("agent-{i}"),
                workspace_name: "main".into(),
            })
            .collect();
        App::new(
            vec![project],
            vec![ws],
            sessions,
            vec![AgentProfile {
                id: AgentId::new("mock"),
                name: "Mock".into(),
                adapter: AdapterKind::Acp {
                    command: "/bin/mock".into(),
                    args: vec![],
                },
                env: Default::default(),
                available: true,
            }],
        )
    }
    fn draw(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| crate::shell::draw(f, app)).unwrap();
        term
    }
    fn click(app: &mut App, target: Target) -> AppAction {
        let area = app
            .wb
            .hits
            .borrow()
            .iter()
            .find(|h| h.target == target)
            .expect("visible control")
            .area;
        mouse(
            app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        )
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn close_panel_restores_typing_and_can_reopen_at_all_widths() {
        for (width, height) in [(120, 30), (80, 24), (40, 16)] {
            let mut app = fixture(1);
            app.wb.columns = width;
            if width < 110 {
                app.toggle_sidebar();
            }
            app.insert_text("draft");
            draw(&app, width, height);
            click(&mut app, Target::Command("/hide-sidebar"));
            assert!(!app.sidebar_visible());
            assert_eq!(app.mode, InputMode::Editing);
            app.handle_key(key(KeyCode::Char('!')));
            assert_eq!(app.input, "draft!");
            // Closing twice must never toggle the panel back open.
            app.command("/hide-sidebar");
            assert!(!app.sidebar_visible());
            draw(&app, width, height);
            click(&mut app, Target::Menu);
            draw(&app, width, height);
            click(&mut app, Target::Command("/sidebar"));
            assert!(app.sidebar_visible());
            draw(&app, width, height);
            click(&mut app, Target::Command("/hide-sidebar"));
            assert_eq!(app.input, "draft!");
        }
    }

    #[test]
    fn close_panel_cancels_relay_selection() {
        let mut app = fixture(2);
        app.wb.columns = 80;
        app.relay = Some(crate::app::RelayPick {
            stage: RelayStage::Session,
            session_cursor: 0,
            event_cursor: 0,
            source: None,
        });
        app.mode = InputMode::RelayPick;
        draw(&app, 80, 24);
        click(&mut app, Target::Command("/hide-sidebar"));
        assert!(app.relay.is_none());
        assert!(!app.sidebar_visible());
        assert_eq!(app.mode, InputMode::Editing);
    }

    #[test]
    fn composer_offers_resume_only_for_terminated_sessions() {
        for state in [
            SessionState::Ready,
            SessionState::Done,
            SessionState::Error("failed".into()),
        ] {
            let mut app = fixture(1);
            let expected = state != SessionState::Ready;
            app.sessions[0].session.state = state;
            draw(&app, 120, 30);
            assert_eq!(
                app.wb
                    .hits
                    .borrow()
                    .iter()
                    .any(|h| h.target == Target::Command("/resume")),
                expected
            );
        }
    }

    #[test]
    fn resume_and_send_is_visible_and_clickable_at_all_widths() {
        for width in [40, 80, 120] {
            let mut app = fixture(1);
            app.wb.columns = width;
            app.sessions[0].session.state = SessionState::Error("daemon restarted".into());
            app.insert_text("你是谁");
            let terminal = draw(&app, width, 24);
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Resume & send"));
            assert!(!text.contains("restart required"));
            assert!(
                matches!(click(&mut app, Target::Key(KeyCode::Enter)), AppAction::Submit { text, .. } if text == "你是谁")
            );
        }
    }

    #[test]
    fn mouse_switch_preserves_drafts_and_send_targets_selected_task() {
        let mut app = fixture(2);
        let first = app.sessions[0].session.id;
        let second = app.sessions[1].session.id;
        app.insert_text("first draft");
        draw(&app, 120, 30);
        click(&mut app, Target::Session(second));
        app.insert_text("second draft");
        draw(&app, 120, 30);
        click(&mut app, Target::Session(first));
        assert_eq!(app.input, "first draft");
        draw(&app, 120, 30);
        assert!(
            matches!(click(&mut app,Target::Key(KeyCode::Enter)),AppAction::Submit { text, .. } if text=="first draft")
        );
        assert_eq!(app.selected_session_id(), Some(first));
        assert_eq!(app.wb.sessions[&second].draft, "second draft");
    }

    #[test]
    fn scrolled_grouped_rows_and_picker_use_actual_rendered_offsets() {
        let mut app = fixture(30);
        app.select_session(29);
        let term = draw(&app, 120, 15);
        let hits = app.wb.hits.borrow().clone();
        let sessions: Vec<_> = hits
            .iter()
            .filter(|h| matches!(h.target, Target::Session(_)))
            .collect();
        assert!(!sessions.is_empty());
        assert!(sessions.len() < 30);
        for h in sessions {
            let Target::Session(id) = h.target else {
                unreachable!()
            };
            let line: String = (h.area.x..h.area.right())
                .map(|x| term.backend().buffer()[(x, h.area.y)].symbol())
                .collect();
            assert!(line.contains(&app.session_title(id)), "{line}");
            mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: h.area.x,
                    row: h.area.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
            );
            assert_eq!(app.selected_session_id(), Some(id));
        }
        app.open_picker();
        app.wb.picker_cursor = 29;
        let term = draw(&app, 80, 20);
        let hits = app.wb.hits.borrow().clone();
        for h in hits
            .iter()
            .filter(|h| matches!(h.target, Target::Session(_)))
        {
            let Target::Session(id) = h.target else {
                unreachable!()
            };
            let line: String = (h.area.x..h.area.right())
                .map(|x| term.backend().buffer()[(x, h.area.y)].symbol())
                .collect();
            assert!(line.contains(&app.session_title(id)), "{line}");
        }
    }

    #[test]
    fn mouse_wizard_creates_task_without_commands() {
        let mut app = fixture(0);
        draw(&app, 80, 24);
        click(&mut app, Target::Command("/new"));
        draw(&app, 80, 24);
        click(&mut app, Target::Wizard(0));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Workspace);
        draw(&app, 80, 24);
        click(&mut app, Target::Wizard(0));
        draw(&app, 80, 24);
        assert!(matches!(
            click(&mut app, Target::Wizard(0)),
            AppAction::CreateSession { .. }
        ));
        assert_eq!(app.mode, InputMode::NewSession);
        assert!(app.wizard.as_ref().unwrap().submitting);
    }

    #[test]
    fn add_agent_is_clickable_at_every_width_and_submission_blocks_background() {
        for (width, height) in [(40, 16), (80, 24), (120, 30), (160, 40)] {
            let mut app = fixture(1);
            app.wb.columns = width;
            app.insert_text("original draft");
            let workspace_id = app.sessions[0].session.workspace_id;
            draw(&app, width, height);
            click(&mut app, Target::Command("/add-agent"));
            let term = draw(&app, width, height);
            let text: String = term
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("Space: main"), "{width}: {text}");
            assert!(app
                .wb
                .hits
                .borrow()
                .iter()
                .all(|h| !matches!(h.target, Target::Session(_))));
            assert!(
                matches!(click(&mut app, Target::Wizard(0)), AppAction::CreateSession { workspace: crate::newsession::WorkspacePick::Existing { id, .. }, .. } if id == workspace_id)
            );
            draw(&app, width, height);
            assert!(app
                .wb
                .hits
                .borrow()
                .iter()
                .all(|h| !matches!(h.target, Target::Wizard(_) | Target::Key(KeyCode::Enter))));
            assert_eq!(app.handle_key(key(KeyCode::Enter)), AppAction::None);
            click(&mut app, Target::Command("/close-wizard"));
            assert_eq!(app.input, "original draft");
        }
    }

    #[test]
    fn unavailable_agents_and_cancel_are_visible_without_create_action() {
        let mut app = fixture(1);
        app.agents[0].available = false;
        app.start_add_agent();
        let term = draw(&app, 80, 24);
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("No available agents"));
        assert!(text.contains("Mock (unavailable)"));
        assert!(!app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|h| h.target == Target::Key(KeyCode::Enter)));
        click(&mut app, Target::Command("/close-wizard"));
        assert_eq!(app.mode, InputMode::Editing);
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn space_counts_picker_groups_and_file_activity_keep_their_scope() {
        let mut app = fixture(3);
        let first = app.sessions[0].session.id;
        let second = app.sessions[1].session.id;
        let third = app.sessions[2].session.id;
        app.sessions[2].session.workspace_id = WorkspaceId::new();
        app.sessions[2].workspace_name = "other".into();
        app.sessions[0].session.state = SessionState::Prompting;
        app.sessions[2].session.state = SessionState::Prompting;
        for (id, path) in [
            (first, "shared.rs"),
            (second, "shared.rs"),
            (third, "shared.rs"),
        ] {
            app.handle_event(Event {
                session_id: id,
                seq: 1,
                ts: Utc::now(),
                kind: EventKind::FileEdited { path: path.into() },
            });
        }
        let term = draw(&app, 120, 30);
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("2 agents · 1 running"), "{text}");
        assert!(text.contains("1 in other spaces"));
        assert_eq!(
            app.workspace_file_participants()["shared.rs"],
            vec![first, second]
        );
        app.wb.tab = SideTab::Files;
        app.wb.files.source = crate::files::Source::Activity;
        let term = draw(&app, 120, 30);
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("2 agents seen"));
        app.select_session(2);
        app.open_picker();
        assert_eq!(app.picker_matches(), vec![2, 0, 1]);
        let term = draw(&app, 80, 24);
        let text: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Current space") && text.contains("Other spaces"));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.selected_session_id(), Some(third));
    }

    #[test]
    fn keyboard_add_agent_and_naming_modals_isolate_background_controls() {
        let mut app = fixture(1);
        app.insert_text("draft");
        draw(&app, 80, 24);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        assert!(app
            .wb
            .control_focus
            .as_ref()
            .is_some_and(|t| *t == Target::Command("/add-agent")));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.wizard.as_ref().unwrap().adding);
        app.handle_key(key(KeyCode::Esc));
        app.command("/rename");
        draw(&app, 80, 24);
        assert!(app.wb.hits.borrow().iter().all(|h| !matches!(
            h.target,
            Target::Session(_) | Target::Wizard(_) | Target::Editor { .. }
        )));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn pasted_chinese_name_filters_picker_without_changing_draft() {
        let mut app = fixture(2);
        let second = app.sessions[1].session.id;
        app.apply_title(second, "回归测试".into(), 10);
        app.insert_text("source draft");
        app.open_picker();
        app.paste_text("回归测试");
        assert_eq!(app.picker_matches(), vec![1]);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.selected_session_id(), Some(second));
        app.select_session(0);
        assert_eq!(app.input, "source draft");
    }

    #[test]
    fn identical_titles_still_identify_the_recipient_instance() {
        let mut app = fixture(2);
        for view in app.sessions.clone() {
            app.apply_title(view.session.id, "same task".into(), 10);
        }
        for (index, label) in [(0, "To: agent-0 #1"), (1, "To: agent-1 #2")] {
            app.select_session(index);
            let term = draw(&app, 80, 24);
            let text: String = term
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains(label), "{text}");
        }
    }

    #[test]
    fn onboarding_path_form_and_result_continue_to_workspace() {
        let mut app = fixture(0);
        app.projects.clear();
        draw(&app, 80, 24);
        click(&mut app, Target::Command("/new"));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::ProjectPath);
        for c in "/repo with spaces".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        draw(&app, 80, 24);
        assert_eq!(
            click(&mut app, Target::Key(KeyCode::Enter)),
            AppAction::RegisterProject("/repo with spaces".into())
        );
        assert_eq!(app.handle_key(key(KeyCode::Enter)), AppAction::None);
        let project = Project {
            id: ProjectId::new(),
            name: "added".into(),
            root_path: "/repo with spaces".into(),
        };
        crate::apply_msg(&mut app, crate::UiMsg::Project(Ok(project.clone())));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Workspace);
        assert_eq!(app.wizard.as_ref().unwrap().project_id, Some(project.id));
    }

    #[test]
    fn modal_blocks_background_and_permission_double_click() {
        let mut app = fixture(1);
        app.insert_text("do not send");
        app.permission = Some(PermissionNotice {
            session_id: app.sessions[0].session.id,
            request_id: "request".into(),
            summary: "Write file?".into(),
            request: serde_json::json!({"options":[{"kind":"allow_once"}]}),
            resume: InputMode::Editing,
            pending: false,
        });
        app.open_permissions();
        draw(&app, 80, 24);
        assert!(!app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|h| matches!(h.target, Target::Command("/new"))));
        assert!(matches!(
            click(&mut app, Target::Key(KeyCode::Char('y'))),
            AppAction::RespondPermission {
                outcome: PermissionDecision::AllowOnce,
                ..
            }
        ));
        assert_eq!(
            click(&mut app, Target::Key(KeyCode::Char('y'))),
            AppAction::None
        );
        assert_eq!(app.input, "do not send");
    }

    #[test]
    fn click_files_and_return_to_chat() {
        let mut app = fixture(1);
        app.wb.files.source = crate::files::Source::Activity;
        for i in 0..30 {
            app.handle_event(Event {
                session_id: app.sessions[0].session.id,
                seq: i,
                ts: Utc::now(),
                kind: EventKind::FileEdited {
                    path: format!("src/{i}.rs").into(),
                },
            });
        }
        app.wb.file_cursor = 29;
        draw(&app, 120, 15);
        click(&mut app, Target::Tab(SideTab::Files));
        let term = draw(&app, 120, 15);
        let hits = app.wb.hits.borrow().clone();
        for h in hits.iter().filter(|h| matches!(h.target, Target::File(_))) {
            let Target::File(path) = &h.target else {
                unreachable!()
            };
            let line: String = (h.area.x..h.area.right())
                .map(|x| term.backend().buffer()[(x, h.area.y)].symbol())
                .collect();
            assert!(line.contains(path));
            assert_eq!(
                click(&mut app, h.target.clone()),
                AppAction::InspectFile(path.clone())
            );
        }
        app.wb.inspection = Some(("src/29.rs".into(), "+new".into()));
        draw(&app, 120, 30);
        click(&mut app, Target::Command("/chat"));
        assert!(app.wb.inspection.is_none());
    }

    #[test]
    fn mouse_editor_handles_unicode_and_wrapped_lines() {
        assert_eq!(cursor_at("中👩‍💻文", 4, 0, 1), 0);
        assert_eq!(cursor_at("中👩‍💻文", 4, 0, 3), "中".len());
        assert_eq!(cursor_at("中👩‍💻文", 4, 1, 0), "中👩‍💻".len());
        assert_eq!(cursor_at("abc\ndef", 10, 0, 8), 3);
        let mut app = fixture(1);
        app.insert_text("中文abc");
        draw(&app, 120, 30);
        let h = app
            .wb
            .hits
            .borrow()
            .iter()
            .find(|h| matches!(h.target, Target::Editor { .. }))
            .unwrap()
            .clone();
        mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: h.area.x + 2,
                row: h.area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        app.insert_text("!");
        assert_eq!(app.input, "中!文abc");
    }

    #[test]
    fn tab_and_escape_work_without_vim_modes() {
        let mut app = fixture(1);
        draw(&app, 80, 24);
        app.handle_key(key(KeyCode::Tab));
        assert!(app.wb.control_focus == Some(Target::Command("/new")));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, InputMode::NewSession);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, InputMode::Editing);
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.input, "i");
    }

    #[test]
    fn narrow_drawer_blocks_click_through_and_resize_rebuilds_targets() {
        let mut app = fixture(1);
        app.wb.drawer = true;
        app.wb.columns = 80;
        draw(&app, 80, 24);
        let block = app
            .wb
            .hits
            .borrow()
            .iter()
            .find(|h| h.target == Target::Blocked)
            .unwrap()
            .area;
        let cursor = app.wb.cursor;
        mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: block.right() - 1,
                row: block.y + 2,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert!(app.wb.drawer);
        assert_eq!(cursor, app.wb.cursor);
        app.wb.drawer = false;
        draw(&app, 40, 12);
        assert!(app
            .wb
            .hits
            .borrow()
            .iter()
            .all(|h| h.area.right() <= 40 && h.area.bottom() <= 12));
        assert!(!app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|h| matches!(h.target, Target::Session(_))));
        click(&mut app, Target::Menu);
        draw(&app, 40, 12);
        for _ in 0..crate::shell::menu_actions().len() {
            app.handle_key(key(KeyCode::Down));
        }
        draw(&app, 40, 12);
        assert!(app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|h| h.target == Target::Command("/quit")));
    }
    #[test]
    fn mouse_quote_picks_source_and_destination_without_shortcuts() {
        let mut app = fixture(2);
        let source = app.sessions[0].session.id;
        let destination = app.sessions[1].session.id;
        app.handle_event(Event {
            session_id: source,
            seq: 42,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("Useful finding".into()),
        });
        activate(&mut app, Target::Command("/relay"));
        draw(&app, 120, 30);
        click(&mut app, Target::Relay(0));
        draw(&app, 120, 30);
        click(&mut app, Target::Session(destination));
        assert_eq!(app.selected_session_id(), Some(destination));
        assert_eq!(app.mode, InputMode::Editing);
        assert_eq!(app.pending_relays.len(), 1);
        assert_eq!(app.pending_relays[0].source, source);
        assert_eq!(app.pending_relays[0].target, destination);
    }

    #[test]
    fn wheel_over_tasks_changes_task_without_scrolling_transcript() {
        let mut app = fixture(3);
        draw(&app, 120, 30);
        let area = app
            .wb
            .hits
            .borrow()
            .iter()
            .find(|h| matches!(h.target, Target::Session(_)))
            .unwrap()
            .area;
        mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.selected, 1);
        assert!(app.wb.sessions.values().all(|s| s.scroll == 0));
    }
    #[test]
    fn recovery_preserves_current_draft_and_references() {
        for command in ["/recover", "/unqueue"] {
            let mut app = fixture(1);
            let id = app.sessions[0].session.id;
            app.insert_text("new unsent draft");
            app.pending_relays.push(crate::app::PendingRelay {
                source: id,
                target: id,
                seq: 42,
            });
            let prompt = crate::workbench::Prompt {
                text: "old message".into(),
                references: vec![],
            };
            if command == "/recover" {
                app.wb.failed.entry(id).or_default().push(prompt);
            } else {
                app.wb.queues.entry(id).or_default().push_back(prompt);
            }
            app.command(command);
            assert_eq!(app.input, "old message");
            assert_eq!(app.wb.failed[&id].last().unwrap().text, "new unsent draft");
            app.command("/recover");
            assert_eq!(app.input, "new unsent draft");
            assert_eq!(app.pending_relays[0].seq, 42);
            assert_eq!(app.wb.failed[&id].last().unwrap().text, "old message");
        }
    }

    #[test]
    fn late_diff_never_closes_permission_or_replaces_a_newer_request() {
        let mut app = fixture(1);
        let id = app.sessions[0].session.id;
        let old = app.begin_diff("old.rs".into());
        let new = app.begin_diff("new.rs".into());
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request: old,
                session_id: id,
                path: "old.rs".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "old".into(),
                    scope: Default::default(),
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert!(app.wb.inspection.is_none());
        app.permission = Some(PermissionNotice {
            session_id: id,
            request_id: "req".into(),
            summary: "Review".into(),
            request: serde_json::json!({}),
            resume: InputMode::Editing,
            pending: false,
        });
        app.open_permissions();
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request: new,
                session_id: id,
                path: "new.rs".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "new".into(),
                    scope: Default::default(),
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert_eq!(app.mode, InputMode::Permission);
        assert!(app.wb.inspection.is_none());
        app.mode = InputMode::Editing;
        let request = app.begin_diff("new.rs".into());
        app.handle_key(key(KeyCode::Esc));
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request,
                session_id: id,
                path: "new.rs".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "new".into(),
                    scope: Default::default(),
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert!(app.wb.inspection.is_none());
        let request = app.begin_diff("new.rs".into());
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request,
                session_id: id,
                path: "new.rs".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "new".into(),
                    scope: Default::default(),
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert_eq!(app.wb.inspection, Some(("new.rs".into(), "new".into())));
    }

    #[test]
    fn arrows_follow_wrapped_cells_for_ascii_cjk_emoji_and_tabs() {
        for (width, text) in [
            (80, "a".repeat(100)),
            (120, "中👩‍💻".repeat(30)),
            (40, "a\tb".repeat(20)),
        ] {
            let mut app = fixture(1);
            app.insert_text(&text);
            draw(&app, width, 24);
            let layout = crate::editor::EditorLayout::new(&app.input, app.wb.editor_width.get());
            let (row, col) = layout.position(app.wb.cursor);
            assert!(row > 0);
            app.handle_key(key(KeyCode::Up));
            assert_eq!(layout.position(app.wb.cursor), (row - 1, col));
            assert!(app.input.is_char_boundary(app.wb.cursor));
            app.handle_key(key(KeyCode::Down));
            assert_eq!(app.wb.cursor, text.len());
        }
    }

    #[test]
    fn tab_skips_controls_covered_by_a_narrow_drawer() {
        let mut app = fixture(1);
        app.wb.drawer = true;
        app.wb.columns = 40;
        draw(&app, 40, 24);
        let blocker = app
            .wb
            .hits
            .borrow()
            .iter()
            .find(|h| h.target == Target::Blocked)
            .unwrap()
            .area;
        let mut seen = false;
        for _ in 0..30 {
            app.handle_key(key(KeyCode::Tab));
            let target = app.wb.control_focus.as_ref().unwrap();
            let hits = app.wb.hits.borrow();
            let index = hits.iter().position(|h| &h.target == target).unwrap();
            let blocker_index = hits
                .iter()
                .position(|h| h.target == Target::Blocked)
                .unwrap();
            if index < blocker_index {
                assert!(hits[index].area.intersection(blocker).is_empty());
            }
            seen |= matches!(target, Target::Session(_));
        }
        assert!(seen, "drawer tasks remain keyboard accessible");
    }
}
