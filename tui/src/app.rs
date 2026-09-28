//! `App` — the TUI's UI-state container.
//!
//! Pure state machine: no terminal, no I/O, no async. Everything the
//! render loop draws lives here; everything the key handler decides is
//! returned as an [`AppAction`] for `main.rs` to execute against the
//! daemon. This is what makes the app unit-testable without a TTY.

use agentmux_core::{AgentProfile, Event, EventKind, Session, SessionId, SessionState, Workspace};
use crossterm::event::{KeyEvent, KeyEventKind};

use crate::input;

/// What the UI is currently doing with keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Navigation keys are active (`j/k`, `n`, `q`, …).
    Normal,
    /// Keystrokes go into the prompt input box.
    Editing,
    /// Picking a relay target session. Variant defined for Task 14 —
    /// only minimal enter/leave/confirm behavior exists today.
    RelayPick,
}

/// A request from the key handler to the async main loop.
///
/// `App` itself never touches the daemon; it translates keys into these
/// actions and `main.rs` performs the matching `DaemonClient` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    /// Nothing to do (unhandled key, or a pure state change).
    None,
    /// `q` — leave the UI.
    Quit,
    /// Editing `Enter` — send the buffer as a prompt to the selected
    /// session.
    Submit(String),
    /// `n` — create a session (v1: first workspace + first agent).
    NewSession,
    /// `ctrl-c` in Normal mode — cancel the selected session's turn.
    CancelPrompt,
    /// RelayPick `Enter` — relay the current selection (Task 14 wiring).
    Relay,
}

/// A [`Session`] decorated with the display names of its agent and
/// workspace, so the list never has to look them up while rendering.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub session: Session,
    pub agent_name: String,
    pub workspace_name: String,
}

/// All renderable UI state.
pub struct App {
    /// Sessions in display order (grouped by workspace).
    pub sessions: Vec<SessionView>,
    /// Index into `sessions` of the highlighted session.
    pub selected: usize,
    /// Events received from the daemon, newest last. The right pane
    /// renders the slice belonging to the selected session.
    pub events: Vec<Event>,
    pub mode: InputMode,
    /// Workspaces in display order — sessions group under these headers;
    /// empty ones still render so `n` has somewhere to land.
    pub workspaces: Vec<Workspace>,
    /// Configured agents (for `n` picking the first available one).
    pub agents: Vec<AgentProfile>,
    /// Prompt input buffer (`Editing` mode).
    pub input: String,
    /// One-line status shown in the bottom bar (connection problems,
    /// action results, …). Cleared on the next successful action.
    pub status: Option<String>,
}

/// Upper bound on the in-memory event log — a safety valve so a long
/// session cannot grow `events` without limit. The daemon's per-session
/// JSONL log stays authoritative; this only bounds what the TUI renders.
const MAX_EVENTS: usize = 10_000;

impl App {
    pub fn new(
        workspaces: Vec<Workspace>,
        sessions: Vec<SessionView>,
        agents: Vec<AgentProfile>,
    ) -> App {
        App {
            sessions,
            selected: 0,
            events: Vec::new(),
            mode: InputMode::Normal,
            workspaces,
            agents,
            input: String::new(),
            status: None,
        }
    }

    /// Merge one daemon [`Event`] into the UI state: `StateChanged`
    /// updates the matching session's badge state, and every event is
    /// appended to the log the right pane renders.
    pub fn handle_event(&mut self, ev: Event) {
        if let EventKind::StateChanged { to, .. } = &ev.kind {
            if let Some(view) = self
                .sessions
                .iter_mut()
                .find(|v| v.session.id == ev.session_id)
            {
                view.session.state = to.clone();
            }
        }
        self.events.push(ev);
        // Bound the log so a long-lived session can't grow it forever;
        // the daemon's JSONL event log stays authoritative.
        if self.events.len() >= MAX_EVENTS {
            self.events.drain(..MAX_EVENTS / 4);
        }
    }

