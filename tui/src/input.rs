//! Key → action mapping, one handler per [`InputMode`](crate::app::InputMode).
//!
//! Pure functions over `&mut App` — they mutate only UI state (mode,
//! selection, input buffer) and return an [`AppAction`] for the async
//! layer in `main.rs`. No terminal or daemon access here.
//!
//! Typing is the default. Visible controls also accept Tab / Enter through
//! `interaction`; legacy browse shortcuts remain optional.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use agentmux_core::PermissionDecision;

use crate::app::{event_summary, App, AppAction, InputMode, RelaySource, RelayStage};

/// `ctrl-<code>` was pressed.
fn is_ctrl(key: &KeyEvent, code: KeyCode) -> bool {
    key.code == code && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// A plain character (no Control/Alt), safe to treat as text input.
/// Shift is allowed — `Char` arrives already case/shift-resolved.
pub(crate) fn plain_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

/// Keystroke in [`InputMode::Normal`].
pub(crate) fn normal_key(app: &mut App, key: KeyEvent) -> AppAction {
    if is_ctrl(&key, KeyCode::Char('c')) {
        return if app.selected_is_native() {
            AppAction::None
        } else {
            AppAction::CancelPrompt
        };
    }
    match key.code {
        KeyCode::Char('q') => app.command("/quit"),
        KeyCode::Char('p') => {
            app.open_permissions();
            AppAction::None
        }
        KeyCode::Char('?') => app.command("/help"),
        KeyCode::End => {
            app.follow_latest();
            AppAction::None
        }
        KeyCode::Esc => {
            app.wb.inspection = None;
            app.mode = InputMode::Editing;
            AppAction::None
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.select_next();
            AppAction::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.select_prev();
            AppAction::None
        }
        KeyCode::Char('i') | KeyCode::Char('a') => {
            app.mode = InputMode::Editing;
            AppAction::None
        }
        KeyCode::Char('n') => {
            app.start_wizard();
            AppAction::None
        }
        KeyCode::Char('@') => {
            app.start_relay();
            AppAction::None
        }
        KeyCode::Tab => {
            app.show_diff = !app.show_diff;
            AppAction::None
        }
        KeyCode::Char('x') => AppAction::KillSession,
        KeyCode::Char('r') => AppAction::ResumeSession,
        _ => AppAction::None,
    }
}

/// Keystroke in [`InputMode::Editing`]: everything goes into `app.input`
/// except the mode/submit keys.
pub(crate) fn editing_key(app: &mut App, key: KeyEvent) -> AppAction {
    if is_ctrl(&key, KeyCode::Char('c')) || key.code == KeyCode::Esc {
        app.wb.inspection = None;
        app.wb.drawer = false;
        return AppAction::None;
    }
    match key.code {
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            app.insert_text("\n")
        }
        KeyCode::Enter => {
            if app.input.trim().is_empty() {
                if app.selected_is_native() {
                    return AppAction::AttachNative;
                }
                return AppAction::None;
            }
            if app.input.starts_with('/') {
                return crate::commands::submit(app);
            }
            if !app.wb.connected {
                app.set_error("Disconnected — draft kept. Exit is in Menu.");
                return AppAction::None;
            }
            if app.selected_session_id().is_none() {
                app.set_status("Click + New space to choose a project and agent first.");
                return AppAction::None;
            }
            app.wb.cursor = 0;
            return AppAction::Submit {
                text: std::mem::take(&mut app.input),
                references: std::mem::take(&mut app.pending_relays),
            };
        }
        KeyCode::Backspace => app.erase(true),
        KeyCode::Delete => app.erase(false),
        KeyCode::Left => app.move_cursor(false),
        KeyCode::Right => app.move_cursor(true),
        KeyCode::Home => app.line_edge(false),
        KeyCode::End => app.line_edge(true),
        KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => app.input_history(true),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => app.input_history(false),
        KeyCode::Up => app.vertical_cursor(false),
        KeyCode::Down => app.vertical_cursor(true),
        _ => {
            if let Some(c) = plain_char(&key) {
                app.insert_text(&c.to_string());
            }
        }
    }
    AppAction::None
}

