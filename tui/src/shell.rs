//! Responsive terminal shell: conversation first, collaboration on the right.
use crate::interaction::{buttons, hit, Target};
use crate::{
    app::{App, InputMode},
    theme::THEME,
    ui,
    workbench::SideTab,
};
use agentmux_core::SessionState;
use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
    },
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn compact(area: Rect) -> bool {
    area.width < 90 || area.height < 24
}

pub(crate) fn fit_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.to_string();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width();
        if used + cells > width - 1 {
            break;
        }
        result.push_str(grapheme);
        used += cells;
    }
    result.push('…');
    result
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    app.wb.hits.borrow_mut().clear();
    frame.render_widget(
        Block::default().style(THEME.backdrop.patch(THEME.text)),
        area,
    );
    let compact = compact(area);
    let rows = Layout::vertical([
        Constraint::Length(if compact { 2 } else { 3 }),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    draw_header(frame, app, rows[0], compact);
    let sidebar = area.width >= 110 && !app.wb.focus;
    let body = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(if sidebar { 34 } else { 0 }),
    ])
    .split(rows[1]);
    let main = Rect::new(
        body[0].x + 1,
        body[0].y,
        body[0].width.saturating_sub(2),
        body[0].height,
    );
    let (editor_rows, _, _) = editor_lines(
        &app.input,
        app.wb.cursor,
        main.width.saturating_sub(5).max(1),
    );
    // Size by the whole draft, not its cursor: moving upward must not shrink chat.
    let input_height = editor_rows
        .len()
        .saturating_add(if compact { 2 } else { 4 })
        .clamp(if compact { 3 } else { 6 }, 10)
        .min(usize::from((main.height / 2).max(3)))
        .min(usize::from(main.height)) as u16;
    let inner = Layout::vertical([
        Constraint::Length(u16::from(app.wb.attention.latest_error().is_some())),
        Constraint::Min(1),
        Constraint::Length(input_height),
    ])
    .split(main);
    if inner[0].height > 0 {
        crate::attention::banner(frame, app, inner[0]);
    }
    hit(app, inner[1], Target::Conversation);
    if app.wb.inspection.is_some() {
        draw_inspection(frame, app, inner[1]);
    } else {
        ui::draw_body(frame, app, inner[1]);
    }
    draw_input(frame, app, inner[2], compact);
    if crate::focus::current(app) == crate::focus::Pane::Reading && inner[1].height > 0 {
        for y in inner[1].y..inner[1].bottom() {
            frame.buffer_mut()[(inner[1].x.saturating_sub(1), y)]
                .set_symbol("│")
                .set_style(THEME.accent);
        }
    }
    if sidebar {
        draw_sidebar(frame, app, body[1]);
    }
    draw_footer(frame, app, rows[2]);
    if !sidebar
        && (app.wb.drawer
            || app.mode == InputMode::Sidebar
            || (app.mode == InputMode::RelayPick
                && app
                    .relay
                    .as_ref()
                    .is_some_and(|p| p.stage == crate::app::RelayStage::Session)))
    {
        let rect = Rect::new(
            rows[1].right().saturating_sub(32.min(area.width)),
            rows[1].y,
            32.min(area.width),
            rows[1].height,
        );
        hit(app, rect, Target::Blocked);
        frame.render_widget(Clear, rect);
        draw_sidebar(frame, app, rect);
    }
    if matches!(
        app.mode,
        InputMode::NewSession | InputMode::Permission | InputMode::TaskPicker | InputMode::Help
    ) {
        app.wb.hits.borrow_mut().clear();
    }
    match app.mode {
        InputMode::NewSession => ui::draw_wizard(frame, app),
        InputMode::Permission => ui::draw_permission(frame, app),
        InputMode::TaskPicker => draw_picker(frame, app),
        InputMode::Help => draw_help(frame, app),
        _ => {}
    }
    crate::commands::draw(frame, app);
    if app.wb.menu {
        app.wb.hits.borrow_mut().clear();
        draw_menu(frame, app);
    }
    crate::naming::draw(frame, app);
    crate::attention::draw(frame, app);
    crate::transcript::draw(frame, app);
    crate::references::draw(frame, app);
    crate::agent_info::draw(frame, app);
    crate::context::draw_panel(frame, app);
    if let Some(target) = &app.wb.control_focus {
        if let Some(h) = crate::interaction::focusable(app)
            .iter()
            .find(|h| &h.target == target)
        {
            frame.buffer_mut().set_style(h.area, THEME.selection);
        }
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, compact: bool) {
    let title = app
        .selected_session_id()
        .and_then(|id| app.wb.sessions.get(&id))
        .and_then(|s| s.title.as_deref())
        .unwrap_or("New conversation");
    let workspace = app
        .selected_session()
        .map(|v| v.workspace_name.as_str())
        .unwrap_or("workspace");
    let menu_label = if app.permission_count() > 0 {
        format!("Menu · {}", app.permission_count())
    } else {
        "Menu".into()
    };
    if compact {
        let width = usize::from(area.width.saturating_sub(4));
        let space = fit_text(workspace, (width / 2).max(1));
        let title = fit_text(title, width.saturating_sub(space.width() + 3));
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(space, THEME.accent),
                Span::styled(" / ", THEME.dim),
                Span::styled(
                    title,
                    THEME.text.add_modifier(ratatui::style::Modifier::BOLD),
                ),
            ])),
            Rect::new(area.x + 2, area.y, area.width.saturating_sub(4), 1),
        );
        let mut candidates = vec![];
        if area.width >= 70 {
            candidates.push(("+ New space", Target::Command("/new")));
        }
        if area.width >= 30 {
            candidates.push((
                if area.width >= 40 {
                    "+ Add agent"
                } else {
                    "+ Agent"
                },
                Target::Command("/add-agent"),
            ));
        }
        if area.width >= 20 {
            candidates.push(("Agents", Target::Command("/tasks")));
        }
        let mut choices = vec![];
        let mut used = 0;
        // Menu is the fallback for hidden actions, so always reserve its cells.
        for (label, target) in candidates {
            let needed = label.width() + 3;
            if used + needed + menu_label.width() + 2 <= usize::from(area.width.saturating_sub(2)) {
                choices.push((label, target));
                used += needed;
            }
        }
        choices.push((menu_label.as_str(), Target::Menu));
        buttons(
            frame,
            app,
            Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1),
            &choices,
        );
        return;
    }
    let creation_width = 27;
    let navigation_width = 11 + menu_label.width() as u16;
    let action_width = creation_width + 3 + navigation_width;
    frame.render_widget(
        Paragraph::new(fit_text(
            title,
            usize::from(area.width.saturating_sub(action_width + 4)),
        ))
        .style(THEME.text.add_modifier(ratatui::style::Modifier::BOLD)),
        Rect::new(
            area.x + 2,
            area.y,
            area.width.saturating_sub(action_width + 4),
            1,
        ),
    );
    let actions = Rect::new(
        area.right().saturating_sub(action_width + 2),
        area.y,
        creation_width,
        1,
    );
    buttons(
        frame,
        app,
        actions,
        &[
            ("+ New space", Target::Command("/new")),
            ("+ Add agent", Target::Command("/add-agent")),
        ],
    );
    buttons(
        frame,
        app,
        Rect::new(actions.right() + 3, actions.y, navigation_width, 1),
        &[
            ("Agents", Target::Command("/tasks")),
            (&menu_label, Target::Menu),
        ],
    );
    let y = area.y + 1;
    let meta_width = area.width.saturating_sub(4);
    if meta_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("agentmux", THEME.accent),
                Span::styled(
                    fit_text(
                        &format!("  /  {workspace}"),
                        usize::from(meta_width.saturating_sub(10)),
                    ),
                    THEME.dim,
                ),
            ])),
            Rect::new(area.x + 2, y, meta_width.saturating_sub(2), 1),
        );
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Block::default().style(THEME.panel), area);
    if crate::focus::current(app) == crate::focus::Pane::Navigation {
        for y in area.y..area.bottom() {
            frame.buffer_mut()[(area.x, y)]
                .set_symbol("│")
                .set_style(THEME.accent);
        }
    }
    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let parts = Layout::vertical([
        Constraint::Length(if area.height >= 20 { 4 } else { 0 }),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(inner);
    if parts[0].height > 0 {
        let workspace = app.selected_session().and_then(|v| {
            app.workspaces
                .iter()
                .find(|w| w.id == v.session.workspace_id)
        });
        let current = app.selected_session().map(|v| v.session.workspace_id);
        let members = app
            .sessions
            .iter()
            .filter(|v| Some(v.session.workspace_id) == current)
            .count();
        let running = app
            .sessions
            .iter()
            .filter(|v| {
                Some(v.session.workspace_id) == current
                    && matches!(
                        v.session.state,
                        SessionState::Prompting | SessionState::WaitingPermission
                    )
            })
            .count();
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    workspace
                        .map(|workspace| workspace.name.as_str())
                        .unwrap_or("No space"),
                    THEME.text.add_modifier(ratatui::style::Modifier::BOLD),
                ),
                Line::styled(
                    workspace
                        .map(|w| {
                            if w.branch == w.name {
                                String::new()
                            } else {
                                fit_text(&w.branch, usize::from(parts[0].width))
                            }
                        })
                        .unwrap_or_else(|| "No project selected".into()),
                    THEME.dim,
                ),
                Line::styled(
                    format!("{members} agents · {running} active here"),
                    THEME.dim,
                ),
                Line::styled(
                    if app.sessions.len() > members {
                        format!("{} in other spaces", app.sessions.len() - members)
                    } else {
                        String::new()
                    },
                    THEME.faint,
                ),
            ]),
            parts[0],
        );
    }
    buttons(
        frame,
        app,
        parts[1],
        &[
            ("Agents", Target::Tab(SideTab::Team)),
            ("Files", Target::Tab(SideTab::Files)),
            ("Context", Target::Tab(SideTab::Context)),
        ],
    );
    match app.wb.tab {
        SideTab::Team => ui::draw_sessions(frame, app, parts[2]),
        SideTab::Files => ui::draw_files(frame, app, parts[2]),
        SideTab::Context => crate::context::draw(frame, app, parts[2]),
    }
    let permissions = format!("Permissions · {}", app.permission_count());
    let mut controls = vec![
        ("+ Add agent", Target::Command("/add-agent")),
        ("Quote", Target::Command("/relay")),
        ("Close panel", Target::Command("/hide-sidebar")),
    ];
    if app.permission_count() > 0 {
        controls.insert(0, (permissions.as_str(), Target::Command("/permissions")));
    }
    buttons(frame, app, parts[3], &controls);
}

