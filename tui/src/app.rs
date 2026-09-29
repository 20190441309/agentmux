//! `App` — the TUI's UI-state container.
//!
//! Pure state machine: no terminal, no I/O, no async. Everything the
//! render loop draws lives here; everything the key handler decides is
//! returned as an [`AppAction`] for `main.rs` to execute against the
//! daemon. This is what makes the app unit-testable without a TTY.

use std::collections::HashSet;

use agentmux_core::{
    AgentId, AgentProfile, Event, EventKind, PermissionDecision, Project, Session, SessionId,
    SessionState, Workspace,
};
use crossterm::event::{KeyEvent, KeyEventKind};

use crate::input;
use crate::newsession::{NewSessionWizard, WorkspacePick};

/// What the UI is currently doing with keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Navigation keys are active (`j/k`, `n`, `q`, …).
    Normal,
    /// Keystrokes go into the prompt input box.
    Editing,
    /// Two-stage `@` relay pick: choose a source event in the selected
    /// session's log, then the session the reference is sent to.
    RelayPick,
    /// `n` new-session wizard: project → workspace → agent.
    NewSession,
    /// Interactive permission dialog. `EventKind::PermissionRequest`
    /// opens it; `y`/`a`/`n`/`Esc` answer via `session/permission` and
    /// the overlay stays up (marked pending) until the matching
    /// `PermissionResolved` event arrives.
    Permission,
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
    /// session. `references` are the relays staged via `@`, converted by
    /// `main.rs` into `SessionPromptParams.references`.
    Submit {
        text: String,
        references: Vec<PendingRelay>,
    },
    /// New-session wizard finished. `main.rs` runs `workspace/create`
    /// first when `workspace` is [`WorkspacePick::New`].
    CreateSession {
        workspace: WorkspacePick,
        agent_id: AgentId,
    },
    /// `ctrl-c` in Normal mode — cancel the selected session's turn.
    CancelPrompt,
    /// `x` in Normal mode — kill the selected session (`session/kill`).
    /// Kill is direct (no confirm dialog): the session is recoverable
    /// via `r`/`session/resume`.
    KillSession,
    /// `r` in Normal mode — resume a `Done`/`Error` session
    /// (`session/resume`) — the only way back for sessions the daemon
    /// swept to `Error` at boot.
    ResumeSession,
    /// `y`/`a`/`n`/`Esc` in Permission mode — answer the parked agent
    /// permission request (`session/permission`). The overlay stays up
    /// until the daemon's `PermissionResolved` event confirms it.
    RespondPermission {
        session_id: SessionId,
        request_id: String,
        outcome: PermissionDecision,
    },
}

/// A relay staged by the `@` picker, consumed by the next
/// [`AppAction::Submit`]. `App` keeps it in its own terms — `main.rs`
/// maps `source`/`seq` to the wire `SessionRef`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRelay {
    /// Session whose event log the reference points into.
    pub source: SessionId,
    /// The picked event's seq — the reference's `event_seq` bound.
    pub seq: u64,
    /// Session the relayed context was aimed at. The picker moves
    /// `selected` onto it when the pick completes, so the `Submit` goes
    /// there; the field records intent (and survives a later reselect).
    pub target: SessionId,
}

/// Which half of the two-stage `@` pick is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStage {
    /// Picking the source event within the selected session's log.
    Event,
    /// Picking the session the reference is relayed to.
    Session,
}

/// The source event chosen at stage [`RelayStage::Event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySource {
    pub session_id: SessionId,
    /// The event's daemon-assigned seq — becomes `SessionRef::event_seq`.
    pub seq: u64,
    /// One-line summary rendered into the `[@…]` input marker.
    pub summary: String,
}

/// State of the `@` relay picker — `Some` iff `mode == RelayPick`.
#[derive(Debug, Clone)]
pub struct RelayPick {
    pub stage: RelayStage,
    /// Cursor over the selected session's events (stage `Event`).
    pub event_cursor: usize,
    /// Cursor over `sessions` (stage `Session`).
    pub session_cursor: usize,
    /// The event picked when stage `Event` was confirmed.
    pub source: Option<RelaySource>,
}

/// A permission request parked agent-side and awaiting the user's
/// answer (see [`InputMode::Permission`]).
#[derive(Debug, Clone)]
pub struct PermissionNotice {
    /// Session whose agent asked.
    pub session_id: SessionId,
    /// The id `session/permission` must echo back.
    pub request_id: String,
    /// Human-readable rendering of the request (tool title + options).
    pub summary: String,
    /// The raw `RequestPermissionRequest` payload — the overlay lists
    /// its `options`, and [`allows_always`](Self::allows_always) gates
    /// the `a` key on an `allow_always` kind being offered.
    pub request: serde_json::Value,
    /// Mode to restore when the request resolves.
    pub resume: InputMode,
    /// An answer key was pressed and the `session/permission` call is
    /// in flight — the overlay stays up (keys ignored) until
    /// `PermissionResolved` or an RPC error (which clears this flag so
    /// the user can answer again).
    pub pending: bool,
}