pub(crate) fn global_key(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    if matches!(
        app.mode,
        InputMode::Permission
            | InputMode::NewSession
            | InputMode::RelayPick
            | InputMode::TaskPicker
            | InputMode::Help
    ) {
        return None;
    }
    if is_ctrl(&key, KeyCode::Char('c')) && !app.selected_is_native() {
        return Some(AppAction::CancelPrompt);
    } else if key.code == KeyCode::F(2) {
        crate::focus::select(app, crate::focus::Pane::Input);
    } else if key.code == KeyCode::F(3) {
        crate::focus::select(app, crate::focus::Pane::Reading);
    } else if key.code == KeyCode::F(4) {
        crate::focus::select(app, crate::focus::Pane::Navigation);
    } else if is_ctrl(&key, KeyCode::Char('b')) {
        app.toggle_sidebar();
    } else if is_ctrl(&key, KeyCode::Char('p')) {
        app.open_picker();
    } else if is_ctrl(&key, KeyCode::Char('o')) {
        app.wb.tools_expanded = !app.wb.tools_expanded;
    } else if is_ctrl(&key, KeyCode::Char('t')) {
        app.wb.thoughts = !app.wb.thoughts;
    } else if key.code == KeyCode::F(1) {
        app.wb.help_scroll = 0;
        app.wb.return_mode = app.mode;
        app.mode = InputMode::Help;
    } else if key.code == KeyCode::F(6) {
        crate::focus::cycle(app, key.modifiers.contains(KeyModifiers::SHIFT));
    } else if key.code == KeyCode::PageUp {
        if app.mode == InputMode::Sidebar {
            return Some(sidebar_page(app, true));
        }
        app.scroll_by(true, 10);
        if app.needs_older_history() {
            return Some(AppAction::LoadHistory);
        }
    } else if key.code == KeyCode::PageDown {
        if app.mode == InputMode::Sidebar {
            return Some(sidebar_page(app, false));
        }
        app.scroll_by(false, 10);
    } else if app.mode == InputMode::Normal
        && app.wb.inspection.is_some()
        && matches!(key.code, KeyCode::Char('[') | KeyCode::Char(']'))
    {
        crate::files::hunk(app, key.code == KeyCode::Char(']'));
    } else if app.mode == InputMode::Normal
        && app.wb.inspection.is_some()
        && key.code == KeyCode::Home
    {
        app.wb.inspect_scroll = 0;
    } else if app.mode == InputMode::Normal
        && app.wb.inspection.is_some()
        && key.code == KeyCode::End
    {
        app.wb.inspect_scroll = app.wb.files.inspection_limit.get();
    } else {
        return None;
    }
    Some(AppAction::None)
}

fn sidebar_page(app: &mut App, up: bool) -> AppAction {
    match app.wb.tab {
        crate::workbench::SideTab::Team => {
            for _ in 0..8 {
                if up {
                    app.select_prev();
                } else {
                    app.select_next();
                }
            }
        }
        crate::workbench::SideTab::Files => {
            app.wb.file_cursor = if up {
                app.wb.file_cursor.saturating_sub(8)
            } else {
                (app.wb.file_cursor + 8).min(app.file_rows().saturating_sub(1))
            }
        }
        crate::workbench::SideTab::Context => {
            app.wb.context_scroll = if up {
                app.wb.context_scroll.saturating_sub(8)
            } else {
                app.wb.context_scroll.saturating_add(8)
            }
        }
    }
    AppAction::None
}

