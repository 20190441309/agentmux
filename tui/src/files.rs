//! Workspace changes and agent activity remain separate, with workspace-bound replies.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use agentmux_core::{
    rpc::{DiffScope, WorkspaceChange, WorkspaceChangesResult, WorkspaceDiffParams},
    WorkspaceId,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::{
    app::{App, AppAction, InputMode},
    interaction::{buttons, hit, Target},
    shell::fit_text,
    theme::THEME,
    workbench::SideTab,
};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    #[default]
    Workspace,
    Activity,
}

#[derive(Default)]
pub struct Snapshot {
    pub data: Option<WorkspaceChangesResult>,
    pub error: Option<String>,
    pub request: Option<u64>,
    pub checked: Option<Instant>,
}

#[derive(Default)]
pub struct Files {
    pub source: Source,
    pub query: String,
    pub searching: bool,
    pub snapshots: HashMap<WorkspaceId, Snapshot>,
    pub serial: u64,
    pub diff_scope: DiffScope,
    pub shown_scope: DiffScope,
    pub diff_params: Option<WorkspaceDiffParams>,
    pub diff_binary: bool,
    pub diff_truncated: bool,
    pub inspection_width: std::cell::Cell<u16>,
    pub inspection_limit: std::cell::Cell<u16>,
}

impl App {
    pub fn file_workspace(&self) -> Option<WorkspaceId> {
        self.selected_session()
            .map(|view| view.session.workspace_id)
    }

