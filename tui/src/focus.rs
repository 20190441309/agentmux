//! Workbench focus does not change recipients or native byte forwarding.
use crate::{
    app::{App, InputMode},
    interaction::Target,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Input,
    Reading,
    Navigation,
}

pub fn current(app: &App) -> Pane {
    if let Some(target) = &app.wb.control_focus {
        return match target {
            Target::Session(_)
            | Target::File(_)
            | Target::WorkspaceFile(..)
            | Target::FileSource(_)
            | Target::Tab(_) => Pane::Navigation,
            Target::Hunk(_) | Target::DiffScope(_) => Pane::Reading,
            Target::Editor { .. } => Pane::Input,
            _ => mode(app),
        };
    }
    mode(app)
}

fn mode(app: &App) -> Pane {
    match app.mode {
        InputMode::Normal => Pane::Reading,
        InputMode::Sidebar => Pane::Navigation,
        _ => Pane::Input,
    }
}

pub fn select(app: &mut App, pane: Pane) {
    app.wb.control_focus = None;
    app.wb.files.searching = false;
    app.mode = match pane {
        Pane::Input => {
            app.wb.drawer = false;
            InputMode::Editing
        }
        Pane::Reading => {
            app.wb.drawer = false;
            InputMode::Normal
        }
        Pane::Navigation => {
            app.wb.focus = false;
            app.wb.drawer = app.wb.columns < 110;
            InputMode::Sidebar
        }
    };
}

pub fn cycle(app: &mut App, reverse: bool) {
    let next = match (current(app), reverse) {
        (Pane::Input, false) | (Pane::Navigation, true) => Pane::Reading,
        (Pane::Reading, false) | (Pane::Input, true) => Pane::Navigation,
        _ => Pane::Input,
    };
    select(app, next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppAction, SessionView};
    use agentmux_core::{AgentId, Session, SessionId, SessionState, WorkspaceId};
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        let session = Session {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            agent_id: AgentId::new("mock"),
            state: SessionState::Prompting,
            acp_session_id: None,
            native_session_file: None,
            native_terminal: false,
            references: vec![],
            created_at: Utc::now(),
        };
        App::new(
            vec![],
            vec![],
            vec![SessionView {
                session,
                agent_name: "Mock".into(),
                workspace_name: "space".into(),
            }],
            vec![],
        )
    }

    #[test]
    fn direct_focus_and_cycles_preserve_recipient_and_draft() {
        let mut app = app();
        app.insert_text("中文 draft");
        let id = app.selected_session_id();
        for (key, pane) in [(2, Pane::Input), (3, Pane::Reading), (4, Pane::Navigation)] {
            app.handle_key(KeyEvent::new(KeyCode::F(key), KeyModifiers::NONE));
            assert_eq!(current(&app), pane);
            assert_eq!(app.selected_session_id(), id);
            assert_eq!(app.input, "中文 draft");
        }
        cycle(&mut app, false);
        assert_eq!(current(&app), Pane::Input);
        cycle(&mut app, true);
        assert_eq!(current(&app), Pane::Navigation);
    }

    #[test]
    fn cancel_is_consistent_across_structured_panes_and_keeps_the_draft() {
        let mut app = app();
        app.insert_text("draft");
        for pane in [Pane::Input, Pane::Reading, Pane::Navigation] {
            select(&mut app, pane);
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                AppAction::CancelPrompt
            );
            assert_eq!(app.input, "draft");
        }
        app.sessions[0].session.native_terminal = true;
        select(&mut app, Pane::Reading);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            AppAction::None
        );
    }

    #[test]
    fn inspection_home_end_and_hunks_do_not_move_the_editor_cursor() {
        let mut app = app();
        app.insert_text("draft");
        let cursor = app.wb.cursor;
        app.wb.inspection = Some((
            "file".into(),
            "header\n@@ first\n+one\n@@ second\n+two".into(),
        ));
        app.wb.files.inspection_limit.set(8);
        app.wb.files.inspection_width.set(80);
        select(&mut app, Pane::Reading);
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.wb.inspect_scroll, 8);
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.wb.inspect_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert_eq!(app.wb.inspect_scroll, 1);
        assert_eq!(app.wb.cursor, cursor);
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn permission_and_attention_overlays_block_background_focus_shortcuts() {
        let mut app = app();
        crate::attention::open(&mut app);
        app.handle_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Editing);
        crate::attention::close(&mut app);
        app.mode = InputMode::Permission;
        app.handle_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
        assert_ne!(app.mode, InputMode::Sidebar);
    }

    #[test]
    fn keyboard_only_send_permission_diff_and_return_workflow() {
        use agentmux_core::{Event, EventKind, PermissionDecision};
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = app();
        let id = app.selected_session_id().unwrap();
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        for character in "任务中文".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert!(
            matches!(app.handle_key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),AppAction::Submit{text,..} if text=="任务中文")
        );
        app.insert_text("unsent draft");
        app.handle_event(Event{session_id:id,seq:1,ts:Utc::now(),kind:EventKind::PermissionRequest{request_id:"request".into(),request:serde_json::json!({"toolCall":{"title":"Run tests"},"options":[{"kind":"allow_once"}]})}});
        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.mode, InputMode::Permission);
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            AppAction::RespondPermission {
                outcome: PermissionDecision::AllowOnce,
                ..
            }
        ));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let workspace = app.file_workspace().unwrap();
        app.wb.files.snapshots.entry(workspace).or_default().data =
            Some(agentmux_core::rpc::WorkspaceChangesResult {
                files: vec![agentmux_core::rpc::WorkspaceChange {
                    path: "file.rs".into(),
                    path_bytes: None,
                    old_path: None,
                    old_path_bytes: None,
                    index_status: "M".into(),
                    worktree_status: " ".into(),
                    added: Some(1),
                    deleted: Some(0),
                    binary: false,
                    size_bytes: Some(5),
                    unavailable: None,
                }],
            });
        app.wb.tab = crate::workbench::SideTab::Files;
        app.handle_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
        assert!(
            matches!(app.handle_key(KeyEvent::new(KeyCode::Enter,KeyModifiers::NONE)),AppAction::InspectChange(params) if params.path=="file.rs")
        );
        let request = app.begin_diff("file.rs".into());
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request,
                session_id: id,
                path: "file.rs".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "@@ test @@\n+new\n".into(),
                    scope: Default::default(),
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert!(app.wb.inspection.is_some());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| crate::shell::draw(frame, &app))
            .unwrap();
        for _ in 0..100 {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            if app.wb.control_focus.as_ref() == Some(&Target::Command("/chat")) {
                break;
            }
        }
        assert!(app.wb.control_focus.as_ref() == Some(&Target::Command("/chat")));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.wb.inspection.is_none());
        assert_eq!(app.mode, InputMode::Editing);
        assert_eq!(app.input, "unsent draft");
    }
}
