//! Layout + drawing. Reads [`App`](crate::app::App) only — never mutates.
//!
//! ```text
//! ╭─ sessions ──────╮─ claude·a1b2c3d4 · ready ───╮
//! │ ▸ alpha         │  ● claude          12:01:04 │
//! │   ● claude·…    │    hello world              │
//! │   ◐ codex·…  •  │  12:01:05 ⚙ edit — completed│
//! │ ▸ beta          │    src/lib.rs               │
//! ╰─────────────────┴─────────────────────────────╯
//! │ ● normal                        hints · · ·   │  status bar
//! ╰───────────────────────────────────────────────╯
//! ╭ prompt ───────────────────────────────────────╮
//! │                                               │
//! ╰───────────────────────────────────────────────╯
//! ```
//!
//! All color lives in [`crate::theme`] — this file only names roles
//! (`THEME.accent`, `THEME.faint`, …), never raw `Color`s.

use agentmux_core::{Event, EventKind};
use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Frame;

use crate::app::{permission_summary, short_id, App, InputMode, RelayStage, SessionView};
use crate::newsession::WizardStep;
use crate::theme::THEME;
use ratatui::style::Style;

/// Width of the left session-list column — wide enough for
/// `● agent·id state` plus borders and the highlight symbol.
const LIST_WIDTH: u16 = 36;

/// Indent of message bodies and tool-call detail lines.
const BODY_INDENT: &str = "  ";

/// Upper bound on the lines a single event block may emit — a giant
/// diff or paste can't flood the viewport-bound scan.
const MAX_BLOCK_LINES: usize = 48;

/// Render the whole UI.
pub fn draw(frame: &mut Frame, app: &App) {
    let vertical = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1), // status bar
        Constraint::Length(3), // input box
    ])
    .split(frame.area());
    let body = Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Min(20)])
        .split(vertical[0]);

    draw_sessions(frame, app, body[0]);
    draw_body(frame, app, body[1]);
    draw_status(frame, app, vertical[1]);
    draw_input(frame, app, vertical[2]);

    // Modal overlays draw last, on top of a dimmed UI.
    match app.mode {
        InputMode::NewSession => draw_wizard(frame, app),
        InputMode::Permission => draw_permission(frame, app),
        _ => {}
    }
}

/// A bordered pane with the house style: rounded corners, faint border,
/// dim title text.
fn pane<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(THEME.border)
        .title(title)
}

/// Wash the whole frame in the backdrop shade — the cheap "dim" behind
/// modal overlays (bg-only style merges over the drawn UI).
fn draw_backdrop(frame: &mut Frame) {
    frame.render_widget(Block::default().style(THEME.backdrop), frame.area());
}

/// The right pane: relay event picker, touched-files list, or the
/// bottom-anchored event stream.
fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    if let (InputMode::RelayPick, Some(pick)) = (app.mode, app.relay.as_ref()) {
        if pick.stage == RelayStage::Event {
            draw_relay_events(frame, app, area, pick.event_cursor);
            return;
        }
    }
    if app.show_diff {
        draw_files(frame, app, area);
        return;
    }
    draw_events(frame, app, area);
}

// --- sessions (left pane) ---------------------------------------------------

/// Left pane: session list grouped by workspace, state badge per row.
///
/// During the relay pick's `Session` stage the highlight follows the
/// picker's `session_cursor` instead of `app.selected` — the relayed
/// reference will go to the cursor's session, so that's what must be
/// visually hot.
fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_row: Option<usize> = None;
    let mut covered = vec![false; app.sessions.len()];
    let highlight = match (app.mode, app.relay.as_ref()) {
        (InputMode::RelayPick, Some(pick)) if pick.stage == RelayStage::Session => {
            pick.session_cursor
        }
        _ => app.selected,
    };
    // `•` marks activity on sessions that aren't being viewed.
    let unread =
        |i: usize| -> bool { i != highlight && app.unread.contains(&app.sessions[i].session.id) };

    for ws in &app.workspaces {
        let mut header = false;
        for (i, view) in app.sessions.iter().enumerate() {
            if view.session.workspace_id != ws.id {
                continue;
            }
            if !header {
                items.push(workspace_header(&ws.name));
                header = true;
            }
            covered[i] = true;
            if i == highlight {
                selected_row = Some(items.len());
            }
            items.push(session_line(view, unread(i)));
        }
        // Workspaces with no sessions still render — `n` can land there.
        if !header {
            items.push(workspace_header(&ws.name));
        }
    }

    // Sessions whose workspace isn't in `app.workspaces` (created while
    // the list was stale, …) group under their recorded workspace names.
    let mut extra: Vec<(&str, Vec<usize>)> = Vec::new();
    for (i, view) in app.sessions.iter().enumerate() {
        if covered[i] {
            continue;
        }
        match extra
            .iter_mut()
            .find(|(name, _)| *name == view.workspace_name)
        {
            Some((_, group)) => group.push(i),
            None => extra.push((view.workspace_name.as_str(), vec![i])),
        }
    }
    for (name, group) in extra {
        items.push(workspace_header(name));
        for i in group {
            if i == highlight {
                selected_row = Some(items.len());
            }
            items.push(session_line(&app.sessions[i], unread(i)));
        }
    }

    if items.is_empty() {
        for l in [
            Line::from(Span::styled("  no sessions yet", THEME.dim)),
            Line::default(),
            Line::from(vec![
                Span::styled("  n", THEME.accent),
                Span::styled("  new session", THEME.faint),
            ]),
            Line::from(vec![
                Span::styled("  q", THEME.accent),
                Span::styled("  quit", THEME.faint),
            ]),
        ] {
            items.push(ListItem::new(l));
        }
    }

    let title = match (app.mode, app.relay.as_ref()) {
        (InputMode::RelayPick, Some(pick)) if pick.stage == RelayStage::Session => {
            Line::from(Span::styled(" relay — pick target ", THEME.special))
        }
        _ => Line::from(Span::styled(" sessions ", THEME.title)),
    };
    let list = List::new(items)
        .block(pane(title))
        .highlight_style(THEME.selection)
        .highlight_symbol("›");
    let mut state = ListState::default();
    state.select(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
}

/// `▸ workspace` — a quiet section label, not a competing accent.
fn workspace_header(name: &str) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(" ▸ ", THEME.faint),
        Span::styled(name.to_string(), THEME.section),
    ]))
}

/// `● agent·id state [•]` — badge color is the state, name is text,
/// the state label stays secondary.
fn session_line(view: &SessionView, unread: bool) -> ListItem<'static> {
    let (glyph, label) = App::badge(&view.session.state);
    let id = view.session.id.to_string();
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(glyph.to_string(), THEME.badge(&view.session.state)),
        Span::styled(format!(" {}", view.agent_name), THEME.text),
        Span::styled(format!("·{} ", short_id(&id)), THEME.faint),
        Span::styled(label, THEME.dim),
    ];
    if unread {
        spans.push(Span::styled("  •", THEME.accent));
    }
    ListItem::new(Line::from(spans))
}

// --- event stream (right pane) ----------------------------------------------