impl PermissionNotice {
    /// Whether the agent offered an `allow_always` option — the `a`
    /// key answers `allow_always` only then.
    pub fn allows_always(&self) -> bool {
        self.request
            .get("options")
            .and_then(|o| o.as_array())
            .map(|opts| {
                opts.iter()
                    .any(|o| o.get("kind").and_then(|k| k.as_str()) == Some("allow_always"))
            })
            .unwrap_or(false)
    }
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
    /// Registered projects — the wizard's first pick.
    pub projects: Vec<Project>,
    /// Workspaces in display order — sessions group under these headers;
    /// empty ones still render so `n` has somewhere to land.
    pub workspaces: Vec<Workspace>,
    /// Configured agents (the wizard offers the `available` ones).
    pub agents: Vec<AgentProfile>,
    /// Prompt input buffer (`Editing` mode).
    pub input: String,
    /// One-line status shown in the bottom bar (connection problems,
    /// action results, …). Cleared on the next successful action.
    pub status: Option<String>,
    /// `@` picker state — `Some` iff `mode == RelayPick`.
    pub relay: Option<RelayPick>,
    /// Relays staged by the picker, attached to the next `Submit` and
    /// consumed by it.
    pub pending_relays: Vec<PendingRelay>,
    /// `n` wizard state — `Some` iff `mode == NewSession`.
    pub wizard: Option<NewSessionWizard>,
    /// Displayed permission notice — `Some` iff `mode == Permission`.
    pub permission: Option<PermissionNotice>,
    /// Right pane shows the touched-files list instead of the event
    /// stream (`Tab` toggles).
    pub show_diff: bool,
    /// Sessions that received events while not selected — the list marks
    /// them `•` until the selection lands on them (cleared by
    /// [`mark_selected_viewed`](Self::mark_selected_viewed)).
    pub unread: HashSet<SessionId>,
}

/// Upper bound on the in-memory event log — a safety valve so a long
/// session cannot grow `events` without limit. The daemon's per-session
/// JSONL log stays authoritative; this only bounds what the TUI renders.
const MAX_EVENTS: usize = 10_000;

impl App {
    pub fn new(
        projects: Vec<Project>,
        workspaces: Vec<Workspace>,
        sessions: Vec<SessionView>,
        agents: Vec<AgentProfile>,
    ) -> App {
        App {
            sessions,
            selected: 0,
            events: Vec::new(),
            mode: InputMode::Normal,
            projects,
            workspaces,
            agents,
            input: String::new(),
            status: None,
            relay: None,
            pending_relays: Vec::new(),
            wizard: None,
            permission: None,
            show_diff: false,
            unread: HashSet::new(),
        }
    }