    pub fn changed_files(&self) -> Vec<WorkspaceChange> {
        self.file_workspace()
            .and_then(|id| self.wb.files.snapshots.get(&id))
            .and_then(|snapshot| snapshot.data.as_ref())
            .map(|data| {
                data.files
                    .iter()
                    .filter(|file| {
                        file.path
                            .to_lowercase()
                            .contains(&self.wb.files.query.to_lowercase())
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn activity_files(&self) -> Vec<String> {
        self.touched_files()
            .into_iter()
            .filter(|path| {
                path.to_lowercase()
                    .contains(&self.wb.files.query.to_lowercase())
            })
            .collect()
    }

    pub fn file_rows(&self) -> usize {
        match self.wb.files.source {
            Source::Workspace => self.changed_files().len(),
            Source::Activity => self.activity_files().len(),
        }
    }

    pub fn file_refresh(&mut self, force: bool) -> Option<(WorkspaceId, u64)> {
        if !self.wb.connected || self.wb.files.source != Source::Workspace {
            return None;
        }
        let id = self.file_workspace()?;
        let snapshot = self.wb.files.snapshots.entry(id).or_default();
        if snapshot.request.is_some()
            || (!force
                && snapshot
                    .checked
                    .is_some_and(|time| time.elapsed() < Duration::from_secs(3)))
        {
            return None;
        }
        self.wb.files.serial += 1;
        let request = self.wb.files.serial;
        snapshot.request = Some(request);
        Some((id, request))
    }

    pub fn apply_files(
        &mut self,
        workspace: WorkspaceId,
        request: u64,
        result: Result<WorkspaceChangesResult, String>,
    ) {
        let selected = if self.file_workspace() == Some(workspace) {
            self.changed_files()
                .get(self.wb.file_cursor)
                .map(WorkspaceChange::key)
        } else {
            None
        };
        let snapshot = self.wb.files.snapshots.entry(workspace).or_default();
        if snapshot.request != Some(request) {
            return;
        }
        snapshot.request = None;
        snapshot.checked = Some(Instant::now());
        match result {
            Ok(data) => {
                snapshot.data = Some(data);
                snapshot.error = None;
            }
            Err(error) => snapshot.error = Some(error),
        }
        if self.file_workspace() == Some(workspace) {
            if let Some(error) = self
                .wb
                .files
                .snapshots
                .get(&workspace)
                .and_then(|snapshot| snapshot.error.clone())
            {
                self.set_error_for(None, format!("Workspace changes unavailable: {error}"));
            }
            let files = self.changed_files();
            self.wb.file_cursor = selected
                .and_then(|key| files.iter().position(|file| file.key() == key))
                .unwrap_or_else(|| self.wb.file_cursor.min(files.len().saturating_sub(1)));
        }
    }

    pub fn inspect_selected_file(&mut self) -> AppAction {
        self.wb.files.searching = false;
        match self.wb.files.source {
            Source::Activity => self
                .activity_files()
                .get(self.wb.file_cursor)
                .cloned()
                .map(AppAction::InspectFile)
                .unwrap_or(AppAction::None),
            Source::Workspace => {
                let Some(workspace) = self.file_workspace() else {
                    return AppAction::None;
                };
                let Some(file) = self.changed_files().get(self.wb.file_cursor).cloned() else {
                    return AppAction::None;
                };
                if let Some(error) = file.unavailable {
                    self.set_error(error);
                    return AppAction::None;
                }
                AppAction::InspectChange(WorkspaceDiffParams {
                    workspace_id: workspace,
                    path: file.path,
                    path_bytes: file.path_bytes,
                    old_path: file.old_path,
                    old_path_bytes: file.old_path_bytes,
                    scope: self.wb.files.diff_scope,
                })
            }
        }
    }
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    if !search_active(app) {
        return None;
    }
    match key.code {
        KeyCode::Esc => app.wb.files.searching = false,
        KeyCode::Enter => return Some(app.inspect_selected_file()),
        KeyCode::Backspace => {
            app.wb.files.query.pop();
            app.wb.file_cursor = 0;
        }
        KeyCode::Up => app.wb.file_cursor = app.wb.file_cursor.saturating_sub(1),
        KeyCode::Down => {
            app.wb.file_cursor = (app.wb.file_cursor + 1).min(app.file_rows().saturating_sub(1))
        }
        KeyCode::Tab | KeyCode::BackTab => {
            app.wb.files.searching = false;
            return None;
        }
        _ if key.modifiers.intersects(
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
        ) || matches!(key.code, KeyCode::F(_)) =>
        {
            return None
        }
        _ => {
            if let Some(character) = crate::input::plain_char(&key) {
                app.wb.files.query.push(character);
                app.wb.file_cursor = 0;
            }
        }
    }
    Some(AppAction::None)
}

pub fn select_source(app: &mut App, source: Source) {
    app.wb.files.source = source;
    app.wb.file_cursor = 0;
    app.wb.files.searching = false;
    app.mode = InputMode::Sidebar;
}

pub fn scope(app: &mut App, scope: DiffScope) -> AppAction {
    let Some(mut params) = app.wb.files.diff_params.clone() else {
        return AppAction::None;
    };
    if app.file_workspace() != Some(params.workspace_id) {
        return AppAction::None;
    }
    params.scope = scope;
    app.wb.files.diff_scope = scope;
    AppAction::InspectChange(params)
}

pub fn hunk(app: &mut App, next: bool) {
    let Some((_, text)) = &app.wb.inspection else {
        return;
    };
    let width = app.wb.files.inspection_width.get().max(1);
    let mut row = 0;
    let mut hunks = vec![];
    for line in text.lines() {
        if line.starts_with("@@") {
            hunks.push(row);
        }
        row += Paragraph::new(line.to_owned())
            .wrap(ratatui::widgets::Wrap { trim: false })
            .line_count(width)
            .max(1);
    }
    let current = usize::from(app.wb.inspect_scroll);
    if let Some(row) = if next {
        hunks.into_iter().find(|row| *row > current)
    } else {
        hunks.into_iter().rev().find(|row| *row < current)
    } {
        app.wb.inspect_scroll = row.min(u16::MAX as usize) as u16;
    }
}

fn label(path: &str) -> String {
    path.replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

pub fn search_active(app: &App) -> bool {
    app.wb.files.searching
        && matches!(app.mode, InputMode::Sidebar | InputMode::Editing)
        && !app.wb.menu
        && app.wb.naming.is_none()
        && app.wb.pi_panel.is_none()
        && app.wb.attention.panel.is_none()
        && app.wb.transcript.panel.is_none()
}

pub fn visible(app: &App) -> bool {
    app.show_diff || (app.wb.tab == SideTab::Files && app.sidebar_visible())
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    buttons(
        frame,
        app,
        parts[0],
        &[
            (
                if area.width < 36 { "Git" } else { "Workspace" },
                Target::FileSource(Source::Workspace),
            ),
            ("Activity", Target::FileSource(Source::Activity)),
        ],
    );
    let snapshot = app
        .file_workspace()
        .and_then(|id| app.wb.files.snapshots.get(&id));
    let marker = if app.wb.files.source == Source::Workspace
        && snapshot.is_some_and(|snapshot| snapshot.error.is_some())
    {
        " · stale"
    } else if app.wb.files.source == Source::Workspace
        && snapshot.is_some_and(|snapshot| snapshot.request.is_some())
    {
        " · loading"
    } else {
        ""
    };
    let prompt = format!(" / {}{marker}", app.wb.files.query);
    frame.render_widget(
        Paragraph::new(fit_text(&prompt, usize::from(parts[1].width))).style(
            if app.wb.files.searching {
                THEME.accent
            } else {
                THEME.dim
            },
        ),
        parts[1],
    );
    hit(app, parts[1], Target::Command("/file-search"));
    if search_active(app) && parts[1].width > 2 {
        use unicode_width::UnicodeWidthStr;
        frame.set_cursor_position((
            parts[1].x + (prompt.width() as u16).min(parts[1].width - 1),
            parts[1].y,
        ));
    }
    buttons(
        frame,
        app,
        parts[3],
        &[
            ("Find", Target::Command("/file-search")),
            ("Refresh", Target::Command("/refresh-files")),
        ],
    );
    let participants = app.workspace_file_participants();
    let width = usize::from(parts[2].width.saturating_sub(2));
    let workspace = app.file_workspace();
    let mut items = vec![];
    let mut targets = vec![];
    match app.wb.files.source {
        Source::Workspace => {
            let snapshot = workspace.and_then(|id| app.wb.files.snapshots.get(&id));
            let files = app.changed_files();
            for file in files {
                let name = if let Some(old) = &file.old_path {
                    format!("{} → {}", label(old), label(&file.path))
                } else {
                    label(&file.path)
                };
                let stats = if let Some(error) = &file.unavailable {
                    error.clone()
                } else if file.binary {
                    "binary".into()
                } else if file.size_bytes.is_some_and(|bytes| bytes > 4 * 1024 * 1024) {
                    "large file".into()
                } else {
                    format!(
                        "+{} -{}",
                        file.added
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "?".into()),
                        file.deleted
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "?".into())
                    )
                };
                let count = participants.get(&file.path).map(Vec::len).unwrap_or(0);
                let status = format!(
                    "{}{} {stats}{}",
                    file.index_status,
                    file.worktree_status,
                    if count > 1 {
                        format!(" · {count} agents seen")
                    } else {
                        String::new()
                    }
                );
                items.push(ListItem::new(vec![
                    Line::styled(fit_text(&name, width), THEME.text),
                    Line::styled(
                        fit_text(&status, width),
                        if file.unavailable.is_some() {
                            THEME.error
                        } else {
                            THEME.dim
                        },
                    ),
                ]));
                targets.push(Target::WorkspaceFile(workspace.unwrap(), file.key()));
            }
            if items.is_empty() {
                let message = match snapshot {
                    Some(snapshot) if snapshot.error.is_some() => {
                        format!("Unavailable: {}", snapshot.error.as_ref().unwrap())
                    }
                    Some(snapshot) if snapshot.data.is_some() => {
                        "No matching workspace changes".into()
                    }
                    _ => "Loading workspace changes…".into(),
                };
                frame.render_widget(Paragraph::new(message).style(THEME.dim), parts[2]);
            }
        }
        Source::Activity => {
            for path in app.activity_files() {
                let count = participants.get(&path).map(Vec::len).unwrap_or(0);
                items.push(ListItem::new(vec![
                    Line::styled(fit_text(&label(&path), width), THEME.text),
                    Line::styled(
                        if count > 1 {
                            format!("{count} agents seen")
                        } else {
                            "This agent".into()
                        },
                        THEME.dim,
                    ),
                ]));
                targets.push(Target::File(path));
            }
        }
    }
    if items.is_empty() {
        return;
    }
    let mut state = ListState::default().with_selected(Some(app.wb.file_cursor));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(THEME.selection)
            .highlight_symbol("› "),
        parts[2],
        &mut state,
    );
    for (row, target) in targets
        .into_iter()
        .skip(state.offset())
        .take(usize::from(parts[2].height) / 2)
        .enumerate()
    {
        hit(
            app,
            Rect::new(parts[2].x, parts[2].y + row as u16 * 2, parts[2].width, 2),
            target,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{PendingRelay, SessionView};
    use agentmux_core::{AgentId, Session, SessionId, SessionState};
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};

    fn app() -> App {
        let workspace_id = WorkspaceId::new();
        let session = Session {
            id: SessionId::new(),
            workspace_id,
            agent_id: AgentId::new("mock"),
            state: SessionState::Ready,
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

    fn change(path: &str) -> WorkspaceChange {
        WorkspaceChange {
            path: path.into(),
            path_bytes: None,
            old_path: None,
            old_path_bytes: None,
            index_status: "M".into(),
            worktree_status: "M".into(),
            added: Some(2),
            deleted: Some(1),
            binary: false,
            size_bytes: Some(40),
            unavailable: None,
        }
    }

    fn populate(app: &mut App, files: Vec<WorkspaceChange>) {
        let (id, request) = app.file_refresh(true).unwrap();
        app.apply_files(id, request, Ok(WorkspaceChangesResult { files }));
    }

    #[test]
    fn git_files_are_independent_of_agent_events_and_raw_paths_stay_exact() {
        let mut app = app();
        let mut raw = change("raw-�");
        raw.path_bytes = Some(b"raw-\xff".to_vec());
        populate(&mut app, vec![change("manual.txt"), raw]);
        assert!(app.touched_files().is_empty());
        assert_eq!(app.changed_files().len(), 2);
        app.wb.file_cursor = 1;
        assert!(
            matches!(app.inspect_selected_file(), AppAction::InspectChange(params) if params.path_bytes == Some(b"raw-\xff".to_vec()))
        );
        select_source(&mut app, Source::Activity);
        assert_eq!(app.file_rows(), 0);
    }

    #[test]
    fn refresh_keeps_selected_path_reading_position_and_rejects_stale_replies() {
        let mut app = app();
        populate(&mut app, vec![change("a.txt"), change("z.txt")]);
        app.wb.file_cursor = 1;
        app.wb.inspection = Some(("z.txt".into(), "old diff".into()));
        app.wb.inspect_scroll = 42;
        let (id, request) = app.file_refresh(true).unwrap();
        app.wb.files.snapshots.get_mut(&id).unwrap().request = Some(request + 1);
        app.apply_files(id, request, Ok(WorkspaceChangesResult { files: vec![] }));
        assert_eq!(app.changed_files().len(), 2);
        app.apply_files(
            id,
            request + 1,
            Ok(WorkspaceChangesResult {
                files: vec![change("0.txt"), change("a.txt"), change("z.txt")],
            }),
        );
        assert_eq!(app.wb.file_cursor, 2);
        assert_eq!(app.wb.inspect_scroll, 42);
        assert_eq!(app.wb.inspection.as_ref().unwrap().1, "old diff");
        let (id, request) = app.file_refresh(true).unwrap();
        app.sessions[0].session.workspace_id = WorkspaceId::new();
        app.apply_files(
            id,
            request,
            Ok(WorkspaceChangesResult {
                files: vec![change("foreign.txt")],
            }),
        );
        assert!(app.changed_files().is_empty());
        assert_eq!(app.wb.inspect_scroll, 42);
    }

    #[test]
    fn filename_search_preserves_draft_and_references_and_modal_paste_is_isolated() {
        let mut app = app();
        populate(&mut app, vec![change("中文文件.txt"), change("other.txt")]);
        app.insert_text("draft");
        let id = app.selected_session_id().unwrap();
        let reference = PendingRelay {
            source: id,
            target: id,
            seq: 1,
        };
        app.pending_relays.push(reference.clone());
        app.command("/file-search");
        app.paste_text("中文");
        assert_eq!(app.changed_files().len(), 1);
        assert_eq!(app.input, "draft");
        assert_eq!(app.pending_relays, vec![reference]);
        crate::attention::open(&mut app);
        app.paste_text("ignored");
        assert_eq!(app.wb.files.query, "中文");
        crate::attention::close(&mut app);
        assert!(
            matches!(keyboard(&mut app, KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)), Some(AppAction::InspectChange(params)) if params.path == "中文文件.txt")
        );
    }

    #[test]
    fn failures_are_explicit_and_cached_data_is_marked_stale() {
        let mut app = app();
        populate(&mut app, vec![change("cached.txt")]);
        let (id, request) = app.file_refresh(true).unwrap();
        app.apply_files(id, request, Err("git unavailable".into()));
        let mut terminal = Terminal::new(TestBackend::new(34, 18)).unwrap();
        terminal
            .draw(|frame| draw(frame, &app, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("stale"));
        assert!(text.contains("cached.txt"));
        assert!(app
            .wb
            .attention
            .latest_error()
            .unwrap()
            .message
            .contains("git unavailable"));
    }

    #[test]
    fn hunk_navigation_uses_wrapped_terminal_rows() {
        let mut app = app();
        app.wb.inspection = Some((
            "中文.txt".into(),
            "diff header\n@@ first @@\n+中文中文中文\n@@ second @@\n+last\n".into(),
        ));
        app.wb.files.inspection_width.set(6);
        hunk(&mut app, true);
        let first = app.wb.inspect_scroll;
        assert!(first > 0);
        hunk(&mut app, true);
        assert!(app.wb.inspect_scroll > first);
        hunk(&mut app, false);
        assert_eq!(app.wb.inspect_scroll, first);
    }

    #[test]
    fn scopes_keep_old_content_until_matching_typed_response_arrives() {
        let mut app = app();
        let id = app.selected_session_id().unwrap();
        let workspace = app.file_workspace().unwrap();
        app.wb.inspection = Some(("a.txt".into(), "original HEAD diff".into()));
        app.wb.inspect_scroll = 5;
        app.wb.files.diff_params = Some(WorkspaceDiffParams {
            workspace_id: workspace,
            path: "a.txt".into(),
            path_bytes: None,
            old_path: None,
            old_path_bytes: None,
            scope: DiffScope::Staged,
        });
        let request = app.begin_diff("a.txt".into());
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request,
                session_id: id,
                path: "a.txt".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "wrong scope".into(),
                    scope: DiffScope::Head,
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert_eq!(app.wb.inspection.as_ref().unwrap().1, "original HEAD diff");
        let request = app.begin_diff("a.txt".into());
        app.handle_key(KeyEvent::new(
            KeyCode::PageDown,
            crossterm::event::KeyModifiers::NONE,
        ));
        let scroll = app.wb.inspect_scroll;
        crate::apply_msg(
            &mut app,
            crate::UiMsg::Diff {
                request,
                session_id: id,
                path: "a.txt".into(),
                result: Ok(agentmux_core::rpc::WorkspaceDiffResult {
                    text: "staged diff".into(),
                    scope: DiffScope::Staged,
                    binary: false,
                    truncated: false,
                }),
            },
        );
        assert_eq!(app.wb.inspection.as_ref().unwrap().1, "staged diff");
        assert_eq!(app.wb.files.shown_scope, DiffScope::Staged);
        assert_eq!(app.wb.inspect_scroll, scroll);
    }
}