/// Right pane: the selected session's event stream, bottom-anchored.
fn draw_events(frame: &mut Frame, app: &App, area: Rect) {
    let agent = app
        .selected_session()
        .map(|v| v.agent_name.as_str())
        .unwrap_or("agentmux");
    let title: Line = match app.selected_session() {
        Some(view) => {
            let (glyph, label) = App::badge(&view.session.state);
            Line::from(vec![
                Span::styled(format!(" {glyph}"), THEME.badge(&view.session.state)),
                Span::styled(format!(" {}", view.agent_name), THEME.title),
                Span::styled(
                    format!("·{}", short_id(&view.session.id.to_string())),
                    THEME.faint,
                ),
                Span::styled(format!("  {label} "), THEME.faint),
            ])
        }
        None => Line::from(Span::styled(" events ", THEME.title)),
    };
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let inner_height = area.height.saturating_sub(2).max(1) as usize;

    // Bound per-frame work to the visible region: scan the log backwards
    // and stop once the collected events cover the viewport in wrapped
    // rows. The per-event estimate counts a message's header even though
    // merging may drop it later — never an under-count of its own body.
    // `max_blocks` is the hard work bound: ~4 viewports of events is
    // plenty of slack for merge-shrink top-ups without ever letting a
    // pathological log turn drawing into O(log) work.
    let mut iter = app.events_for_selected().rev();
    let mut blocks: Vec<EventBlock> = Vec::new(); // newest first
    let mut exhausted = false;
    let mut est = 0usize;
    let max_blocks = inner_height.saturating_mul(4).max(16);
    while est < inner_height && blocks.len() < max_blocks {
        match iter.next() {
            Some(ev) => {
                let block = event_block(ev);
                est += block_rows(&block, inner_width);
                blocks.push(block);
            }
            None => {
                exhausted = true;
                break;
            }
        }
    }

    // Render oldest → newest. Merging makes the render *shorter* than
    // the estimate (N chunks share one header), so when it comes up
    // short of the viewport keep pulling events and re-rendering until
    // the pane fills, the log runs dry, or the block cap hits.
    let mut lines = render_blocks(blocks.iter().rev(), agent);
    let mut rows: usize = lines.iter().map(|l| wrapped_rows(l, inner_width)).sum();
    while rows < inner_height && !exhausted && blocks.len() < max_blocks {
        for _ in 0..inner_height - rows {
            match iter.next() {
                Some(ev) => blocks.push(event_block(ev)),
                None => {
                    exhausted = true;
                    break;
                }
            }
            if blocks.len() >= max_blocks {
                break;
            }
        }
        lines = render_blocks(blocks.iter().rev(), agent);
        rows = lines.iter().map(|l| wrapped_rows(l, inner_width)).sum();
    }

    // Bottom-anchor in wrapped rows: `.scroll` offsets count *post-wrap*
    // rows, so subtracting `lines.len()` (source lines) under-scrolls
    // whenever any line wraps and the newest events end up below the
    // fold. `rows` is the true wrapped-row count instead.
    let scroll = u16::try_from(rows.saturating_sub(inner_height)).unwrap_or(u16::MAX);

    let lines = if lines.is_empty() {
        events_empty_lines(app)
    } else {
        lines
    };
    let paragraph = Paragraph::new(lines)
        .block(pane(title))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// The event pane when it has nothing to show — a hint, not a void.
fn events_empty_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    if app.selected_session().is_none() {
        lines.extend([
            Line::from(Span::styled("  no session selected", THEME.dim)),
            Line::from(vec![
                Span::styled("  n", THEME.accent),
                Span::styled("  new session", THEME.faint),
            ]),
        ]);
    } else {
        lines.push(Line::from(Span::styled(
            "  waiting for output…",
            THEME.faint_italic,
        )));
    }
    lines
}

// --- event rendering ---------------------------------------------------------

/// Which prose role a `session/update` message chunk plays — drives the
/// header styling and whether consecutive chunks merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MsgRole {
    /// `agent_message_chunk` — the agent's visible reply.
    Agent,
    /// `agent_thought_chunk` — internal reasoning, rendered dimmer.
    Thought,
    /// `user_message_chunk` — the operator's side of the stream.
    User,
}

/// One event, as renderable material. `Msg` stays un-rendered so a run
/// of consecutive same-role chunks can coalesce under one header.
enum EventBlock {
    Msg {
        role: MsgRole,
        ts: DateTime<Utc>,
        text: String,
    },
    Static(Vec<Line<'static>>),
}

/// The wrapped rows `block` will occupy when rendered alone — the phase-1
/// scan bound. `Msg` counts its header plus per-source-line body wrap,
/// an over-estimate of the merged contribution (merging drops headers);
/// the top-up pass in `draw_events` corrects the resulting shortfall.
fn block_rows(block: &EventBlock, width: usize) -> usize {
    match block {
        EventBlock::Static(lines) => lines.iter().map(|l| wrapped_rows(l, width)).sum(),
        EventBlock::Msg { text, .. } => {
            1 + text
                .lines()
                .map(|l| wrapped_rows(&Line::from(l), width))
                .sum::<usize>()
                .max(1)
        }
    }
}

/// Render collected blocks (oldest → newest) to lines, coalescing
/// consecutive same-role message chunks into one header + flowing prose
/// body (agents stream replies as many small chunks — one header each
/// would drown the text). Borrows the blocks so the caller can top-up
/// the collection and re-render.
fn render_blocks<'a>(
    blocks: impl Iterator<Item = &'a EventBlock>,
    agent: &str,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    let mut pending: Option<(MsgRole, DateTime<Utc>, String)> = None;
    for block in blocks {
        match block {
            EventBlock::Msg { role, ts, text } => match &mut pending {
                Some((r, _, buf)) if *r == *role => {
                    buf.push_str(text);
                }
                _ => {
                    flush_msg(&mut out, pending.take(), agent);
                    pending = Some((*role, *ts, text.clone()));
                }
            },
            EventBlock::Static(lines) => {
                flush_msg(&mut out, pending.take(), agent);
                out.extend(lines.iter().cloned());
            }
        }
    }
    flush_msg(&mut out, pending.take(), agent);
    out
}

/// Emit a coalesced message run: `● agent  HH:MM:SS` header, then the
/// prose body indented (with diff detection for pasted patches).
fn flush_msg(
    out: &mut Vec<Line<'static>>,
    pending: Option<(MsgRole, DateTime<Utc>, String)>,
    agent: &str,
) {
    let Some((role, ts, text)) = pending else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    let ts = ts.format("%H:%M:%S").to_string();
    let header = match role {
        MsgRole::Agent => vec![
            Span::styled("● ", THEME.accent),
            Span::styled(agent.to_string(), THEME.accent_bold),
            Span::styled(format!("  {ts}"), THEME.faint),
        ],
        MsgRole::Thought => vec![
            Span::styled("◌ ", THEME.faint),
            Span::styled(format!("{agent} thinking"), THEME.dim_italic),
            Span::styled(format!("  {ts}"), THEME.faint),
        ],
        MsgRole::User => vec![
            Span::styled("○ ", THEME.special),
            Span::styled("you", THEME.special_bold),
            Span::styled(format!("  {ts}"), THEME.faint),
        ],
    };
    out.push(Line::from(header));
    let body_style = match role {
        MsgRole::Thought => THEME.dim_italic,
        _ => THEME.text,
    };
    // Bottom-anchored stream: an over-long message keeps its tail.
    out.extend(body_lines(&text, body_style, true));
}

