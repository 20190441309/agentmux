//! Key → action mapping, one handler per [`InputMode`](crate::app::InputMode).
//!
//! Pure functions over `&mut App` — they mutate only UI state (mode,
//! selection, input buffer) and return an [`AppAction`] for the async
//! layer in `main.rs`. No terminal or daemon access here.
//!
//! v1 keys (design spec §8):
//!
//! | Mode      | Key            | Action                        |
//! |-----------|----------------|-------------------------------|
//! | Normal    | `q`            | quit                          |
//! | Normal    | `j`/`k`, ↓/↑   | move selection                |
//! | Normal    | `i`/`a`        | enter Editing                 |
//! | Normal    | `n`            | new session                   |
//! | Normal    | `@`            | enter RelayPick (T14)         |
//! | Normal    | `ctrl-c`       | cancel selected session turn  |
//! | Editing   | `Enter`        | submit prompt                 |
//! | Editing   | `Esc`/`ctrl-c` | back to Normal                |
//! | RelayPick | `j`/`k`        | pick target                   |
//! | RelayPick | `Enter`/`Esc`  | confirm / abort               |

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, AppAction, InputMode};

/// `ctrl-<code>` was pressed.
fn is_ctrl(key: &KeyEvent, code: KeyCode) -> bool {
    key.code == code && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// A plain character (no Control/Alt), safe to treat as text input.
/// Shift is allowed — `Char` arrives already case/shift-resolved.
fn plain_char(key: &KeyEvent) -> Option<char> {
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
        KeyCode::Char('n') => AppAction::NewSession,
        KeyCode::Char('@') => {
            app.mode = InputMode::RelayPick;
            AppAction::None
        }
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
                AppAction::Submit(text)
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

/// Keystroke in [`InputMode::RelayPick`] — Task 14 stub: navigate to a
/// target, `Enter` confirms (main.rs reports it is not wired yet),
/// `Esc`/`q` aborts.
pub(crate) fn relay_pick_key(app: &mut App, key: KeyEvent) -> AppAction {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = InputMode::Normal;
            AppAction::None
        }
        KeyCode::Enter => {
            app.mode = InputMode::Normal;
            AppAction::Relay
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.select_next();
            AppAction::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.select_prev();
            AppAction::None
        }
        _ => AppAction::None,
    }
}