pub fn editor_lines(text: &str, cursor: usize, width: u16) -> (Vec<String>, usize, usize) {
    let layout = crate::editor::EditorLayout::new(text, width);
    let (row, col) = layout.position(cursor);
    (layout.lines, row, col)
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect, compact: bool) {
    if area.height < 2 || area.width < 4 {
        return;
    }
    let id = app.selected_session_id();
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_type(BorderType::Thick)
        .padding(Padding::new(1, 1, u16::from(!compact), 0))
        .border_style(if crate::focus::current(app) == crate::focus::Pane::Input {
            THEME.border_focus
        } else {
            THEME.border
        })
        .style(THEME.input);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(if compact { 1 } else { 2 }),
    ])
    .split(inner);
    let content = Rect::new(
        rows[0].x + 1,
        rows[0].y,
        rows[0].width.saturating_sub(2),
        rows[0].height,
    );
    app.wb.editor_width.set(content.width.max(1));
    let (lines, crow, ccol) = editor_lines(&app.input, app.wb.cursor, content.width.max(1));
    let offset = crow.saturating_sub(content.height.saturating_sub(1) as usize);
    hit(
        app,
        content,
        Target::Editor {
            area: content,
            offset,
        },
    );
    if app.input.is_empty() {
        frame.render_widget(Paragraph::new("Ask anything…").style(THEME.dim), content);
    } else {
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(offset)
                    .take(content.height as usize)
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            )
            .style(THEME.text),
            content,
        );
    }
    if app.mode == InputMode::Editing
        && !app.overlay_open()
        && app.wb.control_focus.is_none()
        && !app.wb.files.searching
        && content.height > 0
        && content.width > 0
    {
        frame.set_cursor_position(Position::new(
            content.x + (ccol as u16).min(content.width - 1),
            content.y + (crow - offset) as u16,
        ));
    }
    let queued = id
        .and_then(|id| app.wb.queues.get(&id))
        .map(|q| q.len())
        .unwrap_or(0);
    let busy = app.selected_session().is_some_and(|s| {
        matches!(
            s.session.state,
            SessionState::Prompting | SessionState::WaitingPermission
        )
    }) || id.is_some_and(|id| app.wb.in_flight.contains(&id));
    let resuming = id.is_some_and(|id| app.wb.resuming.contains(&id));
    let needs_resume = app
        .selected_session()
        .is_some_and(|s| matches!(s.session.state, SessionState::Done | SessionState::Error(_)));
    if app.mode == InputMode::RelayPick {
        buttons(frame, app, rows[2], &[("Cancel quote", Target::Close)]);
        return;
    }
    if let Some(view) = app.selected_session() {
        let instance = app.agent_instance(view.session.id);
        let title = format!(
            "{} · {}",
            app.session_title(view.session.id),
            crate::agent_info::summary(app, view.session.id)
        );
        let mut counts = String::new();
        if queued > 0 {
            counts.push_str(&format!(" · {queued} queued"));
        }
        if !app.pending_relays.is_empty() {
            counts.push_str(&format!(" · {} refs", app.pending_relays.len()));
        }
        let width = usize::from(rows[1].width);
        let recipient = if instance.width() + 5 > width {
            let (name, ordinal) = instance.rsplit_once(" #").unwrap_or((&instance, "1"));
            let suffix = format!(" #{ordinal}");
            format!(
                " To: {}{suffix}",
                fit_text(name, width.saturating_sub(5 + suffix.width()))
            )
        } else {
            format!(" To: {instance}")
        };
        let mut spans = vec![Span::styled(fit_text(&recipient, width), THEME.accent_bold)];
        let remaining = width.saturating_sub(recipient.width());
        if instance != title && remaining > counts.width() + 4 {
            spans.push(Span::styled(
                format!(" · {}", fit_text(&title, remaining - counts.width() - 3)),
                THEME.dim,
            ));
        }
        spans.push(Span::styled(fit_text(&counts, remaining), THEME.warning));
        frame.render_widget(Paragraph::new(Line::from(spans)), rows[1]);
    }
    let mut controls = vec![("New line", Target::Command("/newline"))];
    if busy {
        controls.push(("Stop", Target::Command("/cancel")));
    } else if needs_resume && !resuming {
        controls.push(("Resume", Target::Command("/resume")));
    }
    let send_label = if resuming || busy {
        "Queue"
    } else if needs_resume {
        "Resume & send"
    } else {
        "Send"
    };
    let send_width = (send_label.len() as u16 + 4).min(rows[2].width);
    buttons(
        frame,
        app,
        Rect::new(
            rows[2].x,
            rows[2].y,
            rows[2].width.saturating_sub(send_width),
            rows[2].height,
        ),
        &controls,
    );
    buttons(
        frame,
        app,
        Rect::new(
            rows[2].right().saturating_sub(send_width),
            rows[2].y,
            send_width,
            1,
        ),
        &[(send_label, Target::Key(crossterm::event::KeyCode::Enter))],
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let default_status = "Enter send · Alt+Enter new line";
    let activity = app.run_status();
    let text = activity
        .as_deref()
        .or(app
            .status
            .as_deref()
            .filter(|s| !s.starts_with("connected ·")))
        .unwrap_or(default_status);
    let pending = app.attention_items().len();
    let pending_label = format!("Pending · {pending}");
    let mut choices = vec![];
    if pending > 0 {
        choices.push((pending_label.as_str(), Target::Command("/attention")));
    }
    let help_visible = area.width >= 40;
    if help_visible {
        choices.push(("Help", Target::Command("/help")));
    }
    choices.extend([
        (
            if app.wb.thoughts {
                "Fold thoughts"
            } else {
                "Show thoughts"
            },
            Target::Command("/thinking"),
        ),
        (
            if app.wb.tools_expanded {
                "Hide details"
            } else {
                "Details"
            },
            Target::Command("/tools"),
        ),
        ("Latest", Target::Command("/latest")),
    ]);
    let available = usize::from(area.width);
    let controls_width = |choices: &[(&str, Target)]| {
        choices
            .iter()
            .map(|(label, _)| label.width() + 3)
            .sum::<usize>()
            .saturating_sub(1)
    };
    if 3 + text.width() + controls_width(&choices) >= available {
        choices.truncate(usize::from(pending > 0) + usize::from(help_visible));
    }
    let right_width = controls_width(&choices).min(available.saturating_sub(3)) as u16;
    let text_width = usize::from(area.width.saturating_sub(right_width + 3));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if app.wb.connected { " ● " } else { " ○ " },
                if app.wb.connected {
                    THEME.success
                } else {
                    THEME.error
                },
            ),
            Span::styled(
                fit_text(text, text_width),
                if app.wb.connected {
                    THEME.dim
                } else {
                    THEME.error
                },
            ),
        ])),
        Rect::new(
            area.x,
            area.y,
            area.width.saturating_sub(right_width),
            area.height,
        ),
    );
    if right_width > 0 {
        buttons(
            frame,
            app,
            Rect::new(area.right() - right_width, area.y, right_width, area.height),
            &choices,
        );
    }
}