/// One event → its renderable block (timestamped; `Msg` defers text).
fn event_block(ev: &Event) -> EventBlock {
    match &ev.kind {
        EventKind::SessionUpdate(v) => session_update_block(ev, v),
        EventKind::StateChanged { from, to } => {
            let (_, from_label) = App::badge(from);
            let (_, to_label) = App::badge(to);
            EventBlock::Static(vec![Line::from(vec![
                ts_span(ev),
                Span::styled("· ", THEME.faint),
                Span::styled(format!("{from_label} → {to_label}"), THEME.badge(to)),
            ])])
        }
        EventKind::FileEdited { path } => EventBlock::Static(vec![Line::from(vec![
            ts_span(ev),
            Span::styled("✎ ", THEME.accent),
            Span::styled("edited ", THEME.dim),
            Span::styled(path.display().to_string(), THEME.text),
        ])]),
        EventKind::AgentExited { code } => EventBlock::Static(vec![Line::from(vec![
            ts_span(ev),
            Span::styled("✗ ", THEME.error),
            Span::styled(format!("agent exited (code {code:?})"), THEME.error),
        ])]),
        EventKind::Orchestrator(msg) => EventBlock::Static(vec![Line::from(vec![
            ts_span(ev),
            Span::styled("· ", THEME.faint),
            Span::styled(msg.clone(), THEME.dim_italic),
        ])]),
        EventKind::PermissionRequest { request, .. } => EventBlock::Static(vec![Line::from(vec![
            ts_span(ev),
            Span::styled("⚠ ", THEME.warning),
            Span::styled(
                format!("permission requested — {}", permission_summary(request)),
                THEME.warning_bold,
            ),
        ])]),
        EventKind::PermissionResolved { outcome, .. } => {
            EventBlock::Static(vec![Line::from(vec![
                ts_span(ev),
                Span::styled("⚠ ", THEME.faint),
                Span::styled(format!("permission {outcome}"), THEME.dim),
            ])])
        }
    }
}

/// `session/update` payloads → blocks: message chunks stay `Msg` for
/// coalescing; tool calls and friends render as structured cards.
fn session_update_block(ev: &Event, value: &serde_json::Value) -> EventBlock {
    let update = value.get("update").unwrap_or(value);
    let text = || {
        update
            .pointer("/content/text")
            .or_else(|| update.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string()
    };
    match update.get("sessionUpdate").and_then(|t| t.as_str()) {
        Some("agent_message_chunk") => EventBlock::Msg {
            role: MsgRole::Agent,
            ts: ev.ts,
            text: text(),
        },
        Some("agent_thought_chunk") => EventBlock::Msg {
            role: MsgRole::Thought,
            ts: ev.ts,
            text: text(),
        },
        Some("user_message_chunk") => EventBlock::Msg {
            role: MsgRole::User,
            ts: ev.ts,
            text: text(),
        },
        Some("tool_call") | Some("tool_call_update") => {
            EventBlock::Static(tool_call_lines(ev, update))
        }
        Some("plan") => EventBlock::Static(plan_lines(ev, update)),
        Some("available_commands_update") => {
            EventBlock::Static(vec![available_commands_line(ev, update)])
        }
        Some("current_mode_update") => EventBlock::Static(vec![Line::from(vec![
            ts_span(ev),
            Span::styled("⇄ ", THEME.special),
            Span::styled("mode → ", THEME.dim),
            Span::styled(
                update
                    .get("currentModeId")
                    .and_then(|m| m.as_str())
                    .unwrap_or("?")
                    .to_string(),
                THEME.text,
            ),
        ])]),
        _ => EventBlock::Static(vec![fallback_update_line(ev, update)]),
    }
}

/// `HH:MM:SS ` in the faint gutter color — the shared line prefix.
fn ts_span(ev: &Event) -> Span<'static> {
    Span::styled(format!("{} ", ev.ts.format("%H:%M:%S")), THEME.faint)
}

/// A `tool_call`/`tool_call_update` as a card: `⚙ title · status`
/// header, then locations, diff hunks and text content underneath.
fn tool_call_lines(ev: &Event, update: &serde_json::Value) -> Vec<Line<'static>> {
    // title → kind → toolCallId: the most specific human-readable name
    // wins; a bare `kind` ("edit") still beats a raw call id.
    let title = update
        .get("title")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            update
                .get("kind")
                .and_then(|k| k.as_str())
                .filter(|k| !k.is_empty())
        })
        .or_else(|| {
            update
                .get("toolCallId")
                .and_then(|t| t.as_str())
                .map(short_id)
        })
        .unwrap_or("tool call")
        .to_string();
    let status = update.get("status").and_then(|s| s.as_str());

    let mut header = vec![ts_span(ev), Span::styled("⚙ ", THEME.accent)];
    header.push(Span::styled(title, THEME.text));
    if let Some(status) = status {
        header.push(Span::styled(
            format!("  {status}"),
            THEME.tool_status(Some(status)),
        ));
    } else if update.get("sessionUpdate").and_then(|t| t.as_str()) == Some("tool_call_update") {
        header.push(Span::styled("  update", THEME.faint));
    }
    let mut lines = vec![Line::from(header)];

    if let Some(locations) = update.get("locations").and_then(|l| l.as_array()) {
        for loc in locations.iter().take(MAX_BLOCK_LINES) {
            let Some(path) = loc.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            let line_no = loc.get("line").and_then(|l| l.as_u64());
            let target = match line_no {
                Some(n) => format!("{path}:{n}"),
                None => path.to_string(),
            };
            lines.push(Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled("⌄ ", THEME.faint),
                Span::styled(target, THEME.dim),
            ]));
        }
    }

    if let Some(content) = update.get("content").and_then(|c| c.as_array()) {
        for item in content {
            // One card never floods the pane: stop once the cap is hit.
            if lines.len() >= MAX_BLOCK_LINES {
                break;
            }
            match item.get("type").and_then(|t| t.as_str()) {
                Some("diff") => {
                    let path = item.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                    lines.extend(diff_lines(
                        path,
                        item.get("oldText").and_then(|t| t.as_str()),
                        item.get("newText").and_then(|t| t.as_str()).unwrap_or(""),
                    ));
                }
                Some("content") => {
                    if let Some(text) = item.pointer("/content/text").and_then(|t| t.as_str()) {
                        lines.extend(body_lines(text, THEME.dim, false));
                    }
                }
                Some("terminal") => lines.push(Line::from(vec![
                    Span::raw(BODY_INDENT),
                    Span::styled("terminal ", THEME.faint),
                    Span::styled(
                        item.get("terminalId")
                            .and_then(|t| t.as_str())
                            .unwrap_or("?")
                            .to_string(),
                        THEME.dim,
                    ),
                ])),
                _ => {}
            }
        }
    }
    cap_head(lines)
}

/// A `plan` update: header plus one status-glyph row per entry.
fn plan_lines(ev: &Event, update: &serde_json::Value) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        ts_span(ev),
        Span::styled("◇ ", THEME.accent),
        Span::styled("plan", THEME.title),
    ])];
    if let Some(entries) = update.get("entries").and_then(|e| e.as_array()) {
        for entry in entries.iter().take(MAX_BLOCK_LINES) {
            let status = entry.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let (glyph, style) = match status {
                "completed" => ("✓", THEME.success),
                "in_progress" => ("◐", THEME.warning),
                _ => ("○", THEME.faint),
            };
            let text = entry
                .get("content")
                .or_else(|| entry.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            lines.push(Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled(format!("{glyph} "), style),
                Span::styled(text.to_string(), THEME.dim),
            ]));
        }
    }
    cap_head(lines)
}

