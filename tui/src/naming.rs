//! Workbench conversation names are independent of adapter-level names.
use crate::{
    app::{App, AppAction},
    interaction::{buttons, hit, Target},
    theme::THEME,
};
use agentmux_core::SessionId;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;

pub struct NamePanel {
    pub session_id: SessionId,
    pub name: String,
    pub cursor: usize,
    pub loading: bool,
    pub error: Option<String>,
}

pub fn open(app: &mut App) {
    let Some(session_id) = app.selected_session_id() else {
        app.set_status("Select an agent conversation first.");
        return;
    };
    if app.wb.renaming.contains(&session_id) {
        app.set_status("This conversation is already being renamed.");
        return;
    }
    let name = app.session_title(session_id);
    app.wb.naming = Some(NamePanel {
        session_id,
        cursor: name.len(),
        name,
        loading: false,
        error: None,
    });
    app.wb.control_focus = None;
}

pub fn paste(app: &mut App, text: &str) {
    if let Some(panel) = app.wb.naming.as_mut().filter(|p| !p.loading) {
        let available = 60usize.saturating_sub(panel.name.chars().count());
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
            .take(available)
            .collect();
        panel.name.insert_str(panel.cursor, &text);
        panel.cursor += text.len();
        panel.error = None;
    }
}

pub fn keyboard(app: &mut App, key: KeyEvent) -> Option<AppAction> {
    app.wb.naming.as_ref()?;
    if let Some(action) = crate::interaction::keyboard(app, key) {
        return Some(action);
    }
    if key.code == KeyCode::Esc {
        app.wb.naming = None;
        return Some(AppAction::None);
    }
    let panel = app.wb.naming.as_mut().unwrap();
    if panel.loading {
        return Some(AppAction::None);
    }
    match key.code {
        KeyCode::Enter => {
            let name = panel.name.trim();
            if name.is_empty() {
                panel.error = Some("Name must not be empty.".into());
            } else {
                panel.loading = true;
                return Some(AppAction::RenameSession {
                    session_id: panel.session_id,
                    title: name.into(),
                });
            }
        }
        KeyCode::Left | KeyCode::Backspace => {
            let previous = panel.name[..panel.cursor]
                .grapheme_indices(true)
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            if key.code == KeyCode::Backspace {
                panel.name.replace_range(previous..panel.cursor, "");
            }
            panel.cursor = previous;
        }
        KeyCode::Right | KeyCode::Delete => {
            let next = panel.name[panel.cursor..]
                .graphemes(true)
                .next()
                .map(|g| panel.cursor + g.len())
                .unwrap_or(panel.cursor);
            if key.code == KeyCode::Delete {
                panel.name.replace_range(panel.cursor..next, "");
            } else {
                panel.cursor = next;
            }
        }
        KeyCode::Home => panel.cursor = 0,
        KeyCode::End => panel.cursor = panel.name.len(),
        _ => {
            if let Some(c) = crate::input::plain_char(&key)
                .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
            {
                if panel.name.chars().count() < 60 {
                    panel.name.insert(panel.cursor, c);
                    panel.cursor += c.len_utf8();
                    panel.error = None;
                }
            }
        }
    }
    Some(AppAction::None)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let Some(panel) = &app.wb.naming else { return };
    let rect = crate::ui::centered(frame.area(), 68, 11);
    app.wb.hits.borrow_mut().clear();
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Name agent conversation ")
        .style(THEME.panel);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);
    let target = app
        .sessions
        .iter()
        .find(|v| v.session.id == panel.session_id)
        .map(|v| format!("{} / {}", v.agent_name, v.workspace_name))
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(target).style(THEME.dim), parts[0]);
    let input = if panel.loading {
        "Saving...".into()
    } else {
        format!(
            "{}|{}",
            &panel.name[..panel.cursor],
            &panel.name[panel.cursor..]
        )
    };
    let content = format!("{input}\n{}", panel.error.as_deref().unwrap_or(""));
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .style(THEME.text),
        parts[1],
    );
    hit(app, parts[1], Target::NameInput);
    if !panel.loading && parts[1].width > 0 && parts[1].height > 0 {
        let layout = crate::editor::EditorLayout::new(&panel.name, parts[1].width);
        let (row, col) = layout.position(panel.cursor);
        frame.set_cursor_position(ratatui::layout::Position::new(
            parts[1].x + (col as u16).min(parts[1].width - 1),
            parts[1].y + (row as u16).min(parts[1].height - 1),
        ));
    }
    if panel.loading {
        buttons(
            frame,
            app,
            parts[2],
            &[("Close", Target::Command("/close-name"))],
        );
    } else {
        buttons(
            frame,
            app,
            parts[2],
            &[
                ("Save", Target::Key(KeyCode::Enter)),
                ("Cancel", Target::Command("/close-name")),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn unicode_editing_and_paste_never_change_conversation_draft() {
        let mut app = App::new(vec![], vec![], vec![], vec![]);
        app.insert_text("draft");
        app.wb.naming = Some(NamePanel {
            session_id: SessionId::new(),
            name: String::new(),
            cursor: 0,
            loading: false,
            error: None,
        });
        paste(&mut app, "回归测试\n");
        keyboard(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(app.wb.naming.as_ref().unwrap().name, "回归测");
        assert_eq!(app.input, "draft");
        assert!(
            matches!(keyboard(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Some(AppAction::RenameSession { title, .. }) if title == "回归测")
        );
        assert_eq!(
            keyboard(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(AppAction::None)
        );
        keyboard(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.wb.naming.is_none());
        assert_eq!(app.input, "draft");
    }
}