    /// Merge one daemon [`Event`] into the UI state: `StateChanged`
    /// updates the matching session's badge state, `PermissionRequest`
    /// opens the interactive [`InputMode::Permission`] dialog,
    /// `PermissionResolved` closes the matching one, and every event is
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
        match &ev.kind {
            EventKind::PermissionRequest {
                request_id,
                request,
            } => {
                // Stacked requests keep the original interrupted mode —
                // `resume` of an existing notice wins over the current
                // (already-Permission) mode.
                let resume = self
                    .permission
                    .as_ref()
                    .map(|p| p.resume)
                    .unwrap_or(self.mode);
                self.permission = Some(PermissionNotice {
                    session_id: ev.session_id,
                    request_id: request_id.clone(),
                    summary: permission_summary(request),
                    request: request.clone(),
                    resume,
                    pending: false,
                });
                self.mode = InputMode::Permission;
            }
            EventKind::PermissionResolved {
                request_id,
                outcome,
            } => {
                // Only a resolution for the *displayed* request dismisses
                // the dialog; a stacked/unknown one just logs.
                if let Some(notice) = &self.permission {
                    if notice.request_id == *request_id {
                        self.mode = notice.resume;
                        self.permission = None;
                        self.set_status(format!("permission {outcome}"));
                    }
                }
            }
            _ => {}
        }
        // Activity on a non-selected session is unread until viewed;
        // nil-id global notices belong to no session and never mark one.
        if !ev.session_id.0.is_nil() && Some(ev.session_id) != self.selected_session_id() {
            self.unread.insert(ev.session_id);
        }
        self.events.push(ev);
        // Bound the log so a long-lived session can't grow it forever;
        // the daemon's JSONL event log stays authoritative.
        if self.events.len() >= MAX_EVENTS {
            self.events.drain(..MAX_EVENTS / 4);
        }
    }

    /// Clear the unread marker of the highlighted session — called
    /// wherever `selected` moves (`j`/`k`, relay finish, creation).
    pub fn mark_selected_viewed(&mut self) {
        if let Some(id) = self.selected_session_id() {
            self.unread.remove(&id);
        }
    }

    /// Translate one key press into an [`AppAction`]; key → action
    /// mapping lives in [`crate::input`] and [`crate::newsession`].
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
            InputMode::NewSession => crate::newsession::wizard_key(self, key),
            InputMode::Permission => input::permission_key(self, key),
        }
    }

    /// `j`/Down: move the highlight one session down, clamped.
    pub(crate) fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1).min(self.sessions.len() - 1);
            self.mark_selected_viewed();
        }
    }

    /// `k`/Up: move the highlight one session up, clamped.
    pub(crate) fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.mark_selected_viewed();
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

    /// Events strictly owned by the selected session — unlike
    /// [`events_for_selected`](Self::events_for_selected), the nil-id
    /// global notices are excluded (referencing them would produce a
    /// `SessionRef` pointing at the daemon's own log). The `@` picker
    /// and the files panel operate on this set.
    pub fn session_events(&self) -> impl DoubleEndedIterator<Item = &Event> {
        let selected = self.selected_session_id();
        self.events
            .iter()
            .filter(move |ev| Some(ev.session_id) == selected)
    }

    /// `@` in Normal: open the relay picker at stage `Event` on the
    /// selected session's log — refused with a status hint when the
    /// session has no events yet.
    pub(crate) fn start_relay(&mut self) {
        if self.selected_session_id().is_none() {
            self.set_status("no session selected");
            return;
        }
        let count = self.session_events().count();
        if count == 0 {
            self.set_status("no events to relay yet");
            return;
        }
        self.relay = Some(RelayPick {
            stage: RelayStage::Event,
            // Start on the newest event — the common pick.
            event_cursor: count - 1,
            session_cursor: self.selected,
            source: None,
        });
        self.mode = InputMode::RelayPick;
    }

    /// Finish the two-stage pick (stage `Session` → `Enter`): insert the
    /// `[@<agent>/<session-short>#<seq>: <summary>]` marker into the
    /// input buffer, record the [`PendingRelay`], move `selected` onto
    /// the target session and drop into `Editing` so the message can be
    /// continued and submitted.
    pub(crate) fn finish_relay(&mut self, pick: RelayPick) {
        let Some(source) = pick.source else {
            self.mode = InputMode::Normal;
            return;
        };
        // Copy what we need up front — the borrow must end before the
        // mutations below (selection move clears the unread marker).
        let Some((target_id, target_name)) = self
            .sessions
            .get(pick.session_cursor)
            .map(|v| (v.session.id, v.agent_name.clone()))
        else {
            self.mode = InputMode::Normal;
            self.set_status("relay aborted — no target session");
            return;
        };
        let (agent, short) = self
            .sessions
            .iter()
            .find(|v| v.session.id == source.session_id)
            .map(|v| {
                (
                    v.agent_name.clone(),
                    short_id(&v.session.id.to_string()).to_string(),
                )
            })
            .unwrap_or_else(|| {
                (
                    "?".to_string(),
                    short_id(&source.session_id.to_string()).to_string(),
                )
            });
        let marker = format!("[@{agent}/{short}#{}: {}]", source.seq, source.summary);
        if !self.input.is_empty() && !self.input.ends_with(' ') {
            self.input.push(' ');
        }
        self.input.push_str(&marker);
        self.input.push(' ');
        self.pending_relays.push(PendingRelay {
            source: source.session_id,
            seq: source.seq,
            target: target_id,
        });
        self.selected = pick.session_cursor;
        self.mark_selected_viewed();
        self.relay = None;
        self.mode = InputMode::Editing;
        self.set_status(format!(
            "relay → {}·{}",
            target_name,
            short_id(&target_id.to_string())
        ));
    }

    /// `n` in Normal: open the new-session wizard. Needs at least one
    /// project (a workspace must live somewhere) and one available
    /// agent — anything missing gets a status hint instead.
    pub(crate) fn start_wizard(&mut self) {
        if self.projects.is_empty() {
            self.set_status("no projects registered — register one first (project/register)");
            return;
        }
        if !self.agents.iter().any(|a| a.available) {
            self.set_status("no available agents — none of the configured agents probed usable");
            return;
        }
        self.wizard = Some(NewSessionWizard::default());
        self.mode = InputMode::NewSession;
    }

    /// File paths the selected session touched, in first-touch order:
    /// [`EventKind::FileEdited`] paths plus the `locations` of `tool_call`
    /// session updates. Drives the `Tab` diff/files panel. Pi
    /// `tool_execution_*` records contribute their `args` path too —
    /// normally the conn translator already emits `FileEdited` +
    /// `tool_call` upstream, so this is the defensive path for records
    /// that reached the log unnormalized.
    pub fn touched_files(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for ev in self.session_events() {
            match &ev.kind {
                EventKind::FileEdited { path } => {
                    note_touched(&mut seen, path.display().to_string());
                }
                EventKind::SessionUpdate(v) => {
                    let update = v.get("update").unwrap_or(v);
                    if update.get("sessionUpdate").and_then(|u| u.as_str()) == Some("tool_call") {
                        if let Some(locations) = update.get("locations").and_then(|l| l.as_array())
                        {
                            for loc in locations {
                                if let Some(path) = loc.get("path").and_then(|p| p.as_str()) {
                                    note_touched(&mut seen, path.to_string());
                                }
                            }
                        }
                    } else if let Some(path) = agentmux_core::pi_shape::tool_path(update) {
                        note_touched(&mut seen, path.to_string());
                    }
                }
                _ => {}
            }
        }
        seen
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

    /// The `session/permission` RPC for `request_id` failed: un-pend the
    /// matching notice (the request is still parked agent-side — the
    /// overlay must stay up so the user can answer again) and show the
    /// failure.
    pub fn permission_answer_failed(&mut self, request_id: &str, error: String) {
        if let Some(notice) = &mut self.permission {
            if notice.request_id == request_id {
                notice.pending = false;
                self.set_status(format!(
                    "permission response failed: {error} — answer again"
                ));
                return;
            }
        }
        self.set_status(format!("permission response failed: {error}"));
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

/// Push `path` onto `seen` on first sight — first-touch order.
fn note_touched(seen: &mut Vec<String>, path: String) {
    if !seen.contains(&path) {
        seen.push(path);
    }
}

/// First 8 chars of a session id — enough to tell sessions apart in the
/// list and in `[@…]` relay markers.
pub(crate) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// One-line summary of `ev` for the relay marker — mirrors the
/// orchestrator's `describe_event` (private over there; the TUI keeps
/// its own copy so the marker is self-contained).
pub(crate) fn event_summary(ev: &Event) -> String {
    if let Some(summary) = agentmux_core::collab::summarize_event(&ev.kind) {
        return truncate(summary, 80);
    }
    let text = match &ev.kind {
        EventKind::SessionUpdate(v) => {
            let update = v.get("update").unwrap_or(v);
            match update.get("sessionUpdate").and_then(|u| u.as_str()) {
                Some("agent_message_chunk") => update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                    .map(|t| format!("message: {t}"))
                    .unwrap_or_else(|| "agent message".to_string()),
                Some(other) => format!("session update: {other}"),
                // A pi-native passthrough record — summarize by `type`.
                None => agentmux_core::pi_shape::summary(update)
                    .or_else(|| {
                        agentmux_core::pi_shape::kind(update).map(|k| format!("pi event: {k}"))
                    })
                    .unwrap_or_else(|| "session update".to_string()),
            }
        }
        EventKind::StateChanged { from, to } => format!("state {from:?} → {to:?}"),
        EventKind::AgentExited { code } => format!("agent exited (code {code:?})"),
        EventKind::Orchestrator(note) => note.clone(),
        EventKind::PermissionRequest { request, .. } => {
            format!("permission requested — {}", permission_summary(request))
        }
        EventKind::PermissionResolved { outcome, .. } => format!("permission {outcome}"),
        // `summarize_event` covers FileEdited above.
        EventKind::FileEdited { path } => format!("edited {}", path.display()),
    };
    truncate(text, 80)
}

/// Truncate to `max` chars with an ellipsis so a relay marker stays a
/// single line even for multi-line event payloads.
fn truncate(text: String, max: usize) -> String {
    let first_line = text.lines().next().unwrap_or("").to_string();
    if first_line.chars().count() > max {
        let cut: String = first_line.chars().take(max).collect();
        format!("{cut}…")
    } else {
        first_line
    }
}

/// Render a `PermissionRequest` payload (the serialized ACP
/// `RequestPermissionRequest`, camelCase) as a one-line notice. `ui`
/// reuses it for the banner line the same event leaves in the stream.
pub(crate) fn permission_summary(request: &serde_json::Value) -> String {
    let title = request.pointer("/toolCall/title").and_then(|t| t.as_str());
    let options: Vec<&str> = request
        .get("options")
        .and_then(|o| o.as_array())
        .map(|opts| {
            opts.iter()
                .filter_map(|o| o.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let what = title.unwrap_or("permission requested");
    if options.is_empty() {
        truncate(format!("agent asks: {what}"), 120)
    } else {
        truncate(
            format!("agent asks: {what} (options: {})", options.join(", ")),
            120,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::newsession::{WizardStep, WorkspacePick};
    use agentmux_core::{AdapterKind, AgentId, ProjectId, WorkspaceId};
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn project(name: &str) -> Project {
        Project {
            id: ProjectId::new(),
            root_path: PathBuf::from(format!("/repos/{name}")),
            name: name.to_string(),
        }
    }

    fn workspace_in(project_id: ProjectId, name: &str) -> Workspace {
        Workspace {
            id: WorkspaceId::new(),
            project_id,
            name: name.to_string(),
            worktree_path: format!("/wt/{name}").into(),
            branch: name.to_string(),
            created_at: Utc::now(),
        }
    }

    fn agent(id: &str, available: bool) -> AgentProfile {
        AgentProfile {
            id: AgentId::new(id),
            name: id.to_string(),
            adapter: AdapterKind::Acp {
                command: PathBuf::from(format!("/bin/{id}")),
                args: vec![],
            },
            env: BTreeMap::new(),
            available,
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
        let proj = project("proj");
        let ws = workspace_in(proj.id, "alpha");
        let sessions = states
            .iter()
            .map(|s| SessionView {
                session: session(ws.id, s.clone()),
                agent_name: "claude".into(),
                workspace_name: ws.name.clone(),
            })
            .collect();
        App::new(vec![proj], vec![ws], sessions, vec![agent("claude", true)])
    }

    /// An app with one project, one workspace and two agents (one
    /// unavailable) — the wizard playground.
    fn wizard_app() -> App {
        let proj = project("myproj");
        let ws = workspace_in(proj.id, "ws1");
        App::new(
            vec![proj],
            vec![ws],
            vec![],
            vec![agent("mock", true), agent("gone", false)],
        )
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
        let mut empty = App::new(vec![], vec![], vec![], vec![]);
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
    fn normal_n_opens_new_session_wizard() {
        let mut app = app_with_sessions(&[]);
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), AppAction::None);
        assert_eq!(app.mode, InputMode::NewSession);
        assert!(app.wizard.is_some());
    }

    #[test]
    fn normal_n_without_projects_stays_normal() {
        // No projects → nowhere to create a workspace; status explains.
        let mut app = App::new(vec![], vec![], vec![], vec![agent("mock", true)]);
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), AppAction::None);
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.status.is_some());
    }

    #[test]
    fn normal_n_without_available_agents_stays_normal() {
        let proj = project("p");
        let mut app = App::new(
            vec![proj.clone()],
            vec![workspace_in(proj.id, "w")],
            vec![],
            vec![agent("mock", false)],
        );
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.status.is_some());
    }

    #[test]
    fn normal_tab_toggles_diff_pane() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        assert!(!app.show_diff);
        app.handle_key(key(KeyCode::Tab));
        assert!(app.show_diff);
        app.handle_key(key(KeyCode::Tab));
        assert!(!app.show_diff);
    }

    #[test]
    fn normal_ctrl_c_cancels_prompt() {
        let mut app = app_with_sessions(&[SessionState::Prompting]);
        assert_eq!(
            app.handle_key(ctrl(KeyCode::Char('c'))),
            AppAction::CancelPrompt
        );
    }

    /// `x`/`r` — the lifecycle pair the daemon's zombie sweep makes
    /// reachable UI-side: kill a live session, resume a terminal one.
    /// Both emit unconditionally; `main.rs` maps "no selection" to a
    /// status hint.
    #[test]
    fn normal_x_and_r_kill_and_resume() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('x'))),
            AppAction::KillSession
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Char('r'))),
            AppAction::ResumeSession
        );

        // They don't fire in Editing — `x`/`r` are plain text there.
        app.mode = InputMode::Editing;
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), AppAction::None);
        assert_eq!(app.input, "x");
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), AppAction::None);
        assert_eq!(app.input, "xr");
    }

    #[test]
    fn normal_at_enters_relay_pick() {
        // Covered in detail below (`normal_at_enters_relay_pick_over_events`);
        // `@` on an event-less session is refused with a status hint.
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        app.handle_event(event(sid, EventKind::Orchestrator("e".into())));
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
            AppAction::Submit {
                text: "fix it".into(),
                references: vec![]
            }
        );
        assert_eq!(app.input, "", "buffer drained after submit");
    }

    #[test]
    fn editing_submit_carries_pending_relay_references() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        app.pending_relays.push(PendingRelay {
            source: sid,
            seq: 3,
            target: sid,
        });
        app.mode = InputMode::Editing;
        for c in "go".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            AppAction::Submit {
                text: "go".into(),
                references: vec![PendingRelay {
                    source: sid,
                    seq: 3,
                    target: sid
                }],
            },
            "Submit must carry the staged relays for main.rs to send"
        );
        assert!(app.pending_relays.is_empty(), "submit consumes the relays");
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

    // --- RelayPick-mode keys (Task 14) --------------------------------------

    #[test]
    fn normal_at_enters_relay_pick_over_events() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        app.handle_event(event(sid, EventKind::Orchestrator("hi".into())));
        assert_eq!(app.handle_key(key(KeyCode::Char('@'))), AppAction::None);
        assert_eq!(app.mode, InputMode::RelayPick);
        let pick = app.relay.as_ref().expect("relay state must exist");
        assert_eq!(pick.stage, RelayStage::Event);
        assert_eq!(pick.event_cursor, 0, "cursor starts on the only event");
    }

    #[test]
    fn normal_at_without_events_stays_normal() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        app.handle_key(key(KeyCode::Char('@')));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.status.is_some(), "status explains the refusal");
    }

    /// Brief test ②: the full two-stage pick — event → target session —
    /// formats a `[@agent/session#seq: summary]` marker into the input
    /// buffer and records the pending relay for the next Submit.
    #[test]
    fn relay_pick_flow_inserts_marker_and_records_pending() {
        let mut app = app_with_sessions(&[SessionState::Ready, SessionState::Ready]);
        let src = app.sessions[0].session.id;
        let target = app.sessions[1].session.id;
        app.handle_event(event(
            src,
            EventKind::FileEdited {
                path: "src/lib.rs".into(),
            },
        ));
        app.events[0].seq = 7;

        app.handle_key(key(KeyCode::Char('@')));
        assert_eq!(app.mode, InputMode::RelayPick);
        // Stage 1: pick the source event (cursor already on it).
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.relay.as_ref().unwrap().stage, RelayStage::Session);
        // Stage 2: move to the second session, confirm.
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.mode, InputMode::Editing, "marker ready to extend");
        let short = &src.to_string()[..8];
        assert!(
            app.input
                .contains(&format!("[@claude/{short}#7: edited src/lib.rs]")),
            "marker text in input buffer: {:?}",
            app.input
        );
        assert_eq!(
            app.pending_relays,
            vec![PendingRelay {
                source: src,
                seq: 7,
                target,
            }]
        );
        assert_eq!(app.selected, 1, "selection moves to the relay target");
        assert!(app.relay.is_none(), "picker state cleared");

        // …and the next submit ships the reference.
        for c in "apply it".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            AppAction::Submit { references, .. } if references.len() == 1
        ));
    }

    #[test]
    fn relay_pick_esc_aborts_at_each_stage() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        app.handle_event(event(sid, EventKind::Orchestrator("x".into())));

        app.handle_key(key(KeyCode::Char('@')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.relay.is_none());

        app.handle_key(key(KeyCode::Char('@')));
        app.handle_key(key(KeyCode::Enter)); // → stage Session
        assert_eq!(app.relay.as_ref().unwrap().stage, RelayStage::Session);
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.relay.is_none());
        assert!(app.pending_relays.is_empty());
        assert_eq!(app.input, "");
    }

    /// Stage-Event `j`/`k` walk the selected session's event log (newest
    /// starts selected), clamped at both ends.
    #[test]
    fn relay_event_cursor_walks_session_log() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        for i in 0..3 {
            app.handle_event(event(sid, EventKind::Orchestrator(format!("e{i}"))));
        }
        app.handle_key(key(KeyCode::Char('@')));
        let pick = app.relay.as_ref().unwrap();
        assert_eq!(pick.event_cursor, 2, "starts on the newest event");
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('k'))); // clamp
        assert_eq!(app.relay.as_ref().unwrap().event_cursor, 0);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.relay.as_ref().unwrap().event_cursor, 1);
    }

    // --- permission events (interactive answers) ------------------------------

    fn perm_request_event(sid: SessionId, request_id: &str, options: serde_json::Value) -> Event {
        event(
            sid,
            EventKind::PermissionRequest {
                request_id: request_id.to_string(),
                request: serde_json::json!({
                    "sessionId": "mock",
                    "toolCall": {"toolCallId": "tc-1", "title": "Write src/x.rs"},
                    "options": options,
                }),
            },
        )
    }

    fn perm_options(kinds: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!(kinds
            .iter()
            .map(|(name, kind)| serde_json::json!({"name": name, "kind": kind}))
            .collect::<Vec<_>>())
    }

    /// Brief test ③: a `PermissionRequest` event switches the app into
    /// the answer dialog; `y` produces the `session/permission` action
    /// carrying the request id, and the overlay stays pending until the
    /// `PermissionResolved` event lands.
    #[test]
    fn permission_event_opens_dialog_and_y_answers_allow_once() {
        let mut app = app_with_sessions(&[SessionState::Prompting]);
        let sid = app.sessions[0].session.id;
        app.handle_event(perm_request_event(
            sid,
            "req-1",
            perm_options(&[("Reject", "reject_once"), ("Allow", "allow_once")]),
        ));
        assert_eq!(app.mode, InputMode::Permission);
        let notice = app.permission.as_ref().expect("permission notice");
        assert!(
            notice.summary.contains("Write src/x.rs"),
            "summary should name the tool call: {}",
            notice.summary
        );
        assert!(notice.summary.contains("Reject"), "options listed");
        assert!(!notice.allows_always(), "no allow_always was offered");

        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            AppAction::RespondPermission {
                session_id: sid,
                request_id: "req-1".into(),
                outcome: PermissionDecision::AllowOnce,
            }
        );
        // The overlay stays up while the RPC is in flight; keys are
        // ignored so a double press can't race a second answer.
        assert_eq!(app.mode, InputMode::Permission);
        assert!(app.permission.as_ref().unwrap().pending);
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), AppAction::None);

        // The resolved event dismisses and restores the mode.
        app.handle_event(event(
            sid,
            EventKind::PermissionResolved {
                request_id: "req-1".into(),
                outcome: "selected:allow".into(),
            },
        ));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.permission.is_none());
        assert!(app.status.as_deref().unwrap().contains("selected:allow"));
    }

    /// `n`/`Esc` reject; `a` answers allow-always only when the agent
    /// offered that kind.
    #[test]
    fn permission_keys_reject_and_gate_allow_always() {
        let mut app = app_with_sessions(&[SessionState::Prompting]);
        let sid = app.sessions[0].session.id;
        app.handle_event(perm_request_event(
            sid,
            "req-1",
            perm_options(&[("Allow", "allow_once"), ("No", "reject_once")]),
        ));
        // `a` is a no-op — allow_always wasn't offered.
        assert_eq!(app.handle_key(key(KeyCode::Char('a'))), AppAction::None);
        assert!(!app.permission.as_ref().unwrap().pending);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            AppAction::RespondPermission {
                session_id: sid,
                request_id: "req-1".into(),
                outcome: PermissionDecision::Reject,
            }
        );

        // With allow_always offered, `a` answers it; Esc rejects.
        let mut app = app_with_sessions(&[SessionState::Prompting]);
        let sid = app.sessions[0].session.id;
        app.handle_event(perm_request_event(
            sid,
            "req-2",
            perm_options(&[("Always", "allow_always"), ("No", "reject_once")]),
        ));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            AppAction::RespondPermission {
                session_id: sid,
                request_id: "req-2".into(),
                outcome: PermissionDecision::AllowAlways,
            }
        );

        let mut app = app_with_sessions(&[SessionState::Prompting]);
        let sid = app.sessions[0].session.id;
        app.handle_event(perm_request_event(sid, "req-3", perm_options(&[])));
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            AppAction::RespondPermission {
                session_id: sid,
                request_id: "req-3".into(),
                outcome: PermissionDecision::Reject,
            }
        );
    }

    /// The dialog interrupts whatever mode was active; resolution
    /// restores it (and a stray resolved for another request_id doesn't
    /// dismiss anything).
    #[test]
    fn permission_resolution_restores_interrupted_mode() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        app.mode = InputMode::Editing;
        app.input = "keep me".into();
        app.handle_event(perm_request_event(
            app.sessions[0].session.id,
            "req-1",
            perm_options(&[]),
        ));
        assert_eq!(app.mode, InputMode::Permission);

        // A resolved for a different request_id must not dismiss.
        app.handle_event(event(
            app.sessions[0].session.id,
            EventKind::PermissionResolved {
                request_id: "other".into(),
                outcome: "cancelled".into(),
            },
        ));
        assert_eq!(app.mode, InputMode::Permission);
        assert!(app.permission.is_some());

        app.handle_key(key(KeyCode::Char('y'))); // answer → pending
        app.handle_event(event(
            app.sessions[0].session.id,
            EventKind::PermissionResolved {
                request_id: "req-1".into(),
                outcome: "selected:allow".into(),
            },
        ));
        assert_eq!(
            app.mode,
            InputMode::Editing,
            "resolution resumes the interrupted mode"
        );
        assert_eq!(app.input, "keep me", "input buffer survives the dialog");
    }

    /// A failed `session/permission` call un-pends the overlay so the
    /// user can answer again.
    #[test]
    fn permission_answer_failure_allows_retry() {
        let mut app = app_with_sessions(&[SessionState::Prompting]);
        let sid = app.sessions[0].session.id;
        app.handle_event(perm_request_event(sid, "req-1", perm_options(&[])));
        app.handle_key(key(KeyCode::Char('y')));
        assert!(app.permission.as_ref().unwrap().pending);

        app.permission_answer_failed("req-1", "daemon gone".into());
        assert!(!app.permission.as_ref().unwrap().pending);
        assert!(app.status.as_deref().unwrap().contains("answer again"));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            AppAction::RespondPermission {
                session_id: sid,
                request_id: "req-1".into(),
                outcome: PermissionDecision::Reject,
            },
            "a retry issues a fresh response"
        );
    }

    // --- new-session wizard (Task 14) -----------------------------------------

    #[test]
    fn wizard_flow_existing_workspace() {
        let mut app = wizard_app();
        let ws_id = app.workspaces[0].id;
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Project);
        app.handle_key(key(KeyCode::Enter)); // pick project
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Workspace);
        app.handle_key(key(KeyCode::Enter)); // pick ws1
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Agent);
        // `j` clamps over the single available agent (`gone` is filtered).
        app.handle_key(key(KeyCode::Char('j')));
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            AppAction::CreateSession {
                workspace: WorkspacePick::Existing {
                    id: ws_id,
                    name: "ws1".into()
                },
                agent_id: AgentId::new("mock"),
            }
        );
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.wizard.is_none(), "wizard finished");
    }

    #[test]
    fn wizard_flow_new_workspace_with_name() {
        let mut app = wizard_app();
        let pid = app.projects[0].id;
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Enter)); // project
                                             // Options: ws1, "+ new workspace" — `j` moves onto the sentinel.
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::WorkspaceName);
        for c in "wt2".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter)); // name accepted → agent step
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Agent);
        let action = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            AppAction::CreateSession {
                workspace: WorkspacePick::New {
                    project_id: pid,
                    name: "wt2".into()
                },
                agent_id: AgentId::new("mock"),
            }
        );
    }

    #[test]
    fn wizard_esc_steps_back_then_aborts() {
        let mut app = wizard_app();
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Enter)); // → Workspace
        app.handle_key(key(KeyCode::Enter)); // → Agent
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Workspace);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Project);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.wizard.is_none());
    }

    #[test]
    fn wizard_name_step_edits_like_input() {
        let mut app = wizard_app();
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Enter)); // WorkspaceName step
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.wizard.as_ref().unwrap().name, "a");
        // `q` types here rather than aborting — text mode wins.
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.wizard.as_ref().unwrap().name, "aq");
        // Esc returns to the workspace list without losing the name.
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.wizard.as_ref().unwrap().step, WizardStep::Workspace);
        assert_eq!(app.wizard.as_ref().unwrap().name, "aq");
    }

    // --- diff / files panel ---------------------------------------------------

    #[test]
    fn touched_files_collects_edited_and_tool_paths() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        app.handle_event(event(
            sid,
            EventKind::FileEdited {
                path: "src/a.rs".into(),
            },
        ));
        app.handle_event(event(
            sid,
            EventKind::SessionUpdate(serde_json::json!({
                "sessionId": "mock",
                "update": {
                    "sessionUpdate": "tool_call",
                    "title": "Write",
                    "locations": [{"path": "src/b.rs"}, {"path": "src/c.rs"}]
                }
            })),
        ));
        // A repeat edit dedupes; a tool call without locations adds nothing.
        app.handle_event(event(
            sid,
            EventKind::FileEdited {
                path: "src/a.rs".into(),
            },
        ));
        app.handle_event(event(
            sid,
            EventKind::SessionUpdate(serde_json::json!({
                "update": {"sessionUpdate": "tool_call", "title": "Read"}
            })),
        ));
        assert_eq!(
            app.touched_files(),
            vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string()
            ]
        );
    }

    /// Pi `tool_execution_*` records carry the target path in `args` —
    /// a raw record (persisted pre-translation log, defensive path)
    /// still lands on the files panel.
    #[test]
    fn touched_files_collects_pi_tool_paths() {
        let mut app = app_with_sessions(&[SessionState::Ready]);
        let sid = app.sessions[0].session.id;
        app.handle_event(event(
            sid,
            EventKind::SessionUpdate(serde_json::json!({
                "type": "tool_execution_start",
                "toolCallId": "tc-1",
                "toolName": "edit",
                "args": {"path": "src/pi.rs", "oldText": "a", "newText": "b"}
            })),
        ));
        // Normalized pi output contributes its `tool_call` locations too.
        app.handle_event(event(
            sid,
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "tool_call",
                "title": "edit src/pi.rs",
                "status": "in_progress",
                "locations": [{"path": "src/pi.rs"}],
                "pi": {"type": "tool_execution_start"}
            })),
        ));
        app.handle_event(event(
            sid,
            EventKind::FileEdited {
                path: "src/pi.rs".into(),
            },
        ));
        assert_eq!(app.touched_files(), vec!["src/pi.rs".to_string()]);
    }

    /// Relay markers summarize pi passthrough records by `type` — a pi
    /// delta picks its text, lifecycle gets a label, never a JSON wall.
    #[test]
    fn event_summary_handles_pi_records() {
        let sid = SessionId::new();
        let delta = event(
            sid,
            EventKind::SessionUpdate(serde_json::json!({"type":"message_update",
                "assistantMessageEvent":{"type":"text_delta","delta":"pi reply"}})),
        );
        assert_eq!(event_summary(&delta), "message: pi reply");

        let settled = event(
            sid,
            EventKind::SessionUpdate(serde_json::json!({"type": "agent_settled"})),
        );
        assert_eq!(event_summary(&settled), "run settled");
    }

    // --- helpers -----------------------------------------------------------

    #[test]
    fn add_session_keeps_workspace_groups_together() {
        let proj = project("p");
        let a = workspace_in(proj.id, "a");
        let b = workspace_in(proj.id, "b");
        let mut app = App::new(vec![proj], vec![a.clone(), b.clone()], vec![], vec![]);
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