/// `available_commands_update` → one dim summary line.
fn available_commands_line(ev: &Event, update: &serde_json::Value) -> Line<'static> {
    let names: Vec<&str> = update
        .get("availableCommands")
        .and_then(|c| c.as_array())
        .map(|cmds| {
            cmds.iter()
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let shown: Vec<&str> = names.iter().take(6).copied().collect();
    let more = if names.len() > 6 {
        format!(" +{}", names.len() - 6)
    } else {
        String::new()
    };
    Line::from(vec![
        ts_span(ev),
        Span::styled("⌘ ", THEME.special),
        Span::styled(
            format!("commands: {}{}", shown.join(" · "), more),
            THEME.dim,
        ),
    ])
}

/// Unknown payloads: a terse, truncated summary — never a JSON wall.
fn fallback_update_line(ev: &Event, update: &serde_json::Value) -> Line<'static> {
    Line::from(vec![
        ts_span(ev),
        Span::styled(session_update_text(update), THEME.faint),
    ])
}

/// Prose body lines: `BODY_INDENT`-indented in `style`, or diff-colored
/// when the text looks like a patch. `tail` selects the cap direction —
/// message bodies keep their newest lines, tool output keeps its head.
/// At most `MAX_BLOCK_LINES + 1` source lines are ever materialised.
fn body_lines(text: &str, style: Style, tail: bool) -> Vec<Line<'static>> {
    if looks_like_diff(text) {
        return patch_lines(text, tail);
    }
    let mk = |l: &str| {
        Line::from(vec![
            Span::raw(BODY_INDENT),
            Span::styled(l.to_string(), style),
        ])
    };
    // Early bound either way: tail keeps the LAST `MAX + 1` source
    // lines (scan the end of the text, not the start), head keeps the
    // first — neither materialises a full paste.
    let mut lines: Vec<Line> = if tail {
        let mut v: Vec<Line> = text
            .lines()
            .rev()
            .take(MAX_BLOCK_LINES + 1)
            .map(mk)
            .collect();
        v.reverse();
        v
    } else {
        text.lines().take(MAX_BLOCK_LINES + 1).map(mk).collect()
    };
    if lines.is_empty() && !text.is_empty() {
        lines.push(Line::from(Span::styled(text.to_string(), style)));
    }
    if tail {
        cap_tail(lines)
    } else {
        cap_head(lines)
    }
}

/// Whether a text block is a unified-diff-shaped patch: an explicit
/// `diff --git`/`@@`/`Index:` marker, or the `---`/`+++` header pair
/// (a lone `---` is more likely a markdown rule, so it needs its mate).
fn looks_like_diff(text: &str) -> bool {
    let mut saw_old = false;
    let mut saw_new = false;
    for l in text.lines() {
        if l.starts_with("diff --git") || l.starts_with("@@") || l.starts_with("Index:") {
            return true;
        }
        saw_old |= l.starts_with("--- ");
        saw_new |= l.starts_with("+++ ");
    }
    saw_old && saw_new
}

/// Render a unified-diff-ish text block: `+` lines green, `-` lines red,
/// `@@`/`diff`/`index`/`---`/`+++` headers accent/faint, context dim.
fn patch_lines(text: &str, tail: bool) -> Vec<Line<'static>> {
    // Same bound as `body_lines`: tail semantics scan the LAST lines.
    let source: Vec<&str> = if tail {
        let mut v: Vec<&str> = text.lines().rev().take(MAX_BLOCK_LINES + 1).collect();
        v.reverse();
        v
    } else {
        text.lines().take(MAX_BLOCK_LINES + 1).collect()
    };
    let mut lines = Vec::new();
    for l in source {
        let style = if l.starts_with("+++") || l.starts_with("---") {
            THEME.faint
        } else if l.starts_with('+') {
            THEME.success
        } else if l.starts_with('-') {
            THEME.error
        } else if l.starts_with("@@") || l.starts_with("diff --git") || l.starts_with("index ") {
            THEME.accent
        } else {
            THEME.dim
        };
        lines.push(Line::from(vec![
            Span::raw(BODY_INDENT),
            Span::styled(l.to_string(), style),
        ]));
    }
    if tail {
        cap_tail(lines)
    } else {
        cap_head(lines)
    }
}

/// An ACP `diff` content item: highlighted path header, then `-` old
/// lines and `+` new lines (a pseudo-diff — ACP ships old/new text, not
/// a unified patch, so line-level +/- is the honest rendering).
fn diff_lines(path: &str, old: Option<&str>, new: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::raw(BODY_INDENT),
        Span::styled("± ", THEME.special),
        Span::styled(path.to_string(), THEME.accent),
    ])];
    if let Some(old) = old {
        for l in old.lines().take(MAX_BLOCK_LINES) {
            lines.push(Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled(format!("-{l}"), THEME.error),
            ]));
        }
    }
    for l in new.lines().take(MAX_BLOCK_LINES) {
        lines.push(Line::from(vec![
            Span::raw(BODY_INDENT),
            Span::styled(format!("+{l}"), THEME.success),
        ]));
    }
    lines
}

/// The ellipsis row a capped block ends (head-cap) or begins
/// (tail-cap) with.
fn cap_note(tail: bool) -> Line<'static> {
    let text = if tail {
        "… earlier lines"
    } else {
        "… more lines"
    };
    Line::from(vec![
        Span::raw(BODY_INDENT),
        Span::styled(text, THEME.faint),
    ])
}

/// Bound a block's height keeping its HEAD — for card-ish content
/// (tool calls, plans) the header must stay visible.
fn cap_head(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() > MAX_BLOCK_LINES {
        lines.truncate(MAX_BLOCK_LINES - 1);
        lines.push(cap_note(false));
    }
    lines
}

/// Bound a block's height keeping its TAIL — in a bottom-anchored
/// stream the newest lines are the ones that matter.
fn cap_tail(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() > MAX_BLOCK_LINES {
        let dropped = lines.len() - (MAX_BLOCK_LINES - 1);
        let mut kept = lines.split_off(dropped);
        kept.insert(0, cap_note(true));
        return kept;
    }
    lines
}

/// Rows `line` occupies under `Wrap { trim: false }` at `width` columns —
/// a greedy word-wrap estimate: words pack into a row until the next
/// word would overflow, over-wide words split mid-word. Also floored at
/// `ceil(display-width / width)` so wide-grapheme (CJK/emoji) text — for
/// which the char-count simulation undercounts — still wraps right.
fn wrapped_rows(line: &Line, width: usize) -> usize {
    let width = width.max(1);
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut rows = 1usize;
    let mut col = 0usize;
    for (i, word) in text.split(' ').enumerate() {
        let w = word.chars().count();
        let sep = usize::from(i > 0);
        if col > 0 && col + sep + w > width {
            // The word moves to a fresh row; its separator stays behind.
            rows += 1;
            col = 0;
        } else {
            col += sep;
        }
        col += w;
        while col > width {
            rows += 1;
            col -= width;
        }
    }
    rows.max(line.width().div_ceil(width))
}

/// A compact one-line rendering of an event — for the relay picker's
/// highlightable list (one list row per event).
fn event_line(ev: &Event) -> Line<'static> {
    match event_block(ev) {
        EventBlock::Static(mut lines) => {
            if lines.is_empty() {
                Line::default()
            } else {
                lines.remove(0)
            }
        }
        EventBlock::Msg { role, text, .. } => {
            let (glyph, style) = match role {
                MsgRole::Agent => ("●", THEME.accent),
                MsgRole::Thought => ("◌", THEME.faint),
                MsgRole::User => ("○", THEME.special),
            };
            let first = text.lines().next().unwrap_or("");
            let first: String = first.chars().take(120).collect();
            Line::from(vec![
                ts_span(ev),
                Span::styled(format!("{glyph} "), style),
                Span::styled(first, THEME.dim),
            ])
        }
    }
}