fn draw_inspection(frame: &mut Frame, app: &App, area: Rect) {
    let controls = Layout::vertical([
        Constraint::Length(if area.width < 70 { 3 } else { 2 }),
        Constraint::Min(1),
    ])
    .split(area);
    buttons(
        frame,
        app,
        Rect::new(controls[0].x, controls[0].y, controls[0].width, 1),
        &[
            ("Back to chat", Target::Command("/chat")),
            ("Prev hunk", Target::Hunk(false)),
            ("Next hunk", Target::Hunk(true)),
        ],
    );
    buttons(
        frame,
        app,
        Rect::new(
            controls[0].x,
            controls[0].y + 1,
            controls[0].width,
            controls[0].height.saturating_sub(1),
        ),
        &[
            (
                "HEAD",
                Target::DiffScope(agentmux_core::rpc::DiffScope::Head),
            ),
            (
                "Staged",
                Target::DiffScope(agentmux_core::rpc::DiffScope::Staged),
            ),
            (
                "Unstaged",
                Target::DiffScope(agentmux_core::rpc::DiffScope::Unstaged),
            ),
        ],
    );
    let area = controls[1];
    let Some((path, text)) = &app.wb.inspection else {
        return;
    };
    let lines = text
        .lines()
        .map(|l| {
            Line::styled(
                l.to_string(),
                if l.starts_with('+') {
                    THEME.success
                } else if l.starts_with('-') {
                    THEME.error
                } else {
                    THEME.text
                },
            )
        })
        .collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(THEME.border)
        .title(Line::styled(
            fit_text(
                &format!(
                    " {} · {:?}{}{}{} ",
                    path.replace('\n', "\\n").replace('\t', "\\t"),
                    app.wb.files.shown_scope,
                    if app.wb.pending_diff.is_some() {
                        " · loading"
                    } else {
                        ""
                    },
                    if app.wb.files.diff_binary {
                        " · binary"
                    } else {
                        ""
                    },
                    if app.wb.files.diff_truncated {
                        " · truncated"
                    } else {
                        ""
                    }
                ),
                usize::from(area.width),
            ),
            THEME.accent,
        ));
    let inner = block.inner(area);
    app.wb.files.inspection_width.set(inner.width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max = paragraph
        .line_count(inner.width)
        .saturating_sub(inner.height as usize)
        .min(u16::MAX as usize) as u16;
    app.wb.files.inspection_limit.set(max);
    frame.render_widget(
        paragraph
            .block(block)
            .scroll((app.wb.inspect_scroll.min(max), 0)),
        area,
    );
}

pub(crate) fn clear_overlay(frame: &mut Frame, rect: Rect) {
    // Erase adjacent wide glyphs before painting single-cell modal borders.
    let left = rect.x.saturating_sub(1).max(frame.area().x);
    let right = rect.right().saturating_add(1).min(frame.area().right());
    let wash = Rect::new(left, rect.y, right.saturating_sub(left), rect.height);
    frame.render_widget(Clear, wash);
    frame.render_widget(
        Block::default().style(THEME.backdrop.patch(THEME.text)),
        wash,
    );
}

pub(crate) fn modal(frame: &mut Frame, width: u16, height: u16, title: &str) -> Rect {
    let rect = ui::centered(frame.area(), width, height);
    clear_overlay(frame, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(THEME.border_focus)
        .style(THEME.panel)
        .title(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

fn draw_picker(frame: &mut Frame, app: &App) {
    let inner = modal(frame, 70, 18, " Agents ");
    let parts = Layout::vertical([
        Constraint::Length(if inner.width < 60 { 4 } else { 3 }),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(inner);
    buttons(
        frame,
        app,
        parts[2],
        &[
            ("Close", Target::Close),
            ("+ Add agent", Target::Command("/add-agent")),
            ("+ New space", Target::Command("/new")),
        ],
    );
    frame.render_widget(
        Paragraph::new(format!(" / {}", app.wb.picker_query)).style(THEME.accent),
        parts[0],
    );
    buttons(
        frame,
        app,
        Rect::new(
            parts[0].x,
            parts[0].y + 1,
            parts[0].width,
            parts[0].height.saturating_sub(1),
        ),
        &[
            (
                "All",
                Target::AgentFilter(crate::workbench::AgentFilter::All),
            ),
            (
                "Running",
                Target::AgentFilter(crate::workbench::AgentFilter::Running),
            ),
            (
                "Pending",
                Target::AgentFilter(crate::workbench::AgentFilter::Pending),
            ),
            (
                "Unread",
                Target::AgentFilter(crate::workbench::AgentFilter::Unread),
            ),
            ("This space", Target::PickerSpace),
        ],
    );
    let matches = app.picker_matches();
    let current = app.selected_session().map(|v| v.session.workspace_id);
    let mut items = vec![];
    let mut targets = vec![];
    let mut group = None;
    let mut selected = None;
    for (cursor, i) in matches.iter().enumerate() {
        let v = &app.sessions[*i];
        let here = Some(v.session.workspace_id) == current;
        if group != Some(here) {
            items.push(ListItem::new(Line::styled(
                if here {
                    "Current space"
                } else {
                    "Other spaces"
                },
                THEME.section,
            )));
            targets.push((1, None));
            group = Some(here);
        }
        if cursor == app.wb.picker_cursor {
            selected = Some(items.len());
        }
        targets.push((2, Some(v.session.id)));
        items.push(ListItem::new(vec![
            Line::styled(app.session_title(v.session.id), THEME.text),
            Line::styled(
                format!("  {} / {}", v.agent_name, v.workspace_name),
                THEME.dim,
            ),
        ]));
    }
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching agents.").style(THEME.dim),
            parts[1],
        );
        return;
    }
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(THEME.selection)
            .highlight_symbol("› "),
        parts[1],
        &mut state,
    );
    let mut y = parts[1].y;
    for (height, id) in targets.into_iter().skip(state.offset()) {
        if y + height > parts[1].bottom() {
            break;
        }
        if let Some(id) = id {
            hit(
                app,
                Rect::new(parts[1].x, y, parts[1].width, height),
                Target::Session(id),
            );
        }
        y += height;
    }
}

fn draw_help(frame: &mut Frame, app: &App) {
    let inner = modal(frame, 76, 19, " Getting started ");
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let text = "New space · Add agent · Agents · Pending\n\nF2  Input\nF3  Chat / diff\nF4  Navigation\nF6 / Shift+F6  Next / previous pane\nTab / Shift+Tab  Next / previous control\nEnter  Activate / send\nAlt+Enter  New line\nCtrl+C  Stop structured turn; draft retained\nPageUp / PageDown  Scroll focused pane\nHome / End  Diff start / end\n[ / ]  Previous / next diff hunk\nCtrl+P  Agent picker\nCtrl+B  Sidebar\nCtrl+O  Tool details\nCtrl+T  Thoughts\nEsc  Back\n\nNative mode keeps native keys.\nCtrl+] then m returns to the workbench.\nQueued messages stay in this TUI until sent.";
    let paragraph = Paragraph::new(text)
        .style(THEME.text)
        .wrap(Wrap { trim: false });
    let limit = paragraph
        .line_count(parts[0].width)
        .saturating_sub(usize::from(parts[0].height))
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        paragraph.scroll((app.wb.help_scroll.min(limit), 0)),
        parts[0],
    );
    buttons(frame, app, parts[1], &[("Close", Target::Close)]);
}

pub(crate) fn menu_actions() -> Vec<(&'static str, Target)> {
    vec![
        ("New space", Target::Command("/new")),
        ("Add agent to current space", Target::Command("/add-agent")),
        ("Find agent conversation", Target::Command("/tasks")),
        ("Agent details", Target::Command("/info")),
        ("Search conversation", Target::Command("/search")),
        (
            "Previous user message",
            Target::Command("/previous-message"),
        ),
        ("Next user message", Target::Command("/next-message")),
        ("Edit draft externally", Target::Command("/edit-draft")),
        ("Name agent conversation", Target::Command("/rename")),
        ("Files", Target::Command("/files")),
        ("Workspace context", Target::Command("/context")),
        ("Draft references", Target::Command("/references")),
        ("Show / hide sidebar", Target::Command("/sidebar")),
        ("Help", Target::Command("/help")),
        ("Pending items", Target::Command("/attention")),
        ("Error details", Target::Command("/error")),
        ("Review permissions", Target::Command("/permissions")),
        ("Quote event to another agent", Target::Command("/relay")),
        ("Expand / collapse tools", Target::Command("/tools")),
        ("Jump to latest", Target::Command("/latest")),
        ("Expand / fold thoughts", Target::Command("/thinking")),
        ("Restore last queued message", Target::Command("/unqueue")),
        ("Recover failed message", Target::Command("/recover")),
        ("Resume selected agent", Target::Command("/resume")),
        ("Stop current turn", Target::Command("/cancel")),
        ("Exit TUI", Target::Command("/quit")),
        ("Close menu", Target::Close),
    ]
}
fn draw_menu(frame: &mut Frame, app: &App) {
    let inner = modal(frame, 58, 21, " Actions · arrows / wheel to scroll ");
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let actions = menu_actions();
    let mut state = ListState::default().with_selected(Some(app.wb.menu_cursor));
    frame.render_stateful_widget(
        List::new(
            actions
                .iter()
                .map(|(label, _)| ListItem::new(*label))
                .collect::<Vec<_>>(),
        ),
        parts[0],
        &mut state,
    );
    for (row, choice) in actions
        .iter()
        .skip(state.offset())
        .take(parts[0].height as usize)
        .enumerate()
    {
        let area = Rect::new(parts[0].x, parts[0].y + row as u16, parts[0].width, 1);
        frame.render_widget(
            Paragraph::new(format!(" {} ", choice.0)).style(
                if state.offset() + row == app.wb.menu_cursor {
                    THEME.selection
                } else {
                    THEME.text
                },
            ),
            area,
        );
        hit(app, area, choice.1.clone());
    }
    buttons(
        frame,
        app,
        parts[1],
        &[
            ("Up", Target::MenuStep(true)),
            ("Down", Target::MenuStep(false)),
            ("Close", Target::Close),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SessionView;
    use agentmux_core::{AgentId, Session, SessionId, WorkspaceId};
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};

    fn selected_app() -> App {
        let session = Session {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            agent_id: AgentId::new("mock"),
            state: SessionState::Ready,
            acp_session_id: None,
            native_session_file: None,
            native_terminal: false,
            references: vec![],
            created_at: Utc::now(),
        };
        let id = session.id;
        let mut app = App::new(
            vec![],
            vec![],
            vec![SessionView {
                session,
                agent_name: "Mock".into(),
                workspace_name: "repo-main".into(),
            }],
            vec![],
        );
        app.set_title_from_prompt(id, "Test task");
        app
    }

    fn render(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
    }

    fn row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, row)].symbol())
            .collect()
    }

    fn conversation_area(app: &App) -> Rect {
        app.wb
            .hits
            .borrow()
            .iter()
            .find(|h| h.target == Target::Conversation)
            .unwrap()
            .area
    }

    #[test]
    fn compact_layout_keeps_space_recipient_and_minimum_reading_rows() {
        for (width, height, minimum) in [(80, 24, 14), (40, 16, 6)] {
            let app = selected_app();
            let terminal = render(&app, width, height);
            let conversation = conversation_area(&app);
            assert_eq!(conversation.y, 2);
            assert!(conversation.height.saturating_sub(1) >= minimum);
            assert!(row_text(&terminal, 0).contains("repo-main / Test task"));
            assert!(row_text(&terminal, height - 3).contains("To: Mock #1"));
            assert!(row_text(&terminal, height - 1).contains('●'));
            assert!(app
                .wb
                .hits
                .borrow()
                .iter()
                .any(|h| h.target == Target::Key(crossterm::event::KeyCode::Enter)));
        }
    }

    #[test]
    fn short_wide_windows_also_use_compact_chrome() {
        let app = selected_app();
        render(&app, 120, 16);
        assert_eq!(conversation_area(&app).y, 2);
        assert!(conversation_area(&app).height >= 8);
        render(&app, 120, 30);
        assert_eq!(conversation_area(&app).y, 3);
    }

    #[test]
    fn header_actions_share_one_row_and_follow_the_sidebar_right_edge() {
        for (width, height) in [(90, 24), (110, 30), (140, 38), (160, 40)] {
            let app = selected_app();
            let terminal = render(&app, width, height);
            let hits = app.wb.hits.borrow();
            let actions: Vec<_> = [
                Target::Command("/new"),
                Target::Command("/add-agent"),
                Target::Command("/tasks"),
                Target::Menu,
            ]
            .iter()
            .map(|target| hits.iter().find(|hit| &hit.target == target).unwrap())
            .collect();
            assert!(actions.iter().all(|hit| hit.area.y == 0));
            assert_eq!(actions.last().unwrap().area.right(), width - 2);
            assert!(actions
                .windows(2)
                .all(|pair| pair[0].area.right() < pair[1].area.x));
            assert!(!row_text(&terminal, 1).contains("panel"));
            assert!(!row_text(&terminal, 1).contains("Help"));
            assert_eq!(
                hits.iter()
                    .find(|hit| hit.target == Target::Command("/help"))
                    .unwrap()
                    .area
                    .y,
                height - 1
            );
        }
    }

    #[test]
    fn compact_header_keeps_one_control_row_and_help_stays_reachable() {
        for (width, height) in [(20, 8), (40, 16), (80, 24), (120, 16)] {
            let app = selected_app();
            let terminal = render(&app, width, height);
            let hits = app.wb.hits.borrow();
            assert!(hits
                .iter()
                .any(|hit| hit.target == Target::Menu && hit.area.y == 1));
            assert!(!row_text(&terminal, 1).contains("panel"));
            if width >= 40 {
                assert!(hits
                    .iter()
                    .any(|hit| hit.target == Target::Command("/help") && hit.area.y == height - 1));
            }
        }
    }

    #[test]
    fn multiline_draft_height_does_not_follow_the_cursor() {
        for (width, height) in [(40, 16), (80, 24), (120, 30)] {
            let mut app = selected_app();
            app.insert_text("中文👩‍💻\nline two\nline three\nline four\nline five\nline six");
            let draft = app.input.clone();
            render(&app, width, height);
            let before = conversation_area(&app);
            app.wb.cursor = 0;
            let mut terminal = render(&app, width, height);
            assert_eq!(conversation_area(&app), before);
            assert_eq!(app.input, draft);
            let cursor = terminal.get_cursor_position().unwrap();
            assert!(cursor.x < width && cursor.y < height);
            assert!(before.height >= height / 3);
        }
    }

    #[test]
    fn cell_ellipsis_preserves_graphemes_and_never_exceeds_its_budget() {
        for text in ["中文标题", "a👩‍💻b", "e\u{301}abcdef", "abcdef"] {
            for width in 0..12 {
                let fitted = fit_text(text, width);
                assert!(fitted.width() <= width, "{text:?} => {fitted:?}");
            }
        }
        assert_eq!(fit_text("中文标题", 5), "中文…");
        assert_eq!(fit_text("a👩‍💻b", 3), "a…");
        assert_eq!(fit_text("e\u{301}abcdef", 3), "e\u{301}a…");
    }

    #[test]
    fn long_titles_leave_recipient_and_queue_counts_visible() {
        for (width, height) in [(40, 16), (80, 24), (120, 30)] {
            let mut app = selected_app();
            let id = app.selected_session_id().unwrap();
            app.apply_title(id, "很长的会话标题👩‍💻".repeat(30), 2);
            app.wb
                .queues
                .entry(id)
                .or_default()
                .push_back(crate::workbench::Prompt {
                    text: "queued message".into(),
                    references: vec![],
                });
            let terminal = render(&app, width, height);
            let offset = if compact(Rect::new(0, 0, width, height)) {
                3
            } else {
                4
            };
            let text = row_text(&terminal, height - offset);
            assert!(text.contains("To: Mock #1"), "{text}");
            assert!(text.contains("1 queued"), "{text}");
            assert!(text.contains('…'), "{text}");
        }
    }

    #[test]
    fn long_agent_names_keep_the_instance_ordinal() {
        let mut app = selected_app();
        app.sessions[0].agent_name = "中文长名称👩‍💻".repeat(10);
        let terminal = render(&app, 40, 16);
        let text = row_text(&terminal, 13);
        assert!(text.contains("To:"), "{text}");
        assert!(text.contains("#1"), "{text}");
        assert!(text.contains('…'), "{text}");
    }

    #[test]
    fn footer_fits_full_idle_hint_and_prioritizes_long_status_over_buttons() {
        let mut app = selected_app();
        let terminal = render(&app, 80, 24);
        let text = row_text(&terminal, 23);
        assert!(text.contains("Enter send · Alt+Enter new line"), "{text}");
        assert!(text.contains("Latest"), "{text}");
        app.wb.connected = false;
        app.set_status(
            "Daemon disconnected; draft kept; reconnect before sending another message.",
        );
        let terminal = render(&app, 80, 24);
        let text = row_text(&terminal, 23);
        assert!(
            text.contains("○ Daemon disconnected; draft kept; reconnect"),
            "{text}"
        );
        assert!(!app
            .wb
            .hits
            .borrow()
            .iter()
            .any(|h| h.target == Target::Command("/tools")));
    }

    #[test]
    fn tiny_window_keeps_menu_editor_and_send_even_with_pending_permissions() {
        let mut app = selected_app();
        app.handle_event(agentmux_core::Event {
            session_id: app.selected_session_id().unwrap(),
            seq: 1,
            ts: Utc::now(),
            kind: agentmux_core::EventKind::PermissionRequest {
                request_id: "request".into(),
                request: serde_json::json!({"toolCall": {"title": "run tests"}}),
            },
        });
        let terminal = render(&app, 20, 8);
        let hits = app.wb.hits.borrow();
        assert!(hits.iter().any(|h| h.target == Target::Menu));
        assert!(hits
            .iter()
            .any(|h| matches!(h.target, Target::Editor { .. })));
        assert!(hits
            .iter()
            .any(|h| h.target == Target::Key(crossterm::event::KeyCode::Enter)));
        assert!(hits
            .iter()
            .all(|h| h.area.intersection(Rect::new(0, 0, 20, 8)) == h.area));
        assert!(row_text(&terminal, 5).contains("To: Mock #1"));
    }

    #[test]
    fn resize_rebuilds_controls_and_preserves_unicode_draft() {
        let mut app = selected_app();
        app.insert_text("中文👩‍💻\nsecond line");
        let draft = app.input.clone();
        for (width, height) in [(160, 40), (40, 16), (80, 24), (20, 8), (120, 30)] {
            render(&app, width, height);
            assert_eq!(app.input, draft);
            let hits = app.wb.hits.borrow();
            let area = Rect::new(0, 0, width, height);
            assert!(hits.iter().all(|h| h.area.intersection(area) == h.area));
            assert!(hits.iter().any(|h| h.target == Target::Menu));
            assert_eq!(
                hits.iter().any(|h| matches!(h.target, Target::Tab(_))),
                width >= 110
            );
        }
    }

    #[test]
    fn editor_wraps_cjk_and_emoji_by_terminal_cells() {
        let text = "中👩‍💻文";
        let (lines, row, col) = editor_lines(text, text.len(), 4);
        assert_eq!(lines, vec!["中👩‍💻", "文"]);
        assert_eq!((row, col), (1, 2));
        let (_, row, col) = editor_lines("abcd", 4, 4);
        assert_eq!((row, col), (1, 0));
    }
    #[test]
    fn responsive_shell_and_cursor_fit_small_and_large_terminals() {
        for (w, h) in [(20, 8), (40, 12), (80, 24), (120, 30), (160, 40)] {
            let mut app = App::new(vec![], vec![], vec![], vec![]);
            app.insert_text("中文👩‍💻\nlong draft");
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw(f, &app)).unwrap();
            let cursor = term.get_cursor_position().unwrap();
            assert!(cursor.x < w && cursor.y < h, "{w}x{h}: {cursor:?}");
            let text: String = term
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert_eq!(text.contains(" Agents   Files "), w >= 110);
        }
    }
}
