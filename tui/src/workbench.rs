//! Per-session interaction state for the conversation workbench.
use std::collections::{HashMap, HashSet, VecDeque};

use agentmux_core::{Event, EventKind, SessionId, SessionState};
use unicode_segmentation::UnicodeSegmentation;

use crate::app::{App, AppAction, InputMode, PendingRelay, PermissionNotice};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum SideTab {
    #[default]
    Team,
    Files,
    Context,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum AgentFilter {
    #[default]
    All,
    Running,
    Pending,
    Unread,
}

#[derive(Default)]
pub struct SessionUi {
    pub conversation_start: u64,
    pub draft: String,
    pub cursor: usize,
    pub relays: Vec<PendingRelay>,
    pub title: Option<String>,
    pub title_seq: u64,
    pub scroll: usize,
    pub scroll_limit: std::cell::Cell<Option<usize>>,
    pub anchor: Option<u64>,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub history_draft: String,
    pub older: bool,
    pub loading: bool,
    pub history_retry_after: Option<std::time::Instant>,
}

#[derive(Clone)]
pub struct Prompt {
    pub text: String,
    pub references: Vec<PendingRelay>,
}

pub struct Workbench {
    pub info: crate::agent_info::Info,
    pub context: crate::context::Context,
    pub references: crate::references::References,
    pub transcript: crate::transcript::Transcript,
    pub external_edit: Option<crate::external::EditRequest>,
    pub files: crate::files::Files,
    pub attention: crate::attention::Attention,
    pub native_attach: Option<SessionId>,
    pub native_seed: Option<String>,
    pub event_layout: std::cell::RefCell<Option<crate::ui::EventLayout>>,
    #[cfg(test)]
    pub layout_builds: std::cell::Cell<usize>,
    pub reasoning: HashMap<SessionId, crate::reasoning::Reasoning>,
    pub reasoning_toggles: HashSet<(SessionId, u64)>,
    pub pi_commands: HashMap<SessionId, Vec<crate::commands::SlashCommand>>,
    pub acp_commands: HashMap<SessionId, (u64, Vec<crate::commands::SlashCommand>)>,
    pub pi_loaded: HashSet<SessionId>,
    pub pi_loading: HashSet<SessionId>,
    pub pi_busy: HashSet<SessionId>,
    pub pi_serial: u64,
    pub pi_panel: Option<crate::commands::PiPanel>,
    pub slash_cursor: usize,
    pub slash_dismissed: Option<String>,
    pub hits: std::cell::RefCell<Vec<crate::interaction::Hit>>,
    pub control_focus: Option<crate::interaction::Target>,
    pub menu: bool,
    pub menu_cursor: usize,
    pub interaction_epoch: u64,
    pub creating: Option<u64>,
    pub naming: Option<crate::naming::NamePanel>,
    pub renaming: HashSet<SessionId>,
    pub sessions: HashMap<SessionId, SessionUi>,
    pub cursor: usize,
    pub focus: bool,
    pub drawer: bool,
    pub tab: SideTab,
    pub context_scroll: u16,
    pub help_scroll: u16,
    pub file_cursor: usize,
    pub inspection: Option<(String, String)>,
    pub inspect_scroll: u16,
    pub diff_serial: u64,
    pub pending_diff: Option<(u64, SessionId, String)>,
    pub editor_width: std::cell::Cell<u16>,
    pub tools_expanded: bool,
    pub thoughts: bool,
    pub picker_query: String,
    pub picker_filter: AgentFilter,
    pub picker_current_space: bool,
    pub collapsed_spaces: HashSet<agentmux_core::WorkspaceId>,
    pub picker_cursor: usize,
    pub return_mode: InputMode,
    pub permissions: VecDeque<PermissionNotice>,
    pub queues: HashMap<SessionId, VecDeque<Prompt>>,
    pub in_flight: HashSet<SessionId>,
    pub resuming: HashSet<SessionId>,
    pub failed: HashMap<SessionId, Vec<Prompt>>,
    pub connected: bool,
    pub columns: u16,
    pub permission_scroll: u16,
    pub quitting: bool,
}

impl Default for Workbench {
    fn default() -> Self {
        Self {
            info: Default::default(),
            context: Default::default(),
            references: Default::default(),
            transcript: Default::default(),
            external_edit: None,
            files: Default::default(),
            attention: Default::default(),
            native_attach: None,
            native_seed: None,
            event_layout: Default::default(),
            #[cfg(test)]
            layout_builds: Default::default(),
            reasoning: HashMap::new(),
            reasoning_toggles: HashSet::new(),
            pi_commands: Default::default(),
            acp_commands: Default::default(),
            pi_loaded: Default::default(),
            pi_loading: Default::default(),
            pi_busy: Default::default(),
            pi_serial: 0,
            pi_panel: None,
            slash_cursor: 0,
            slash_dismissed: None,
            hits: Default::default(),
            control_focus: None,
            menu: false,
            menu_cursor: 0,
            interaction_epoch: 0,
            creating: None,
            naming: None,
            renaming: HashSet::new(),
            sessions: HashMap::new(),
            cursor: 0,
            focus: false,
            drawer: false,
            tab: SideTab::Team,
            context_scroll: 0,
            help_scroll: 0,
            file_cursor: 0,
            inspection: None,
            inspect_scroll: 0,
            diff_serial: 0,
            pending_diff: None,
            editor_width: std::cell::Cell::new(80),
            tools_expanded: false,
            thoughts: true,
            picker_query: String::new(),
            picker_filter: AgentFilter::All,
            picker_current_space: false,
            collapsed_spaces: HashSet::new(),
            picker_cursor: 0,
            return_mode: InputMode::Editing,
            permissions: VecDeque::new(),
            queues: HashMap::new(),
            in_flight: HashSet::new(),
            resuming: HashSet::new(),
            failed: HashMap::new(),
            connected: true,
            columns: 120,
            permission_scroll: 0,
            quitting: false,
        }
    }
}

impl App {
    pub fn start_conversation(&mut self, id: SessionId, seq: u64) {
        let ui = self.wb.sessions.entry(id).or_default();
        if seq <= ui.conversation_start {
            return;
        }
        ui.conversation_start = seq;
        ui.title = None;
        ui.title_seq = 0;
        ui.scroll = 0;
        ui.anchor = None;
        ui.scroll_limit.set(None);
        ui.history.clear();
        ui.history_cursor = None;
        ui.history_draft.clear();
        ui.older = false;
        self.wb.attention.reset(id, seq);
        self.wb.info.sessions.remove(&id);
        self.wb
            .transcript
            .tools
            .retain(|(session, _), _| *session != id);
        if self
            .wb
            .acp_commands
            .get(&id)
            .is_some_and(|(catalog_seq, _)| *catalog_seq < seq)
        {
            self.wb.acp_commands.remove(&id);
        }
    }

    pub fn session_title(&self, id: SessionId) -> String {
        self.wb
            .sessions
            .get(&id)
            .and_then(|s| s.title.clone())
            .unwrap_or_else(|| self.agent_instance(id))
    }

    pub fn agent_instance(&self, id: SessionId) -> String {
        let view = self.sessions.iter().find(|s| s.session.id == id);
        let ordinal = self
            .sessions
            .iter()
            .filter(|v| {
                view.is_some_and(|current| {
                    v.session.workspace_id == current.session.workspace_id
                        && v.session.agent_id == current.session.agent_id
                })
            })
            .position(|v| v.session.id == id)
            .map(|i| i + 1)
            .unwrap_or(1);
        format!(
            "{} #{ordinal}",
            view.map(|v| v.agent_name.as_str()).unwrap_or("Agent")
        )
    }

    pub fn set_title_from_prompt(&mut self, id: SessionId, text: &str) {
        let ui = self.wb.sessions.entry(id).or_default();
        if ui.title.is_none() {
            ui.title = Some(
                text.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("New conversation")
                    .chars()
                    .take(60)
                    .collect(),
            );
        }
    }

    pub fn apply_title(&mut self, id: SessionId, title: String, seq: u64) {
        let ui = self.wb.sessions.entry(id).or_default();
        if seq > ui.conversation_start && seq >= ui.title_seq {
            ui.title = Some(title);
            ui.title_seq = seq;
        }
    }

    pub fn merge_history_page(
        &mut self,
        id: SessionId,
        page: agentmux_core::rpc::SessionHistoryResult,
    ) {
        if let Some(event) = &page.available_commands {
            crate::commands::observe_acp_commands(self, event);
        }
        let title = page.title.clone();
        self.merge_history(
            id,
            page.events,
            page.has_more,
            page.title,
            page.conversation_start,
        );
        if let Some(title) = title {
            self.apply_title(id, title, page.title_seq);
        }
    }

    pub fn select_session(&mut self, index: usize) {
        self.wb.interaction_epoch += 1;
        self.wb.control_focus = None;
        self.wb.pending_diff = None;
        if index >= self.sessions.len() {
            return;
        }
        if let Some(id) = self.selected_session_id() {
            let ui = self.wb.sessions.entry(id).or_default();
            ui.draft = std::mem::take(&mut self.input);
            ui.cursor = self.wb.cursor;
            ui.relays = std::mem::take(&mut self.pending_relays);
        }
        self.selected = index;
        self.wb
            .collapsed_spaces
            .remove(&self.sessions[index].session.workspace_id);
        self.wb.pi_panel = None;
        self.wb.slash_cursor = 0;
        self.wb.slash_dismissed = None;
        let id = self.sessions[index].session.id;
        let ui = self.wb.sessions.entry(id).or_default();
        self.input = std::mem::take(&mut ui.draft);
        self.wb.cursor = ui.cursor.min(self.input.len());
        self.pending_relays = std::mem::take(&mut ui.relays);
        self.wb.inspection = None;
        self.wb.files.diff_params = None;
        self.wb.files.searching = false;
        self.wb.file_cursor = 0;
        self.show_diff = false;
        self.mark_selected_viewed();
    }

    pub fn insert_text(&mut self, text: &str) {
        self.clamp_cursor();
        // Bracketed paste is a single edit, including all its newlines.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        self.input.insert_str(self.wb.cursor, &text);
        self.wb.cursor += text.len();
    }

    pub fn clamp_cursor(&mut self) {
        self.wb.cursor = self.wb.cursor.min(self.input.len());
        while !self.input.is_char_boundary(self.wb.cursor) {
            self.wb.cursor -= 1;
        }
    }

    pub fn move_cursor(&mut self, right: bool) {
        self.clamp_cursor();
        self.wb.cursor = if right {
            self.input[self.wb.cursor..]
                .graphemes(true)
                .next()
                .map(|g| self.wb.cursor + g.len())
                .unwrap_or(self.input.len())
        } else {
            self.input[..self.wb.cursor]
                .grapheme_indices(true)
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
    }

    pub fn erase(&mut self, backwards: bool) {
        self.clamp_cursor();
        let old = self.wb.cursor;
        self.move_cursor(!backwards);
        let new = self.wb.cursor;
        self.input.replace_range(old.min(new)..old.max(new), "");
        self.wb.cursor = old.min(new);
    }

    pub fn line_edge(&mut self, end: bool) {
        self.clamp_cursor();
        self.wb.cursor = if end {
            self.input[self.wb.cursor..]
                .find('\n')
                .map(|i| self.wb.cursor + i)
                .unwrap_or(self.input.len())
        } else {
            self.input[..self.wb.cursor]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        };
    }

    pub fn vertical_cursor(&mut self, down: bool) {
        self.clamp_cursor();
        let layout = crate::editor::EditorLayout::new(&self.input, self.wb.editor_width.get());
        let (row, col) = layout.position(self.wb.cursor);
        let target = if down { row + 1 } else { row.saturating_sub(1) };
        if target < layout.lines.len() {
            self.wb.cursor = layout.cursor_at(target, col);
        }
    }

    pub fn begin_diff(&mut self, path: String) -> u64 {
        self.wb.diff_serial += 1;
        if let Some(id) = self.selected_session_id() {
            self.wb.pending_diff = Some((self.wb.diff_serial, id, path));
        }
        self.wb.diff_serial
    }

    fn edit_recovered(&mut self, id: SessionId, prompt: Prompt) {
        let previous = Prompt {
            text: std::mem::replace(&mut self.input, prompt.text),
            references: std::mem::replace(&mut self.pending_relays, prompt.references),
        };
        if !previous.text.is_empty() || !previous.references.is_empty() {
            self.wb.failed.entry(id).or_default().push(previous);
            self.set_status(
                "Message restored. Previous draft saved under Menu → Recover failed message.",
            );
        }
        self.wb.cursor = self.input.len();
        self.mode = InputMode::Editing;
    }

    pub fn input_history(&mut self, older: bool) {
        let Some(id) = self.selected_session_id() else {
            return;
        };
        let ui = self.wb.sessions.entry(id).or_default();
        if ui.history.is_empty() {
            return;
        }
        if older {
            let index = match ui.history_cursor {
                Some(i) => i.saturating_sub(1),
                None => {
                    ui.history_draft = self.input.clone();
                    ui.history.len() - 1
                }
            };
            ui.history_cursor = Some(index);
            self.input = ui.history[index].clone();
        } else if let Some(i) = ui.history_cursor {
            if i + 1 < ui.history.len() {
                ui.history_cursor = Some(i + 1);
                self.input = ui.history[i + 1].clone();
            } else {
                ui.history_cursor = None;
                self.input = ui.history_draft.clone();
            }
        }
        self.wb.cursor = self.input.len();
    }

    /// Fetch only near the oldest loaded row, not on every upward wheel tick.
    /// A missing limit means the local layout still has older rows to visit.
    pub fn needs_older_history(&self) -> bool {
        self.wb.inspection.is_none()
            && self
                .selected_session_id()
                .and_then(|id| self.wb.sessions.get(&id))
                .is_some_and(|ui| {
                    ui.anchor.is_some()
                        && ui.older
                        && !ui.loading
                        && ui
                            .history_retry_after
                            .is_none_or(|at| std::time::Instant::now() >= at)
                        && ui
                            .scroll_limit
                            .get()
                            .is_some_and(|limit| ui.scroll.saturating_add(24) >= limit)
                })
    }

    pub fn scroll_by(&mut self, older: bool, rows: usize) {
        if self.wb.inspection.is_some() {
            self.wb.inspect_scroll = if older {
                self.wb.inspect_scroll.saturating_sub(rows as u16)
            } else {
                self.wb.inspect_scroll.saturating_add(rows as u16)
            };
            return;
        }
        let Some(id) = self.selected_session_id() else {
            return;
        };
        let latest = self
            .session_events()
            .next_back()
            .map(|e| e.seq)
            .unwrap_or(0);
        let ui = self.wb.sessions.entry(id).or_default();
        ui.scroll = ui.scroll.min(ui.scroll_limit.get().unwrap_or(usize::MAX));
        if older {
            ui.anchor.get_or_insert(latest);
            ui.scroll = ui.scroll.saturating_add(rows);
        } else {
            ui.scroll = ui.scroll.saturating_sub(rows);
            if ui.scroll == 0 {
                ui.anchor = None;
            }
        }
    }

    pub fn follow_latest(&mut self) {
        if let Some(id) = self.selected_session_id() {
            let ui = self.wb.sessions.entry(id).or_default();
            ui.scroll = 0;
            ui.anchor = None;
            self.wb.attention.viewed(id);
        }
    }

    pub fn sidebar_visible(&self) -> bool {
        (self.wb.columns >= 110 && !self.wb.focus)
            || self.wb.drawer
            || self.mode == InputMode::Sidebar
            || (self.mode == InputMode::RelayPick
                && self
                    .relay
                    .as_ref()
                    .is_some_and(|p| p.stage == crate::app::RelayStage::Session))
    }

    pub fn close_sidebar(&mut self) {
        self.wb.focus = true;
        self.wb.drawer = false;
        self.wb.control_focus = None;
        self.relay = None;
        self.mode = InputMode::Editing;
    }

    pub fn toggle_sidebar(&mut self) {
        if self.sidebar_visible() {
            self.close_sidebar();
        } else {
            self.wb.focus = false;
            self.wb.drawer = self.wb.columns < 110;
            self.mode = InputMode::Sidebar;
        }
    }

    pub fn open_permissions(&mut self) {
        self.wb.interaction_epoch += 1;
        if let Some(p) = self.permission.as_mut() {
            if self.mode != InputMode::Permission {
                p.resume = self.mode;
            }
            self.mode = InputMode::Permission;
            self.wb.permission_scroll = 0;
        } else {
            self.set_status("no pending permission requests");
        }
    }

    pub fn permission_count(&self) -> usize {
        usize::from(self.permission.is_some()) + self.wb.permissions.len()
    }

    pub fn open_picker(&mut self) {
        self.wb.return_mode = self.mode;
        self.wb.picker_query.clear();
        self.wb.picker_cursor = 0;
        self.mode = InputMode::TaskPicker;
    }

    pub fn picker_matches(&self) -> Vec<usize> {
        use nucleo_matcher::{
            pattern::{AtomKind, CaseMatching, Normalization, Pattern},
            Config, Matcher, Utf32Str,
        };
        let pattern = Pattern::new(
            &self.wb.picker_query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buffer = vec![];
        let current = self.selected_session().map(|v| v.session.workspace_id);
        let pending: HashSet<_> = self
            .attention_items()
            .iter()
            .filter_map(|item| item.session)
            .collect();
        let mut matches: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, view)| {
                if self.wb.picker_current_space && Some(view.session.workspace_id) != current {
                    return None;
                }
                let allowed = match self.wb.picker_filter {
                    AgentFilter::All => true,
                    AgentFilter::Running => matches!(
                        view.session.state,
                        SessionState::Connecting
                            | SessionState::Prompting
                            | SessionState::WaitingPermission
                    ),
                    AgentFilter::Pending => pending.contains(&view.session.id),
                    AgentFilter::Unread => self.unread.contains(&view.session.id),
                };
                if !allowed {
                    return None;
                }
                let text = format!(
                    "{} {} {}",
                    self.session_title(view.session.id),
                    view.agent_name,
                    view.workspace_name
                );
                pattern
                    .score(Utf32Str::new(&text, &mut buffer), &mut matcher)
                    .map(|score| {
                        (
                            index,
                            score,
                            !text
                                .to_lowercase()
                                .contains(&self.wb.picker_query.to_lowercase()),
                        )
                    })
            })
            .collect();
        matches.sort_by_key(|(index, score, fuzzy_only)| {
            (
                *fuzzy_only,
                Some(self.sessions[*index].session.workspace_id) != current,
                std::cmp::Reverse(*score),
                *index,
            )
        });
        matches.into_iter().map(|(index, _, _)| index).collect()
    }

    pub fn set_picker_filter(&mut self, filter: AgentFilter) {
        self.wb.picker_filter = filter;
        self.wb.picker_cursor = 0;
    }

    pub fn command(&mut self, command: &str) -> AppAction {
        self.wb.pending_diff = None;
        let (name, arg) = command.split_once(' ').unwrap_or((command, ""));
        if name != "/quit" {
            self.wb.quitting = false;
        }
        match name {
            "/terminal" => return AppAction::AttachNative,
            "/history" => return AppAction::BrowseNative,
            "/native" => {
                if let Some(session_id) = self.selected_session_id() {
                    return AppAction::OpenNative {
                        session_id,
                        session_file: None,
                        native: true,
                        history: false,
                    };
                }
            }
            "/quit" => {
                let queued: usize = self.wb.queues.values().map(|q| q.len()).sum();
                if queued > 0 && !self.wb.quitting {
                    self.wb.quitting = true;
                    self.set_status(format!("{queued} prompts are queued in this TUI; choose Menu → Exit again to discard them, agents keep running"));
                } else {
                    return AppAction::Quit;
                }
            }
            "/close-wizard" => {
                self.wb.interaction_epoch += 1;
                if self.wizard.as_ref().is_some_and(|w| w.submitting) {
                    self.set_status("Agent creation continues in the background.");
                }
                self.wizard = None;
                self.mode = InputMode::Editing;
            }
            "/newline" => {
                self.mode = InputMode::Editing;
                self.insert_text("\n");
            }
            "/chat" => {
                self.wb.inspection = None;
                self.show_diff = false;
                self.mode = InputMode::Editing;
            }
            "/new" => self.start_wizard(),
            "/add-agent" => self.start_add_agent(),
            "/rename" => crate::naming::open(self),
            "/close-name" => self.wb.naming = None,
            "/project" if !arg.trim().is_empty() => {
                return AppAction::RegisterProject(arg.trim().into())
            }
            "/tasks" => self.open_picker(),
            "/info" => crate::agent_info::open(self),
            "/refresh-info" => return AppAction::RefreshInfo,
            "/search" | "/messages" => crate::transcript::open(self),
            "/previous-message" => crate::transcript::user_message(self, false),
            "/next-message" => crate::transcript::user_message(self, true),
            "/edit-draft" => {
                if let Some(session) = self.selected_session_id() {
                    self.wb.external_edit = Some(crate::external::EditRequest::Draft {
                        session,
                        text: self.input.clone(),
                    });
                }
            }
            "/focus" | "/sidebar" => self.toggle_sidebar(),
            "/hide-sidebar" => self.close_sidebar(),
            "/files" => {
                self.wb.tab = SideTab::Files;
                self.wb.drawer = true;
                self.mode = InputMode::Sidebar;
            }
            "/context" => {
                if self.wb.columns < 110 {
                    crate::context::open(self);
                }
                self.wb.tab = SideTab::Context;
                self.wb.drawer = true;
                self.mode = InputMode::Sidebar;
            }
            "/refresh-context" => return AppAction::RefreshContext,
            "/context-preview" => crate::context::open(self),
            "/edit-context" => return crate::context::edit(self),
            "/save-context" => return crate::context::save(self),
            "/references" => crate::references::open(self),
            "/remove-reference" => crate::references::remove(self),
            "/file-search" => {
                self.wb.files.searching = true;
                self.mode = InputMode::Sidebar;
                self.wb.control_focus = None;
            }
            "/refresh-files" => return AppAction::RefreshFiles,
            "/permissions" => self.open_permissions(),
            "/attention" => crate::attention::open(self),
            "/error" => crate::attention::error_details(self),
            "/relay" => self.start_relay(),
            "/cancel" => return AppAction::CancelPrompt,
            "/resume" => return AppAction::ResumeSession,
            "/kill" => return AppAction::KillSession,
            "/latest" => self.follow_latest(),
            "/tools" => self.wb.tools_expanded = !self.wb.tools_expanded,
            "/thinking" => {
                self.wb.thoughts = !self.wb.thoughts;
                self.wb.reasoning_toggles.clear();
                for ui in self.wb.sessions.values_mut() {
                    ui.scroll_limit.set(None);
                }
            }
            "/unqueue" => {
                if let Some(id) = self.selected_session_id() {
                    if let Some(p) = self.wb.queues.entry(id).or_default().pop_back() {
                        self.edit_recovered(id, p);
                    }
                }
            }
            "/recover" => {
                if let Some(id) = self.selected_session_id() {
                    if let Some(p) = self.wb.failed.entry(id).or_default().pop() {
                        self.edit_recovered(id, p);
                    }
                }
            }
            "/help" | "/" => {
                self.wb.help_scroll = 0;
                self.wb.return_mode = self.mode;
                self.mode = InputMode::Help;
            }
            _ => self.set_status("unknown command — /help lists commands"),
        }
        AppAction::None
    }

    pub fn merge_history(
        &mut self,
        id: SessionId,
        events: Vec<Event>,
        older: bool,
        title: Option<String>,
        conversation_start: u64,
    ) {
        let boundary = events
            .iter()
            .filter(|e| e.resets_conversation_view())
            .map(|e| e.seq)
            .max()
            .unwrap_or(0)
            .max(conversation_start);
        self.start_conversation(id, boundary);
        let ui = self.wb.sessions.entry(id).or_default();
        ui.loading = false;
        ui.history_retry_after = None;
        ui.scroll_limit.set(None);
        ui.older = older && events.iter().all(|e| e.seq > ui.conversation_start);
        if let Some(title) = title.filter(|_| conversation_start >= ui.conversation_start) {
            ui.title.get_or_insert(title);
        }
        let mut seen: HashSet<_> = self
            .events
            .iter()
            .filter(|e| e.session_id == id)
            .map(|e| e.seq)
            .collect();
        for event in events {
            if matches!(event.kind, EventKind::StateChanged { .. }) {
                self.wb.attention.baseline(id, event.seq);
            }
            crate::commands::observe_acp_commands(self, &event);
            if let EventKind::TitleChanged { title } = &event.kind {
                self.apply_title(id, title.clone(), event.seq);
            }
            if seen.insert(event.seq) {
                self.events.push(event);
            }
        }
        self.events.sort_by_key(|e| (e.ts, e.seq));
        let mut reasoning = crate::reasoning::Reasoning::default();
        let boundary = self.wb.sessions[&id].conversation_start;
        let mut events: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.session_id == id && e.seq >= boundary)
            .collect();
        events.sort_by_key(|e| e.seq);
        for event in events {
            reasoning.observe(event);
        }
        self.wb.reasoning.insert(id, reasoning);
        self.wb
            .transcript
            .tools
            .retain(|(session, _), _| *session != id);
        let mut tools: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.session_id == id && event.seq > boundary)
            .cloned()
            .collect();
        tools.sort_by_key(|event| event.seq);
        for event in tools {
            crate::transcript::observe(self, &event);
        }
        let mut metadata: Vec<_> = self
            .events
            .iter()
            .filter(|event| event.session_id == id && event.seq > boundary)
            .cloned()
            .collect();
        metadata.sort_by_key(|event| event.seq);
        for event in metadata {
            crate::agent_info::observe(self, &event);
        }
        // Repair can change reply/thought grouping without adding any events
        // (e.g. already-received deltas following an out-of-order state update).
        // Event count/sequence alone cannot identify that projection change.
        if self.selected_session_id() == Some(id) {
            self.wb.event_layout.get_mut().take();
        }
    }

    pub fn record_prompt(&mut self, id: SessionId, text: &str) {
        self.set_title_from_prompt(id, text);
        let ui = self.wb.sessions.entry(id).or_default();
        ui.history.push(text.into());
        ui.history_cursor = None;
    }

    pub fn restore_prompt(&mut self, id: SessionId, prompt: Prompt) {
        if self.selected_session_id() == Some(id) && self.input.is_empty() {
            self.input = prompt.text;
            self.pending_relays = prompt.references;
            self.wb.cursor = self.input.len();
        } else {
            // Never overwrite a newer draft, including one in another session.
            self.wb.failed.entry(id).or_default().push(prompt);
        }
    }

    pub fn ready_queued(&mut self) -> Option<(SessionId, Prompt)> {
        if !self.wb.connected {
            return None;
        }
        for view in &self.sessions {
            let id = view.session.id;
            if view.session.state == SessionState::Ready
                && !self.wb.in_flight.contains(&id)
                && !self.wb.resuming.contains(&id)
                && !self.wb.pi_busy.contains(&id)
            {
                if let Some(prompt) = self.wb.queues.entry(id).or_default().pop_front() {
                    self.wb.in_flight.insert(id);
                    let state = self.wb.reasoning.entry(id).or_default();
                    state.started = Some(chrono::Utc::now());
                    state.phase = "Waiting";
                    return Some((id, prompt));
                }
            }
        }
        None
    }
}