/// RelayPick stage `Event`: the selected session's event log as a
/// highlightable list (newest at the bottom, cursor pre-placed there).
fn draw_relay_events(frame: &mut Frame, app: &App, area: Rect, cursor: usize) {
    let items: Vec<ListItem> = app
        .session_events()
        .map(|ev| ListItem::new(event_line(ev)))
        .collect();
    let items = if items.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no events yet",
            THEME.dim,
        )))]
    } else {
        items
    };
    let list = List::new(items)
        .block(pane(Line::from(Span::styled(
            " relay — pick event · enter next · esc abort ",
            THEME.special,
        ))))
        .highlight_style(THEME.selection)
        .highlight_symbol("›");
    let mut state = ListState::default();
    state.select(Some(cursor));
    frame.render_stateful_widget(list, area, &mut state);
}

/// `Tab` diff/files panel: paths the selected session touched
/// (`FileEdited` events + `tool_call` locations), first-touch order.
fn draw_files(frame: &mut Frame, app: &App, area: Rect) {
    let files = app.touched_files();
    let title: Line = match app.selected_session() {
        Some(view) => Line::from(vec![
            Span::styled(" files touched — ", THEME.title),
            Span::styled(
                format!(
                    "{}·{}",
                    view.agent_name,
                    short_id(&view.session.id.to_string())
                ),
                THEME.faint,
            ),
            Span::styled(" ", THEME.faint),
        ]),
        None => Line::from(Span::styled(" files touched ", THEME.title)),
    };
    let items: Vec<ListItem> = if files.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no file activity yet",
            THEME.dim,
        )))]
    } else {
        files
            .iter()
            .map(|f| {
                ListItem::new(Line::from(vec![
                    Span::styled("  ✎ ", THEME.accent),
                    Span::styled(f.clone(), THEME.text),
                ]))
            })
            .collect()
    };
    let list = List::new(items).block(pane(title).title_bottom(Line::from(Span::styled(
        " tab — back to events ",
        THEME.faint,
    ))));
    frame.render_widget(list, area);
}

/// A centered modal rect — for the wizard and the permission notice.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let v = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .split(v[1])[1]
}

/// The `n` wizard overlay: a step-titled picker list (project →
/// workspace → agent) with the create-new-workspace name input inline.
fn draw_wizard(frame: &mut Frame, app: &App) {
    let Some(wiz) = &app.wizard else {
        return;
    };
    let (title, items, cursor): (&str, Vec<ListItem>, Option<usize>) = match wiz.step {
        WizardStep::Project => (
            "pick project",
            app.projects
                .iter()
                .map(|p| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("  {}", p.name), THEME.text),
                        Span::styled(format!("  {}", p.root_path.display()), THEME.faint),
                    ]))
                })
                .collect(),
            Some(wiz.project_cursor),
        ),
        WizardStep::Workspace => {
            let mut items: Vec<ListItem> = wiz
                .workspace_options(app)
                .map(|w| {
                    ListItem::new(Line::from(Span::styled(
                        format!("  {}", w.name),
                        THEME.text,
                    )))
                })
                .collect();
            items.push(ListItem::new(Line::from(vec![
                Span::styled("  + ", THEME.accent),
                Span::styled("create new workspace…", THEME.accent),
            ])));
            ("pick workspace", items, Some(wiz.workspace_cursor))
        }
        WizardStep::WorkspaceName => (
            "name the new workspace",
            vec![
                ListItem::new(Line::from(vec![
                    Span::styled("  name: ", THEME.dim),
                    Span::styled(format!("{}▌", wiz.name), THEME.text),
                ])),
                ListItem::new(Line::from(Span::styled(
                    "  (git worktree under the project)",
                    THEME.faint,
                ))),
            ],
            None,
        ),
        WizardStep::Agent => (
            "pick agent",
            wiz.agent_options(app)
                .map(|a| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("  {}", a.name), THEME.text),
                        Span::styled(format!("  ({})", a.id), THEME.faint),
                    ]))
                })
                .collect(),
            Some(wiz.agent_cursor),
        ),
    };
    let height = (items.len() as u16 + 4).min(frame.area().height.saturating_sub(2));
    let rect = centered(frame.area(), 52, height.max(5));
    draw_backdrop(frame);
    frame.render_widget(Clear, rect);
    let list = List::new(items)
        .block(
            pane(Line::from(Span::styled(
                format!(" new session — {title} "),
                THEME.title,
            )))
            .border_style(THEME.border_focus)
            .title_bottom(Line::from(Span::styled(
                " enter select · esc back ",
                THEME.faint,
            ))),
        )
        .highlight_style(THEME.selection)
        .highlight_symbol("›");
    let mut state = ListState::default();
    state.select(cursor);
    frame.render_stateful_widget(list, rect, &mut state);
}

/// The permission dialog: the agent's request is parked daemon-side —
/// `y`/`a`/`n`/`Esc` send `session/permission`. While the answer is in
/// flight (`pending`) the dialog stays up with an "answering…" row and
/// keys are ignored; the `PermissionResolved` event dismisses it.
fn draw_permission(frame: &mut Frame, app: &App) {
    let Some(notice) = &app.permission else {
        return;
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("⚠ ", THEME.warning),
            Span::styled(notice.summary.clone(), THEME.warning_bold),
        ]),
        Line::default(),
    ];
    if notice.pending {
        lines.push(Line::from(Span::styled("  answering…", THEME.dim_italic)));
    } else {
        let mut hints = vec![
            Span::styled("  y", THEME.warning_bold),
            Span::styled(" allow once", THEME.dim),
        ];
        if notice.allows_always() {
            hints.push(Span::styled(" · ", THEME.faint));
            hints.push(Span::styled("a", THEME.warning_bold));
            hints.push(Span::styled(" always", THEME.dim));
        }
        hints.push(Span::styled(" · ", THEME.faint));
        hints.push(Span::styled("n/esc", THEME.warning_bold));
        hints.push(Span::styled(" reject", THEME.dim));
        lines.push(Line::from(hints));
    }
    let rect = centered(frame.area(), 60, lines.len() as u16 + 2);
    draw_backdrop(frame);
    frame.render_widget(Clear, rect);
    let block = pane(Line::from(Span::styled(
        format!(
            " permission requested — {} ",
            short_id(&notice.session_id.to_string())
        ),
        THEME.title,
    )))
    .border_style(THEME.warning);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Pull human-readable text out of an opaque ACP `session/update` JSON
/// blob: known shapes get their `text` payload, everything else falls
/// back to a compact, truncated dump. (Kept for the compact/relay path
/// and tests; the rich renderer lives in [`session_update_block`].)
fn session_update_text(value: &serde_json::Value) -> String {
    let update = value.get("update").unwrap_or(value);
    let tag = update.get("sessionUpdate").and_then(|t| t.as_str());
    if let Some(text) = update
        .pointer("/content/text")
        .or_else(|| update.get("text"))
        .and_then(|t| t.as_str())
    {
        return text.to_string();
    }
    let dump = serde_json::to_string(update).unwrap_or_else(|_| "<?>".to_string());
    let dump = if dump.chars().count() > 160 {
        let truncated: String = dump.chars().take(160).collect();
        format!("{truncated}…")
    } else {
        dump
    };
    match tag {
        Some(tag) => format!("[{tag}] {dump}"),
        None => dump,
    }
}