pub(crate) fn sidebar_key(app: &mut App, key: KeyEvent) -> AppAction {
    use crate::workbench::SideTab;
    match key.code {
        KeyCode::Esc | KeyCode::Char('i') => {
            app.mode = InputMode::Editing;
            app.wb.drawer = false;
        }
        KeyCode::Tab | KeyCode::Right => {
            app.wb.tab = match app.wb.tab {
                SideTab::Team => SideTab::Files,
                SideTab::Files => SideTab::Context,
                SideTab::Context => SideTab::Team,
            }
        }
        KeyCode::BackTab | KeyCode::Left => {
            app.wb.tab = match app.wb.tab {
                SideTab::Team => SideTab::Context,
                SideTab::Files => SideTab::Team,
                SideTab::Context => SideTab::Files,
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.wb.tab == SideTab::Team {
                app.select_next();
            } else {
                app.wb.file_cursor =
                    (app.wb.file_cursor + 1).min(app.file_rows().saturating_sub(1));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.wb.tab == SideTab::Team {
                app.select_prev();
            } else {
                app.wb.file_cursor = app.wb.file_cursor.saturating_sub(1);
            }
        }
        KeyCode::Enter => {
            if app.wb.tab == SideTab::Files {
                return app.inspect_selected_file();
            } else {
                app.mode = InputMode::Editing;
                app.wb.drawer = false;
            }
        }
        KeyCode::Char('p') => app.open_permissions(),
        KeyCode::Char('/') if app.wb.tab == SideTab::Files => {
            app.wb.files.searching = true;
        }
        KeyCode::Char('n') => app.start_wizard(),
        _ => {}
    }
    AppAction::None
}

pub(crate) fn picker_key(app: &mut App, key: KeyEvent) -> AppAction {
    match key.code {
        KeyCode::Esc => app.mode = app.wb.return_mode,
        KeyCode::Down => {
            app.wb.picker_cursor =
                (app.wb.picker_cursor + 1).min(app.picker_matches().len().saturating_sub(1))
        }
        KeyCode::Up => app.wb.picker_cursor = app.wb.picker_cursor.saturating_sub(1),
        KeyCode::Enter => {
            if let Some(i) = app.picker_matches().get(app.wb.picker_cursor) {
                app.select_session(*i);
                app.mode = InputMode::Editing;
                app.wb.drawer = false;
            }
        }
        KeyCode::Backspace => {
            app.wb.picker_query.pop();
            app.wb.picker_cursor = 0;
        }
        _ => {
            if let Some(c) = plain_char(&key) {
                app.wb.picker_query.push(c);
                app.wb.picker_cursor = 0;
            }
        }
    }
    AppAction::None
}

/// Keystroke in [`InputMode::RelayPick`] — the two-stage `@` pick.
///
/// Stage `Event`: `j`/`k` walk the selected session's event log, `Enter`
/// locks the source event and moves to stage `Session`. Stage `Session`:
/// `j`/`k` walk the session list, `Enter` completes the relay (marker
/// into the input buffer, `Editing` mode). `Esc`/`q` aborts either stage.
pub(crate) fn relay_pick_key(app: &mut App, key: KeyEvent) -> AppAction {
    // `take` + explicit put-back: the pick is either advanced (state
    // restored) or finished/aborted (state dropped).
    let Some(mut pick) = app.relay.take() else {
        // Mode without state — recover to Normal rather than wedging.
        app.mode = InputMode::Editing;
        return AppAction::None;
    };
    match pick.stage {
        RelayStage::Event => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = InputMode::Editing;
            }
            KeyCode::Enter => {
                // Materialise the pick first — `session_events` borrows
                // `app`, and the mutation below needs it mutable.
                let picked = app
                    .session_events()
                    .nth(pick.event_cursor)
                    .map(|ev| (ev.session_id, ev.seq, event_summary(ev)));
                match picked {
                    Some((session_id, seq, summary)) => {
                        pick.source = Some(RelaySource {
                            session_id,
                            seq,
                            summary,
                        });
                        pick.stage = RelayStage::Session;
                        pick.session_cursor = app.selected;
                        app.relay = Some(pick);
                    }
                    // The event vanished under us (bounded-log drain) —
                    // nothing sane to pick; abort.
                    None => app.mode = InputMode::Editing,
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let count = app.session_events().count();
                if count > 0 {
                    pick.event_cursor = (pick.event_cursor + 1).min(count - 1);
                }
                app.relay = Some(pick);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                pick.event_cursor = pick.event_cursor.saturating_sub(1);
                app.relay = Some(pick);
            }
            _ => app.relay = Some(pick),
        },
        RelayStage::Session => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = InputMode::Editing;
            }
            KeyCode::Enter => app.finish_relay(pick),
            KeyCode::Char('j') | KeyCode::Down => {
                if !app.sessions.is_empty() {
                    pick.session_cursor = (pick.session_cursor + 1).min(app.sessions.len() - 1);
                }
                app.relay = Some(pick);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                pick.session_cursor = pick.session_cursor.saturating_sub(1);
                app.relay = Some(pick);
            }
            _ => app.relay = Some(pick),
        },
    }
    AppAction::None
}

/// Keystroke in [`InputMode::Permission`] — answer the parked agent
/// request: `y` allow-once, `a` allow-always (only when the agent
/// offered that kind), `n` rejects and `Esc` defers. The key sets `pending` and
/// returns [`AppAction::RespondPermission`]; the overlay stays up until
/// the daemon's `PermissionResolved` event (or an RPC error, which
/// un-pends it for a retry). Keys while pending are ignored so a double
/// press can't race two answers.
pub(crate) fn permission_key(app: &mut App, key: KeyEvent) -> AppAction {
    let Some(notice) = app.permission.as_mut() else {
        // Mode without state — recover to Normal rather than wedging.
        app.mode = InputMode::Normal;
        return AppAction::None;
    };
    if key.code == KeyCode::Esc {
        app.mode = notice.resume;
        return AppAction::None;
    }
    if key.code == KeyCode::PageUp {
        app.wb.permission_scroll = app.wb.permission_scroll.saturating_sub(8);
        return AppAction::None;
    }
    if key.code == KeyCode::PageDown {
        app.wb.permission_scroll = app.wb.permission_scroll.saturating_add(8);
        return AppAction::None;
    }
    if notice.pending {
        return AppAction::None;
    }
    let outcome = match key.code {
        KeyCode::Char('y') => PermissionDecision::AllowOnce,
        KeyCode::Char('a') if notice.allows_always() => PermissionDecision::AllowAlways,
        KeyCode::Char('n') => PermissionDecision::Reject,
        _ => return AppAction::None,
    };
    notice.pending = true;
    AppAction::RespondPermission {
        session_id: notice.session_id,
        request_id: notice.request_id.clone(),
        outcome,
    }
}