pub fn prompt_text(event: &Event) -> Option<&str> {
    if let EventKind::SessionUpdate(v) = &event.kind {
        let u = v.get("update").unwrap_or(v);
        if u.get("sessionUpdate").and_then(|v| v.as_str()) == Some("user_message_chunk") {
            return u.pointer("/content/text").and_then(|v| v.as_str());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{RelayPick, RelaySource, RelayStage, SessionView};
    use agentmux_core::{AgentId, Session, WorkspaceId};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn live_name_wins_over_late_history_and_resets_with_conversation() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        app.apply_title(id, "tests".into(), 20);
        app.apply_title(id, "stale".into(), 10);
        assert_eq!(app.session_title(id), "tests");
        app.merge_history(id, vec![], false, Some("first prompt".into()), 0);
        assert_eq!(app.session_title(id), "tests");
        app.start_conversation(id, 30);
        app.apply_title(id, "old name".into(), 20);
        assert_ne!(app.session_title(id), "old name");
        app.apply_title(id, "new tests".into(), 31);
        assert_eq!(app.session_title(id), "new tests");
        assert_ne!(app.session_title(app.sessions[1].session.id), "new tests");
    }

    #[test]
    fn same_profile_instances_have_distinct_readable_default_names() {
        let app = app();
        let first = app.session_title(app.sessions[0].session.id);
        let second = app.session_title(app.sessions[1].session.id);
        assert_ne!(first, second);
        assert!(first.ends_with("1"));
        assert!(second.ends_with("2"));
    }

    #[test]
    fn fuzzy_agent_search_filters_and_folding_keep_recipient_and_drafts() {
        let mut app = app();
        let base = app.sessions[0].clone();
        for index in 2..25 {
            let mut view = base.clone();
            view.session.id = SessionId::new();
            if index >= 15 {
                view.session.workspace_id = WorkspaceId::new();
                view.workspace_name = "other".into();
            }
            app.sessions.push(view);
        }
        let target = app.sessions[12].session.id;
        app.apply_title(target, "Regression tests 中文检查".into(), 10);
        app.insert_text("current draft");
        let selected = app.selected_session_id();
        app.open_picker();
        app.wb.picker_query = "rgts".into();
        assert!(app.picker_matches().contains(&12));
        assert_eq!(app.selected_session_id(), selected);
        assert_eq!(app.input, "current draft");
        app.wb.picker_query.clear();
        app.sessions[12].session.state = SessionState::Prompting;
        app.set_picker_filter(AgentFilter::Running);
        assert_eq!(app.picker_matches(), vec![12]);
        app.unread.insert(target);
        app.set_picker_filter(AgentFilter::Unread);
        assert_eq!(app.picker_matches(), vec![12]);
        app.set_error_for(Some(target), "failure");
        app.set_picker_filter(AgentFilter::Pending);
        assert_eq!(app.picker_matches(), vec![12]);
        app.set_picker_filter(AgentFilter::All);
        app.wb.picker_current_space = true;
        assert_eq!(app.picker_matches().len(), 15);
        app.wb.picker_query = "中文".into();
        assert_eq!(app.picker_matches(), vec![12]);
        let workspace = app.sessions[12].session.workspace_id;
        app.wb.collapsed_spaces.insert(workspace);
        app.select_session(12);
        assert!(!app.wb.collapsed_spaces.contains(&workspace));
        assert_eq!(app.wb.sessions[&selected.unwrap()].draft, "current draft");
    }

    #[test]
    fn exact_other_space_title_outranks_weak_current_space_match() {
        let mut app = app();
        let first = app.sessions[0].session.id;
        let second = app.sessions[1].session.id;
        app.sessions[0].workspace_name = "nav40".into();
        app.sessions[1].session.workspace_id = WorkspaceId::new();
        app.sessions[1].workspace_name = "nav80".into();
        app.apply_title(first, "Task 40 8".into(), 1);
        app.apply_title(second, "Task 80 0".into(), 1);
        app.wb.picker_query = "Task 80 0".into();
        assert_eq!(app.picker_matches()[0], 1);
        assert_eq!(app.selected_session_id(), Some(first));
    }

    fn app() -> App {
        let ws = WorkspaceId::new();
        App::new(
            vec![],
            vec![],
            (0..2)
                .map(|_| SessionView {
                    session: Session {
                        id: SessionId::new(),
                        workspace_id: ws,
                        agent_id: AgentId::new("mock"),
                        state: SessionState::Ready,
                        acp_session_id: None,
                        native_session_file: None,
                        native_terminal: false,
                        references: vec![],
                        created_at: chrono::Utc::now(),
                    },
                    agent_name: "Mock".into(),
                    workspace_name: "shared".into(),
                })
                .collect(),
            vec![],
        )
    }
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    #[test]
    fn default_editor_treats_navigation_letters_as_text() {
        let mut app = app();
        assert_eq!(app.mode, InputMode::Editing);
        for c in "nqxjki@".chars() {
            assert_eq!(app.handle_key(key(KeyCode::Char(c))), AppAction::None);
        }
        assert_eq!(app.input, "nqxjki@");
        assert!(app.wizard.is_none());
    }
    #[test]
    fn unicode_graphemes_and_multiline_paste_edit_without_sending() {
        let mut app = app();
        app.insert_text("中👩‍💻文\r\nnext");
        app.line_edge(false);
        app.move_cursor(false);
        app.erase(true);
        assert_eq!(app.input, "中👩‍💻\nnext");
        app.erase(true);
        assert_eq!(app.input, "中\nnext");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(app.input, "中\n\nnext");
    }
    #[test]
    fn session_switch_restores_draft_cursor_and_relay_target() {
        let mut app = app();
        let a = app.sessions[0].session.id;
        let b = app.sessions[1].session.id;
        app.insert_text("alpha");
        app.move_cursor(false);
        app.pending_relays.push(PendingRelay {
            source: b,
            seq: 7,
            target: a,
        });
        app.select_session(1);
        app.insert_text("beta");
        app.select_session(0);
        assert_eq!(app.input, "alpha");
        assert_eq!(app.wb.cursor, 4);
        assert_eq!(app.pending_relays[0].target, a);
        app.select_session(1);
        assert_eq!(app.input, "beta");
        assert!(app.pending_relays.is_empty());
    }
    #[test]
    fn relay_preserves_source_draft_and_appends_to_target_draft() {
        let mut app = app();
        let source = app.sessions[0].session.id;
        app.insert_text("source draft");
        app.select_session(1);
        app.insert_text("target draft");
        app.select_session(0);
        app.finish_relay(RelayPick {
            stage: RelayStage::Session,
            event_cursor: 0,
            session_cursor: 1,
            source: Some(RelaySource {
                session_id: source,
                seq: 9,
                summary: "result".into(),
            }),
        });
        assert!(app.input.starts_with("target draft [@"));
        app.select_session(0);
        assert_eq!(app.input, "source draft");
        assert!(app.pending_relays.is_empty());
    }
    #[test]
    fn simultaneous_permissions_do_not_steal_focus_or_overwrite_each_other() {
        let mut app = app();
        let a = app.sessions[0].session.id;
        let b = app.sessions[1].session.id;
        for id in [a, b] {
            app.handle_event(Event {
                session_id: id,
                seq: 1,
                ts: chrono::Utc::now(),
                kind: EventKind::PermissionRequest {
                    request_id: "same-id".into(),
                    request: serde_json::json!({"toolCall":{"title":"test"}}),
                },
            });
        }
        assert_eq!(app.permission_count(), 2);
        assert_eq!(app.mode, InputMode::Editing);
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.input, "y");
        assert!(!app.permission.as_ref().unwrap().pending);
        app.handle_event(Event {
            session_id: b,
            seq: 2,
            ts: chrono::Utc::now(),
            kind: EventKind::PermissionResolved {
                request_id: "same-id".into(),
                outcome: "cancelled".into(),
            },
        });
        assert_eq!(app.permission_count(), 1);
        assert_eq!(app.permission.as_ref().unwrap().session_id, a);
        app.open_permissions();
        assert_eq!(app.handle_key(key(KeyCode::Esc)), AppAction::None);
        assert_eq!(app.mode, InputMode::Editing);
        assert_eq!(app.permission_count(), 1);
    }
    #[test]
    fn queue_waits_for_turn_completion_and_retains_target() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        app.wb.queues.entry(id).or_default().extend([
            Prompt {
                text: "first".into(),
                references: vec![],
            },
            Prompt {
                text: "second".into(),
                references: vec![],
            },
        ]);
        app.select_session(1);
        let (target, p) = app.ready_queued().unwrap();
        assert_eq!(target, id);
        assert_eq!(p.text, "first");
        assert!(app.ready_queued().is_none());
        app.wb.in_flight.remove(&id);
        assert_eq!(app.ready_queued().unwrap().1.text, "second");
    }
    #[test]
    fn failed_send_never_overwrites_a_newer_draft() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        app.insert_text("new draft");
        app.restore_prompt(
            id,
            Prompt {
                text: "failed".into(),
                references: vec![],
            },
        );
        assert_eq!(app.input, "new draft");
        assert_eq!(app.wb.failed[&id][0].text, "failed");
    }
    #[test]
    fn fresh_conversation_hides_old_chat_and_resets_view_without_losing_draft() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        let event = |seq, kind| Event {
            session_id: id,
            seq,
            ts: chrono::Utc::now(),
            kind,
        };
        let old = event(1, EventKind::Orchestrator("old response".into()));
        app.handle_event(old.clone());
        app.record_prompt(id, "old title");
        app.insert_text("draft to keep");
        app.wb.sessions.get_mut(&id).unwrap().scroll = 40;
        app.wb.sessions.get_mut(&id).unwrap().anchor = Some(1);
        app.handle_event(event(5, EventKind::ConversationStarted));
        assert_eq!(app.events_for_selected().count(), 0);
        assert_eq!(app.input, "draft to keep");
        let ui = &app.wb.sessions[&id];
        assert!(ui.title.is_none());
        assert!(ui.history.is_empty());
        assert_eq!(ui.scroll, 0);
        assert_eq!(ui.anchor, None);
        // A page requested before resume must not restore the old title/chat.
        app.merge_history(id, vec![old], true, Some("old title".into()), 0);
        assert_eq!(app.events_for_selected().count(), 0);
        assert!(app.wb.sessions[&id].title.is_none());
        assert!(!app.wb.sessions[&id].older);
        assert!(
            app.events.iter().any(|e| e.seq == 1),
            "archive stays intact"
        );
        app.handle_event(event(
            6,
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate":"user_message_chunk", "content":{"text":"new title"}
            })),
        ));
        assert_eq!(app.session_title(id), "new title");
        assert_eq!(app.events_for_selected().count(), 1);
    }

    #[test]
    fn daemon_restart_preserves_chat_and_title_live_and_on_reopen() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        let old = Event {
            session_id: id,
            seq: 1,
            ts: chrono::Utc::now(),
            kind: EventKind::Orchestrator("old chat".into()),
        };
        let restart = Event {
            session_id: id,
            seq: 2,
            ts: chrono::Utc::now(),
            kind: EventKind::StateChanged {
                from: SessionState::Ready,
                to: SessionState::Error("daemon restarted".into()),
            },
        };
        app.handle_event(old.clone());
        app.record_prompt(id, "old title");
        app.handle_event(restart.clone());
        assert_eq!(app.events_for_selected().count(), 2);
        assert_eq!(app.session_title(id), "old title");
        assert!(matches!(
            app.sessions[0].session.state,
            SessionState::Error(_)
        ));
        // A new TUI must not treat a disconnected process as a new conversation.
        app.wb.sessions.clear();
        app.events.clear();
        app.merge_history(id, vec![old, restart], false, Some("old title".into()), 0);
        assert_eq!(app.events_for_selected().count(), 2);
        assert_eq!(app.session_title(id), "old title");
        assert_eq!(app.wb.sessions[&id].conversation_start, 0);
    }

    #[test]
    fn history_boundary_survives_reopen_and_older_page_loading() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        let ts = chrono::Utc::now();
        let event = |seq| Event {
            session_id: id,
            seq,
            ts,
            kind: EventKind::Orchestrator(format!("message {seq}")),
        };
        // Latest page doesn't contain the marker; metadata must still fence it.
        app.merge_history(id, vec![event(250)], true, Some("new title".into()), 100);
        app.merge_history(
            id,
            vec![event(1), event(101)],
            true,
            Some("new title".into()),
            100,
        );
        assert_eq!(
            app.events_for_selected().map(|e| e.seq).collect::<Vec<_>>(),
            vec![101, 250]
        );
        assert_eq!(app.session_title(id), "new title");
        assert!(!app.wb.sessions[&id].older);
    }

    #[test]
    fn failed_resume_does_not_clear_conversation_or_other_sessions() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        let other = app.sessions[1].session.id;
        app.record_prompt(id, "keep this title");
        app.record_prompt(other, "other title");
        for (seq, from, to) in [
            (
                1,
                SessionState::Error("stopped".into()),
                SessionState::Connecting,
            ),
            (
                2,
                SessionState::Connecting,
                SessionState::Error("failed".into()),
            ),
        ] {
            app.handle_event(Event {
                session_id: id,
                seq,
                ts: chrono::Utc::now(),
                kind: EventKind::StateChanged { from, to },
            });
        }
        assert_eq!(app.wb.sessions[&id].conversation_start, 0);
        assert_eq!(app.session_title(id), "keep this title");
        app.start_conversation(id, 3);
        assert_eq!(app.session_title(other), "other title");
        assert_eq!(app.wb.sessions[&other].conversation_start, 0);
    }

    #[test]
    fn legacy_resume_notes_also_start_a_new_conversation() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        app.merge_history(id, vec![Event {
            session_id: id, seq: 20, ts: chrono::Utc::now(),
            kind: EventKind::Orchestrator("session resumed with a fresh adapter session (session/load unsupported); prior history persists in the event log".into()),
        }], false, Some("old title".into()), 0);
        assert_eq!(app.wb.sessions[&id].conversation_start, 20);
        assert_eq!(app.events_for_selected().count(), 0);
        assert!(app.wb.sessions[&id].title.is_none());
    }

    #[test]
    fn history_merge_is_deduplicated_without_replaying_old_state_transitions() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        let e = Event {
            session_id: id,
            seq: 3,
            ts: chrono::Utc::now(),
            kind: EventKind::StateChanged {
                from: SessionState::Connecting,
                to: SessionState::Error("old".into()),
            },
        };
        app.merge_history(id, vec![e.clone()], true, Some("task".into()), 0);
        app.merge_history(id, vec![e], false, None, 0);
        assert_eq!(app.events.len(), 1);
        assert_eq!(app.sessions[0].session.state, SessionState::Ready);
        assert_eq!(app.session_title(id), "task");
    }
    #[test]
    fn cancelling_a_turn_does_not_discard_queued_prompts() {
        let mut app = app();
        let id = app.sessions[0].session.id;
        app.wb.queues.entry(id).or_default().push_back(Prompt {
            text: "next task".into(),
            references: vec![],
        });
        assert_eq!(app.command("/cancel"), AppAction::CancelPrompt);
        assert_eq!(app.wb.queues[&id].front().unwrap().text, "next task");
    }
}