/// Status bar: mode chip left, transient status message, key hints
/// parked at the right edge — all deliberately quiet.
fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let mode_label = match app.mode {
        InputMode::Normal => "normal",
        InputMode::Editing => "editing",
        InputMode::RelayPick => "relay",
        InputMode::NewSession => "new",
        InputMode::Permission => "permission",
    };
    let hints = match app.mode {
        InputMode::Normal => {
            "q quit · j/k select · i prompt · n new · @ relay · x kill · r resume · tab files · ^c cancel"
        }
        InputMode::Editing => "enter send · esc normal",
        InputMode::RelayPick => match app.relay.as_ref().map(|r| r.stage) {
            Some(RelayStage::Event) => "j/k pick event · enter next · esc abort",
            _ => "j/k pick target · enter relay · esc abort",
        },
        InputMode::NewSession => "enter select · esc back",
        InputMode::Permission => match app.permission.as_ref() {
            Some(n) if n.pending => "answering…",
            Some(n) if n.allows_always() => "y allow once · a always · n/esc reject",
            _ => "y allow once · n/esc reject",
        },
    };

    let mut spans = vec![
        Span::styled(" ● ", THEME.mode(app.mode)),
        Span::styled(mode_label, THEME.dim),
    ];
    if let Some(status) = &app.status {
        spans.push(Span::styled("  │ ", THEME.faint));
        spans.push(Span::styled(status.clone(), THEME.warning));
    }
    // Right-align the hints: pad the gap after the left-side spans;
    // a saturated pad still needs one space or label and hints collide.
    let left: Line = Line::from(spans.clone());
    let hint = Span::styled(hints, THEME.faint);
    let pad = (area.width as usize)
        .saturating_sub(left.width() + hint.width() + 1)
        .max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(hint);
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Bottom input box; title + border track the mode (focused while
/// `Editing`), and the buffer tail-scrolls so the cursor stays visible.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.mode == InputMode::Editing;
    let agent = app.selected_session().map(|v| v.agent_name.as_str());
    let title = match agent {
        Some(name) => format!(" prompt → {name} "),
        None => " prompt ".to_string(),
    };
    let block = pane(Line::from(Span::styled(title, THEME.title))).border_style(if focused {
        THEME.border_focus
    } else {
        THEME.border
    });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Horizontal tail-scroll: show the last `inner.width` chars.
    let width = inner.width as usize;
    let shown: String = if app.input.chars().count() > width && width > 0 {
        app.input
            .chars()
            .skip(app.input.chars().count() - width)
            .collect()
    } else {
        app.input.clone()
    };
    if shown.is_empty() {
        let hint = if focused {
            "type a prompt…"
        } else {
            "i — write a prompt"
        };
        frame.render_widget(Paragraph::new(Span::styled(hint, THEME.faint)), inner);
    } else {
        frame.render_widget(
            Paragraph::new(Span::styled(shown.as_str(), THEME.text)),
            inner,
        );
    }
    if focused {
        frame.set_cursor_position(Position::new(
            inner.x + shown.chars().count() as u16,
            inner.y,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PermissionNotice;
    use agentmux_core::{
        AgentId, AgentProfile, Project, ProjectId, Session, SessionId, SessionState, Workspace,
        WorkspaceId,
    };
    use chrono::Utc;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;

    fn project(name: &str) -> Project {
        Project {
            id: ProjectId::new(),
            root_path: format!("/repos/{name}").into(),
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

    fn workspace(name: &str) -> Workspace {
        workspace_in(ProjectId::new(), name)
    }

    fn agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: AgentId::new(id),
            name: id.to_string(),
            adapter: agentmux_core::AdapterKind::Acp {
                command: format!("/bin/{id}").into(),
                args: vec![],
            },
            env: Default::default(),
            available: true,
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

    fn buffer_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Foreground color of the cell where `needle` first appears —
    /// lets render tests assert on *style*, not just text.
    fn fg_at(backend: &TestBackend, needle: &str) -> Option<Color> {
        let buf = backend.buffer();
        let w = buf.area.width as usize;
        for row in buf.content.chunks(w) {
            let line: String = row.iter().map(|c| c.symbol()).collect();
            if let Some(pos) = line.find(needle) {
                let cell_off = line[..pos].chars().count();
                return Some(row[cell_off].fg);
            }
        }
        None
    }

    /// Ratatui `TestBackend` buffer assertion: rendered UI contains the
    /// workspace groups, badge glyphs and the event text.
    #[test]
    fn draw_renders_groups_badges_and_events() {
        let alpha = workspace("alpha");
        let beta = workspace("beta");
        let ready = session(alpha.id, SessionState::Ready);
        let prompting = session(beta.id, SessionState::Prompting);
        let ready_id = ready.id;
        let views = vec![
            SessionView {
                session: ready,
                agent_name: "claude".into(),
                workspace_name: "alpha".into(),
            },
            SessionView {
                session: prompting,
                agent_name: "codex".into(),
                workspace_name: "beta".into(),
            },
        ];
        let mut app = App::new(vec![], vec![alpha, beta], views, vec![]);
        app.handle_event(Event {
            session_id: ready_id,
            seq: 1,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("hello world".into()),
        });

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let text = buffer_text(terminal.backend());
        assert!(text.contains("alpha"), "workspace group header: {text}");
        assert!(text.contains("beta"));
        assert!(text.contains("●"), "ready badge glyph: {text}");
        assert!(text.contains("◐"), "prompting badge glyph: {text}");
        assert!(text.contains("ready"));
        assert!(text.contains("prompting"));
        assert!(text.contains("claude"));
        assert!(text.contains("hello world"), "event text: {text}");
        assert!(text.contains("prompt"), "input box: {text}");
        assert!(text.contains("normal"), "status bar: {text}");
    }

    #[test]
    fn draw_empty_app_shows_hint() {
        let app = App::new(vec![], vec![], vec![], vec![]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("no sessions"), "{text}");
        assert!(text.contains("new session"), "empty hint keys: {text}");
        assert!(text.contains("quit"), "empty hint keys: {text}");
        assert!(text.contains("events"));
        assert!(
            text.contains("no session selected"),
            "event pane hint: {text}"
        );
    }

    /// A selected session with an empty log gets a waiting hint, not a
    /// blank pane.
    #[test]
    fn draw_empty_events_shows_waiting() {
        let ws = workspace("w");
        let s = session(ws.id, SessionState::Ready);
        let app = App::new(
            vec![],
            vec![ws],
            vec![SessionView {
                session: s,
                agent_name: "claude".into(),
                workspace_name: "w".into(),
            }],
            vec![],
        );
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("waiting for output"), "{text}");
    }

    /// A nil-session-id Orchestrator notice (the client's lag warning)
    /// renders in the event pane even though it belongs to no session.
    #[test]
    fn draw_shows_nil_id_global_notice() {
        let ws = workspace("w");
        let s = session(ws.id, SessionState::Ready);
        let app_views = vec![SessionView {
            session: s,
            agent_name: "claude".into(),
            workspace_name: "w".into(),
        }];
        let mut app = App::new(vec![], vec![ws], app_views, vec![]);
        app.handle_event(Event {
            session_id: SessionId(uuid::Uuid::nil()),
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("client event stream lagged".into()),
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(
            buffer_text(terminal.backend()).contains("client event stream lagged"),
            "global notice must render in the event pane"
        );
    }

    #[test]
    fn session_update_renders_message_text() {
        let v = serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi there"}
        });
        assert_eq!(session_update_text(&v), "hi there");
        let wrapped = serde_json::json!({
            "update": {"sessionUpdate": "tool_call", "title": "Read"}
        });
        assert!(session_update_text(&wrapped).contains("[tool_call]"));
    }

    // --- structured event rendering --------------------------------------

    /// One session of workspace `w`, its events preloaded.
    fn app_with_events(kinds: Vec<EventKind>) -> App {
        let ws = workspace("w");
        let s = session(ws.id, SessionState::Ready);
        let sid = s.id;
        let views = vec![SessionView {
            session: s,
            agent_name: "claude".into(),
            workspace_name: "w".into(),
        }];
        let mut app = App::new(vec![], vec![ws], views, vec![]);
        for (seq, kind) in kinds.into_iter().enumerate() {
            app.handle_event(Event {
                session_id: sid,
                seq: seq as u64 + 1,
                ts: Utc::now(),
                kind,
            });
        }
        app
    }

    /// Message chunks render as prose under an agent header — and a run
    /// of consecutive chunks coalesces into ONE header (streamed chunks
    /// are fragments, not messages).
    #[test]
    fn message_chunks_render_prose_under_one_header() {
        let app = app_with_events(vec![
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hello "}
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "world"}
            })),
        ]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("hello world"), "merged prose: {text}");
        // The agent name + double-space + timestamp header appears once:
        // the two chunks coalesced under a single header.
        assert_eq!(
            text.matches("claude  ").count(),
            1,
            "one header for the merged chunk run: {text}"
        );
    }

    /// A `tool_call` update becomes a card: `⚙` header with title, and
    /// the status word carries the status color (completed → success).
    #[test]
    fn tool_call_renders_status_colored_card() {
        let app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1",
            "title": "mock edit of src/lib.rs",
            "kind": "edit",
            "status": "completed",
            "locations": [{"path": "src/lib.rs"}]
        }))]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("⚙"), "tool card glyph: {text}");
        assert!(text.contains("mock edit of src/lib.rs"), "{text}");
        assert!(text.contains("completed"), "{text}");
        assert!(text.contains("src/lib.rs"), "location line: {text}");
        assert_eq!(
            fg_at(terminal.backend(), "completed"),
            THEME.success.fg,
            "completed status in the success hue"
        );
    }

    /// A `diff` content item paints `-` old lines in the error hue and
    /// `+` new lines in the success hue, under a highlighted path.
    #[test]
    fn diff_payload_colors_plus_minus_lines() {
        let app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-2",
            "title": "Edit src/lib.rs",
            "status": "completed",
            "content": [{
                "type": "diff",
                "path": "src/lib.rs",
                "oldText": "let x = 1;",
                "newText": "let x = 2;"
            }]
        }))]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("-let x = 1;"), "{text}");
        assert!(text.contains("+let x = 2;"), "{text}");
        assert_eq!(fg_at(terminal.backend(), "+let"), THEME.success.fg);
        assert_eq!(fg_at(terminal.backend(), "-let"), THEME.error.fg);
    }

    /// Unified-diff-looking *text* also gets +/- coloring — agents paste
    /// patches into messages all the time.
    #[test]
    fn pasted_patch_text_colors_diff_lines() {
        let app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new"}
        }))]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert_eq!(fg_at(terminal.backend(), "+new"), THEME.success.fg);
        assert_eq!(fg_at(terminal.backend(), "-old"), THEME.error.fg);
    }

    /// State transitions read as small dim `from → to` lines.
    #[test]
    fn state_change_renders_transition_line() {
        let app = app_with_events(vec![EventKind::StateChanged {
            from: SessionState::Ready,
            to: SessionState::Prompting,
        }]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("ready → prompting"), "{text}");
    }

    /// A `PermissionRequest` event becomes a warning banner in the
    /// stream — not raw JSON.
    #[test]
    fn permission_event_renders_warning_banner() {
        let app = app_with_events(vec![EventKind::PermissionRequest {
            request_id: "req-1".into(),
            request: serde_json::json!({
                "toolCall": {"title": "Write src/x.rs"},
                "options": [{"name": "reject"}],
            }),
        }]);
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("permission requested"), "{text}");
        assert!(!text.contains("toolCall"), "no raw JSON: {text}");
        assert_eq!(
            fg_at(terminal.backend(), "permission requested"),
            THEME.warning.fg,
        );
    }

    /// Activity on a non-selected session marks it `•` until viewed.
    #[test]
    fn unread_marker_on_inactive_sessions() {
        let ws = workspace("w");
        let a = session(ws.id, SessionState::Ready);
        let b = session(ws.id, SessionState::Ready);
        let b_id = b.id;
        let mut app = App::new(
            vec![],
            vec![ws],
            vec![
                SessionView {
                    session: a,
                    agent_name: "claude".into(),
                    workspace_name: "w".into(),
                },
                SessionView {
                    session: b,
                    agent_name: "codex".into(),
                    workspace_name: "w".into(),
                },
            ],
            vec![],
        );
        app.handle_event(Event {
            session_id: b_id,
            seq: 1,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("ping".into()),
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("•"), "unread marker: {text}");

        // Selecting it clears the marker.
        app.select_next();
        assert!(!app.unread.contains(&b_id));
    }

    // --- Task 14 panes/overlays ----------------------------------------------

    #[test]
    fn wizard_overlay_lists_projects() {
        let proj = project("smoke");
        let ws = workspace_in(proj.id, "ws1");
        let mut app = App::new(vec![proj], vec![ws], vec![], vec![agent("mock")]);
        app.start_wizard();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("new session — pick project"), "{text}");
        assert!(text.contains("smoke"), "{text}");
    }

    /// The permission dialog is interactive: it names the tool call and
    /// shows the answer keys. While the `session/permission` call is in
    /// flight it swaps the hints for "answering…" and stays up.
    #[test]
    fn permission_overlay_renders_interactive_dialog() {
        let mut app = app_with_events(vec![]);
        app.permission = Some(PermissionNotice {
            session_id: SessionId(uuid::Uuid::nil()),
            request_id: "req-1".into(),
            summary: "agent asks: Write src/x.rs (options: allow, reject)".into(),
            request: serde_json::json!({
                "toolCall": {"title": "Write src/x.rs"},
                "options": [
                    {"name": "allow", "kind": "allow_once"},
                    {"name": "always", "kind": "allow_always"},
                    {"name": "reject", "kind": "reject_once"},
                ],
            }),
            resume: InputMode::Normal,
            pending: false,
        });
        app.mode = InputMode::Permission;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("permission requested"), "{text}");
        assert!(text.contains("Write src/x.rs"), "{text}");
        assert!(text.contains("allow once"), "{text}");
        assert!(text.contains("always"), "{text}");
        assert!(text.contains("reject"), "{text}");

        // In-flight answer: hints swap to "answering…".
        app.permission.as_mut().unwrap().pending = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("answering"), "{text}");
        assert!(!text.contains("allow once"), "{text}");
    }

    #[test]
    fn relay_event_stage_lists_events() {
        let mut app = app_with_events(vec![
            EventKind::Orchestrator("first".into()),
            EventKind::FileEdited {
                path: "src/lib.rs".into(),
            },
        ]);
        app.start_relay();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("pick event"), "{text}");
        assert!(text.contains("edited src/lib.rs"), "{text}");
    }

    #[test]
    fn diff_panel_lists_touched_paths() {
        let mut app = app_with_events(vec![EventKind::FileEdited {
            path: "src/lib.rs".into(),
        }]);
        app.show_diff = true;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("files"), "{text}");
        assert!(text.contains("src/lib.rs"), "{text}");
    }

    // --- wrap-aware scroll (the reviewer-flagged under-scroll fix) -----------

    #[test]
    fn wrapped_rows_estimates_word_wrap() {
        // Fits: no wrap.
        assert_eq!(wrapped_rows(&Line::from("hello world"), 20), 1);
        // 100 chars of one word at width 20 → 5 rows.
        assert_eq!(wrapped_rows(&Line::from("a".repeat(100)), 20), 5);
        // Word-wrap waste: two 12-char words at width 20 can't share a
        // row (12+1+12 > 20) → 2 rows, where naive ceil(25/20)=2 too.
        assert_eq!(
            wrapped_rows(&Line::from("aaaaaaaaaaaa bbbbbbbbbbbb"), 20),
            2
        );
        // Four 12-char words at width 20: ceil(51/20)=3, but word
        // boundaries force one word per row → 4.
        assert_eq!(
            wrapped_rows(
                &Line::from("aaaaaaaaaaaa bbbbbbbbbbbb cccccccccccc dddddddddddd"),
                20
            ),
            4
        );
        // Empty line still occupies a row.
        assert_eq!(wrapped_rows(&Line::from(""), 20), 1);
    }

    /// Regression: with `Wrap` on, `.scroll` counts wrapped rows — so a
    /// stream of wrapping lines must still bottom-anchor on the newest
    /// event, not leave it hidden below the fold.
    #[test]
    fn events_pane_bottom_anchors_under_wrap() {
        let mut app = app_with_events(vec![
            EventKind::Orchestrator(format!("old {}", "x".repeat(300))),
            EventKind::Orchestrator("NEWEST-TAIL".into()),
        ]);
        app.mode = InputMode::Normal;
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(
            text.contains("NEWEST-TAIL"),
            "newest event must be visible: {text}"
        );
    }

    /// Perf bound: a 10k-event log must not make drawing expensive — the
    /// reverse scan stops once the viewport is covered. Generous time
    /// ceiling; the point is catching a regression to O(log) work.
    #[test]
    fn events_pane_scan_stays_viewport_bounded() {
        let ws = workspace("w");
        let s = session(ws.id, SessionState::Ready);
        let sid = s.id;
        let mut app = App::new(
            vec![],
            vec![ws],
            vec![SessionView {
                session: s,
                agent_name: "claude".into(),
                workspace_name: "w".into(),
            }],
            vec![],
        );
        for i in 0..10_000 {
            app.handle_event(Event {
                session_id: sid,
                seq: i,
                ts: Utc::now(),
                kind: EventKind::SessionUpdate(serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": format!("chunk {i} lorem ipsum dolor sit amet")}
                })),
            });
        }
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..10 {
            terminal.draw(|f| draw(f, &app)).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "10 draws over a 10k-event log took {elapsed:?} — viewport bound regressed"
        );
        let text = buffer_text(terminal.backend());
        assert!(text.contains("chunk 9999"), "newest chunk renders: {text}");
    }

    /// Regression: a stream of same-role message chunks merges under one
    /// header, so the estimate-based scan comes up short — the top-up
    /// pass must keep pulling events until the pane actually fills
    /// (pre-fix the bottom rows stayed blank mid-stream).
    #[test]
    fn chunk_stream_fills_the_viewport() {
        let kinds: Vec<EventKind> = (0..60)
            .map(|i| {
                EventKind::SessionUpdate(serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text",
                        "text": format!("lorem ipsum dolor sit amet {i} consectetur adipiscing elit sed do ")}
                }))
            })
            .collect();
        let app = app_with_events(kinds);
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let h = buf.area.height as usize;
        // Events-pane geometry, derived not hardcoded: the pane spans
        // x = LIST_WIDTH..w, so its inner columns are LIST_WIDTH+1..w-1;
        // the body strip is rows 0..h-4 (status+input take the last 4),
        // so the pane's inner rows are 1..=h-6 and h-6 is the last.
        let last_inner_y = h - 4 - 2;
        let x0 = LIST_WIDTH as usize + 1;
        let last_inner: String = buf.content[last_inner_y * w + x0..last_inner_y * w + (w - 1)]
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            !last_inner.trim().is_empty(),
            "merged chunk stream must fill to the pane bottom: {last_inner:?}"
        );
    }

    /// Scratch: eyeball a representative frame.
    #[test]
    #[ignore]
    fn print_frame() {
        let ws = workspace("alpha");
        let ws2 = workspace("beta");
        let s1 = session(ws.id, SessionState::Ready);
        let s2 = session(ws.id, SessionState::Prompting);
        let s3 = session(ws2.id, SessionState::Done);
        let sid = s1.id;
        let sid2 = s2.id;
        let sid3 = s3.id;
        let mut app = App::new(
            vec![],
            vec![ws, ws2],
            vec![
                SessionView {
                    session: s1,
                    agent_name: "claude".into(),
                    workspace_name: "alpha".into(),
                },
                SessionView {
                    session: s2,
                    agent_name: "codex".into(),
                    workspace_name: "alpha".into(),
                },
                SessionView {
                    session: s3,
                    agent_name: "pi".into(),
                    workspace_name: "beta".into(),
                },
            ],
            vec![],
        );
        let kinds = vec![
            EventKind::StateChanged {
                from: SessionState::Ready,
                to: SessionState::Prompting,
            },
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "I'll refactor the renderer to use "}
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "a bounded reverse scan."}
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1", "title": "Edit tui/src/ui.rs",
                "kind": "edit", "status": "in_progress",
                "locations": [{"path": "tui/src/ui.rs", "line": 42}],
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-2", "title": "Write theme.rs",
                "status": "completed",
                "content": [{"type": "diff", "path": "tui/src/theme.rs",
                    "oldText": "const FG: u8 = 1;", "newText": "const FG: u8 = 2;"}]
            })),
            EventKind::PermissionRequest {
                request_id: "req-1".into(),
                request: serde_json::json!({
                    "toolCall": {"title": "Write src/x.rs"},
                    "options": [{"name": "reject"}],
                }),
            },
            EventKind::Orchestrator("event stream lagged, skipped 3".into()),
        ];
        for (seq, kind) in kinds.into_iter().enumerate() {
            app.handle_event(Event {
                session_id: sid,
                seq: seq as u64 + 1,
                ts: Utc::now(),
                kind,
            });
        }
        app.permission = None;
        app.mode = InputMode::Normal;
        // Activity on other sessions → unread markers.
        app.handle_event(Event {
            session_id: sid2,
            seq: 9,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("bg".into()),
        });
        app.handle_event(Event {
            session_id: sid3,
            seq: 9,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("bg2".into()),
        });

        let backend = TestBackend::new(90, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        for row in buf.content.chunks(w) {
            let line: String = row.iter().map(|c| c.symbol()).collect();
            println!("{line}");
        }
    }

    /// Scratch: eyeball the wizard overlay.
    #[test]
    #[ignore]
    fn print_wizard() {
        let proj = project("smoke");
        let ws = workspace_in(proj.id, "ws1");
        let mut app = App::new(vec![proj], vec![ws], vec![], vec![agent("mock")]);
        app.start_wizard();
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        for row in buf.content.chunks(w) {
            let line: String = row.iter().map(|c| c.symbol()).collect();
            println!("{line}");
        }
    }
}
