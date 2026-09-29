//! Key → action mapping, one handler per [`InputMode`](crate::app::InputMode).
//!
//! Pure functions over `&mut App` — they mutate only UI state (mode,
//! selection, input buffer) and return an [`AppAction`] for the async
//! layer in `main.rs`. No terminal or daemon access here.
//!
//! v1 keys (design spec §8):
//!
//! | Mode      | Key            | Action                              |
//! |-----------|----------------|-------------------------------------|
//! | Normal    | `q`            | quit                                |
//! | Normal    | `j`/`k`, ↓/↑   | move selection                      |
//! | Normal    | `i`/`a`        | enter Editing                       |
//! | Normal    | `n`            | new-session wizard                  |
//! | Normal    | `@`            | relay pick (event → target session) |
//! | Normal    | `Tab`          | toggle files/diff panel             |
//! | Normal    | `x`            | kill selected session               |
//! | Normal    | `r`            | resume selected (Done/Error) session|
//! | Normal    | `ctrl-c`       | cancel selected session turn        |
//! | Editing   | `Enter`        | submit prompt (+ staged relays)     |
//! | Editing   | `Esc`/`ctrl-c` | back to Normal                      |
//! | RelayPick | `j`/`k`        | move cursor within the stage        |
//! | RelayPick | `Enter`/`Esc`  | advance stage / abort               |
//! | NewSession| `j`/`k`/`Enter`/`Esc` | wizard steps (see newsession)  |
//! | Permission| `y`            | allow once                        |
//! | Permission| `a`            | allow always (when offered)       |
//! | Permission| `n`/`Esc`      | reject                            |

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
        return AppAction::CancelPrompt;
    }
    match key.code {
        KeyCode::Char('q') => AppAction::Quit,
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
    // `ctrl-c` exits Editing rather than cancelling a prompt — the key
    // chord means "leave what I'm doing" here (ruling 4).
    if is_ctrl(&key, KeyCode::Char('c')) || key.code == KeyCode::Esc {
        app.mode = InputMode::Normal;
        return AppAction::None;
    }
    match key.code {
        KeyCode::Enter => {
            let text = std::mem::take(&mut app.input);
            if text.is_empty() {
                AppAction::None
            } else {
                // Staged `@` relays ride along; `main.rs` maps them to
                // `SessionRef`s on the `session/prompt` wire params.
                AppAction::Submit {
                    text,
                    references: std::mem::take(&mut app.pending_relays),
                }
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
            AppAction::None
        }
        _ => {
            if let Some(c) = plain_char(&key) {
                app.input.push(c);
            }
            AppAction::None
        }
    }
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
        app.mode = InputMode::Normal;
        return AppAction::None;
    };
    match pick.stage {
        RelayStage::Event => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.mode = InputMode::Normal;
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
                    None => app.mode = InputMode::Normal,
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
                app.mode = InputMode::Normal;
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
/// offered that kind), `n`/`Esc` reject. The key sets `pending` and
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
    if notice.pending {
        return AppAction::None;
    }
    let outcome = match key.code {
        KeyCode::Char('y') => PermissionDecision::AllowOnce,
        KeyCode::Char('a') if notice.allows_always() => PermissionDecision::AllowAlways,
        KeyCode::Char('n') | KeyCode::Esc => PermissionDecision::Reject,
        _ => return AppAction::None,
    };
    notice.pending = true;
    AppAction::RespondPermission {
        session_id: notice.session_id,
        request_id: notice.request_id.clone(),
        outcome,
    }
}