    /// Translate one key press into an [`AppAction`]; key → action
    /// mapping lives in [`crate::input`].
    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        // Terminals reporting key-event kinds (kitty protocol, Windows)
        // also emit `Release`; only presses and auto-repeat act.
        if key.kind == KeyEventKind::Release {
            return AppAction::None;
        }
        match self.mode {
            InputMode::Normal => input::normal_key(self, key),
            InputMode::Editing => input::editing_key(self, key),
            InputMode::RelayPick => input::relay_pick_key(self, key),
        }
    }

    /// `j`/Down: move the highlight one session down, clamped.
    pub(crate) fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1).min(self.sessions.len() - 1);
        }
    }

    /// `k`/Up: move the highlight one session up, clamped.
    pub(crate) fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// The highlighted session, if any.
    pub fn selected_session(&self) -> Option<&SessionView> {
        self.sessions.get(self.selected)
    }

    /// Convenience for the action layer.
    pub fn selected_session_id(&self) -> Option<SessionId> {
        self.selected_session().map(|v| v.session.id)
    }

    /// Events the right pane shows: the selected session's events plus
    /// global notices — daemon-side lag warnings and the client's own
    /// lag notice carry the nil session id and would otherwise be
    /// invisible (they belong to no session).
    /// `DoubleEndedIterator` so the renderer can scan backwards from the
    /// newest events and stop early (viewport-bounded draw).
    pub fn events_for_selected(&self) -> impl DoubleEndedIterator<Item = &Event> {
        let selected = self.selected_session_id();
        self.events
            .iter()
            .filter(move |ev| Some(ev.session_id) == selected || ev.session_id.0.is_nil())
    }

    /// Insert a session keeping the list grouped by workspace: it lands
    /// after the existing sessions of the same workspace, or at the end
    /// when its workspace isn't listed. Returns the new index.
    pub fn add_session(&mut self, view: SessionView) -> usize {
        let insert_at = self
            .sessions
            .iter()
            .rposition(|v| v.session.workspace_id == view.session.workspace_id)
            .map(|i| i + 1)
            .unwrap_or(self.sessions.len());
        self.sessions.insert(insert_at, view);
        insert_at
    }

    /// Record a status-bar message.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// Badge `(glyph, label)` for a session state — the contract between
    /// `handle_event` updates and the session list's rendering.
    /// Readable single glyphs that survive common terminal fonts:
    /// `○ ◌ ● ◐ ! ✓ ✗`.
    pub fn badge(state: &SessionState) -> (&'static str, &'static str) {
        match state {
            SessionState::Created => ("○", "created"),
            SessionState::Connecting => ("◌", "connecting"),
            SessionState::Ready => ("●", "ready"),
            SessionState::Prompting => ("◐", "prompting"),
            SessionState::WaitingPermission => ("!", "permission"),
            SessionState::Done => ("✓", "done"),
            SessionState::Error(_) => ("✗", "error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmux_core::{AgentId, WorkspaceId};
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn workspace(name: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            project_id: agentmux_core::ProjectId::new(),
            name: name.to_string(),
            worktree_path: format!("/wt/{name}").into(),
            branch: name.to_string(),
            created_at: Utc::now(),
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

    fn event(session_id: SessionId, kind: EventKind) -> Event {
        Event {
            session_id,
            seq: 0,
            ts: Utc::now(),
            kind,
        }
    }

    fn app_with_sessions(states: &[SessionState]) -> App {
        let ws = workspace("alpha");
        let sessions = states
            .iter()
            .map(|s| SessionView {
                session: session(ws.id, s.clone()),
                agent_name: "claude".into(),
                workspace_name: ws.name.clone(),
            })
            .collect();
        App::new(vec![ws], sessions, vec![])
    }

    // --- event handling -------------------------------------------------

    #[test]
    fn state_changed_event_updates_badge_state() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let id = app.sessions[0].session.id;
        app.handle_event(event(
            id,
            EventKind::StateChanged {
                from: SessionState::Ready,
                to: SessionState::Prompting,
            },
        ));
        assert_eq!(app.sessions[0].session.state, SessionState::Prompting);
        assert!(app.events_for_selected().count() == 1);
    }

    #[test]
    fn event_log_collects_events_per_session() {
        let mut app = app_with_sessions(&[SessionState::Ready, SessionState::Done]);
        let first = app.sessions[0].session.id;
        let second = app.sessions[1].session.id;
        app.handle_event(event(first, EventKind::Orchestrator("hi".into())));
        app.handle_event(event(second, EventKind::Orchestrator("yo".into())));
        assert_eq!(app.events_for_selected().count(), 1);
        app.selected = 1;
        assert_eq!(app.events_for_selected().count(), 1);
    }

    #[test]
    fn nil_id_global_notices_are_visible() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        // The client's lag notice convention: nil session id.
        let nil_id = SessionId(uuid::Uuid::nil());
        app.handle_event(event(
            nil_id,
            EventKind::Orchestrator("client event stream lagged".into()),
        ));
        let visible: Vec<_> = app.events_for_selected().collect();
        assert_eq!(visible.len(), 1, "nil-id notice must render");
        // Still visible with no session selected at all.
        let mut empty = App::new(vec![], vec![], vec![]);
        empty.handle_event(event(nil_id, EventKind::Orchestrator("x".into())));
        assert_eq!(empty.events_for_selected().count(), 1);
    }

    // --- Normal-mode keys ------------------------------------------------

    #[test]
    fn normal_jk_moves_selection_clamped() {
        let mut app = app_with_sessions(&[
            SessionState::Ready,
            SessionState::Ready,
            SessionState::Ready,
        ]);
        assert_eq!(app.selected, 0);
        assert_eq!(app.handle_key(key(KeyCode::Char('j'))), AppAction::None);
        assert_eq!(app.selected, 1);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 2);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected, 2, "clamped at last session");
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected, 1);
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected, 0, "clamped at first session");
    }

    #[test]
    fn normal_q_quits() {
        let mut app = app_with_sessions(&[]);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), AppAction::Quit);
    }

    #[test]
    fn normal_i_and_a_enter_editing() {
        let mut app = app_with_sessions(&[]);
        assert_eq!(app.handle_key(key(KeyCode::Char('i'))), AppAction::None);
        assert_eq!(app.mode, InputMode::Editing);
        app.mode = InputMode::Normal;
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.mode, InputMode::Editing);
    }

    #[test]
    fn normal_n_requests_new_session() {
        let mut app = app_with_sessions(&[]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            AppAction::NewSession
        );
    }

    #[test]
    fn normal_ctrl_c_cancels_prompt() {
        let mut app = app_with_sessions(&[SessionState::Prompting]);
        assert_eq!(
            app.handle_key(ctrl(KeyCode::Char('c'))),
            AppAction::CancelPrompt
        );
    }

    #[test]
    fn normal_at_enters_relay_pick() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        assert_eq!(app.handle_key(key(KeyCode::Char('@'))), AppAction::None);
        assert_eq!(app.mode, InputMode::RelayPick);
    }

    // --- Editing-mode keys ------------------------------------------------

    #[test]
    fn editing_typing_and_enter_submits() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        app.mode = InputMode::Editing;
        for c in "fix it".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.input, "fix it");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            AppAction::Submit("fix it".into())
        );
        assert_eq!(app.input, "", "buffer drained after submit");
    }

    #[test]
    fn editing_esc_and_ctrl_c_return_to_normal() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        app.mode = InputMode::Editing;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, InputMode::Normal);
        app.mode = InputMode::Editing;
        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), AppAction::None);
        assert_eq!(app.mode, InputMode::Normal);
    }

    #[test]
    fn editing_backspace_edits_buffer() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        app.mode = InputMode::Editing;
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "");
    }

    // --- RelayPick-mode keys (Task 14 stub) --------------------------------

    #[test]
    fn relay_pick_esc_leaves_and_enter_relays() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        app.mode = InputMode::RelayPick;
        assert_eq!(app.handle_key(key(KeyCode::Esc)), AppAction::None);
        assert_eq!(app.mode, InputMode::Normal);
        app.mode = InputMode::RelayPick;
        assert_eq!(app.handle_key(key(KeyCode::Enter)), AppAction::Relay);
        assert_eq!(app.mode, InputMode::Normal);
    }

    // --- helpers -----------------------------------------------------------

    #[test]
    fn add_session_keeps_workspace_groups_together() {
        let a = workspace("a");
        let b = workspace("b");
        let mut app = App::new(vec![a.clone(), b.clone()], vec![], vec![]);
        app.add_session(SessionView {
            session: session(a.id, SessionState::Ready),
            agent_name: "x".into(),
            workspace_name: "a".into(),
        });
        app.add_session(SessionView {
            session: session(b.id, SessionState::Ready),
            agent_name: "x".into(),
            workspace_name: "b".into(),
        });
        // A second session for workspace `a` must land before `b`'s row.
        let idx = app.add_session(SessionView {
            session: session(a.id, SessionState::Done),
            agent_name: "x".into(),
            workspace_name: "a".into(),
        });
        assert_eq!(idx, 1);
        assert_eq!(app.sessions[1].session.workspace_id, a.id);
        assert_eq!(app.sessions[2].session.workspace_id, b.id);
    }
}
