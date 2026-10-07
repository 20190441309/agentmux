//! Conversation and modal renderers for the responsive workbench shell.
//! Colors are semantic roles from `theme`; layout is owned by `shell`.

use agentmux_core::{pi_shape, Event, EventKind};
use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;

use crate::app::{permission_summary, short_id, App, InputMode, RelayStage, SessionView};
use crate::interaction::{buttons, hit, Target};
use crate::newsession::{WizardStep, WorkspacePick};
use crate::theme::THEME;
use ratatui::style::Style;

/// Indent of message bodies and tool-call detail lines.
const BODY_INDENT: &str = "  ";

/// Upper bound on the lines a single event block may emit — a giant
/// diff or paste can't flood the viewport-bound scan.
const MAX_BLOCK_LINES: usize = 48;

/// Render the whole UI.
pub fn draw(frame: &mut Frame, app: &App) {
    crate::shell::draw(frame, app);
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
pub(crate) fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
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
pub(crate) fn draw_sessions(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut targets: Vec<(u16, Option<Target>)> = Vec::new();
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
                items.push(workspace_header(
                    &ws.name,
                    app.wb.collapsed_spaces.contains(&ws.id),
                ));
                if app.wb.collapsed_spaces.contains(&ws.id)
                    && app
                        .sessions
                        .get(highlight)
                        .is_some_and(|view| view.session.workspace_id == ws.id)
                {
                    selected_row = Some(items.len() - 1);
                }
                targets.push((1, Some(Target::WorkspaceGroup(ws.id))));
                header = true;
            }
            covered[i] = true;
            if app.wb.collapsed_spaces.contains(&ws.id) {
                continue;
            }
            if i == highlight {
                selected_row = Some(items.len());
            }
            targets.push((2, Some(Target::Session(app.sessions[i].session.id))));
            items.push(session_line(
                view,
                unread(i),
                app.session_title(view.session.id),
                app.agent_instance(view.session.id),
            ));
        }
        // Workspaces with no sessions still render — `n` can land there.
        if !header {
            items.push(workspace_header(
                &ws.name,
                app.wb.collapsed_spaces.contains(&ws.id),
            ));
            targets.push((1, Some(Target::WorkspaceGroup(ws.id))));
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
        let workspace = app.sessions[group[0]].session.workspace_id;
        let collapsed = app.wb.collapsed_spaces.contains(&workspace);
        items.push(workspace_header(name, collapsed));
        if collapsed
            && app
                .sessions
                .get(highlight)
                .is_some_and(|view| view.session.workspace_id == workspace)
        {
            selected_row = Some(items.len() - 1);
        }
        targets.push((1, Some(Target::WorkspaceGroup(workspace))));
        if collapsed {
            continue;
        }
        for i in group {
            if i == highlight {
                selected_row = Some(items.len());
            }
            targets.push((2, Some(Target::Session(app.sessions[i].session.id))));
            items.push(session_line(
                &app.sessions[i],
                unread(i),
                app.session_title(app.sessions[i].session.id),
                app.agent_instance(app.sessions[i].session.id),
            ));
        }
    }

    if items.is_empty() {
        for l in [
            Line::from(Span::styled("  no sessions yet", THEME.dim)),
            Line::default(),
            Line::from(vec![
                Span::styled("  Menu → New space", THEME.accent),
                Span::styled("  new session", THEME.faint),
            ]),
            Line::from(vec![
                Span::styled("  Menu → Exit", THEME.accent),
                Span::styled("  quit", THEME.faint),
            ]),
        ] {
            items.push(ListItem::new(l));
        }
    }

    let block = match (app.mode, app.relay.as_ref()) {
        (InputMode::RelayPick, Some(pick)) if pick.stage == RelayStage::Session => Block::default()
            .title(Line::from(Span::styled(
                " relay — pick target ",
                THEME.special,
            ))),
        _ => Block::default(),
    };
    let inner = block.inner(area);
    let list = List::new(items)
        .block(block)
        .highlight_style(THEME.selection)
        .highlight_symbol("▎");
    let mut state = ListState::default();
    state.select(selected_row);
    frame.render_stateful_widget(list, area, &mut state);
    let mut y = inner.y;
    for (height, target) in targets.into_iter().skip(state.offset()) {
        if y + height > inner.bottom() {
            break;
        }
        if let Some(target) = target {
            hit(app, Rect::new(inner.x, y, inner.width, height), target);
        }
        y += height;
    }
}

/// `▸ workspace` — a quiet section label, not a competing accent.
fn workspace_header(name: &str, collapsed: bool) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(if collapsed { " ▸ " } else { " ▾ " }, THEME.faint),
        Span::styled(name.to_string(), THEME.section),
    ]))
}

/// `● agent·id state [•]` — badge color is the state, name is text,
/// the state label stays secondary.
fn session_line(
    view: &SessionView,
    unread: bool,
    title: String,
    instance: String,
) -> ListItem<'static> {
    let (glyph, label) = App::badge(&view.session.state);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(glyph.to_string(), THEME.badge(&view.session.state)),
        Span::styled(format!(" {instance}"), THEME.text),
        Span::raw(" · "),
        Span::styled(label, THEME.dim),
    ];
    if unread {
        spans.push(Span::styled("  •", THEME.accent));
    }
    ListItem::new(vec![
        Line::styled(format!("  {title}"), THEME.text),
        Line::from(spans),
    ])
}

// --- event stream (right pane) ----------------------------------------------

/// A layout window in physical terminal rows. Scroll offsets never enter the
/// cache key: moving the viewport must not parse Markdown or wrap history.
#[derive(PartialEq, Eq)]
struct EventLayoutKey {
    session: Option<agentmux_core::SessionId>,
    width: usize,
    boundary: u64,
    count: usize,
    first: Option<u64>,
    last: Option<u64>,
    tools: bool,
    thoughts: bool,
    toggles: Vec<u64>,
    tool_toggles: Vec<String>,
}

pub(crate) struct EventLayout {
    key: EventLayoutKey,
    lines: Vec<Line<'static>>,
    headers: Vec<(usize, Target)>,
    complete: bool,
    budget: usize,
}

fn physical_rows(
    lines: Vec<Line<'static>>,
    headers: Vec<(usize, Target)>,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, Target)>) {
    let mut out = Vec::new();
    let mut positions = Vec::new();
    let mut headers = headers.into_iter().peekable();
    for (i, line) in lines.into_iter().enumerate() {
        if headers.peek().is_some_and(|(index, _)| *index == i) {
            positions.push((out.len(), headers.next().unwrap().1));
        }
        out.extend(crate::markdown::reflow(vec![line], width));
    }
    (out, positions)
}

fn build_event_layout(
    app: &App,
    events: &[&Event],
    key: EventLayoutKey,
    target_rows: usize,
) -> EventLayout {
    let agent = app
        .selected_session()
        .map(|v| v.agent_name.as_str())
        .unwrap_or("agentmux");
    let inner_width = key.width;
    let mut iter = events.iter().copied().rev();
    let mut blocks: Vec<EventBlock> = Vec::new(); // newest first
    let mut exhausted = false;
    let mut est = 0usize;
    let max_blocks = target_rows.saturating_mul(inner_width).max(64);
    while est < target_rows && blocks.len() < max_blocks {
        match iter.next() {
            Some(ev) => {
                let block = display_block(ev, app, inner_width);
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
    let mut reasoning_headers = Vec::new();
    let lines = render_blocks(
        blocks.iter().rev(),
        agent,
        app,
        &mut reasoning_headers,
        inner_width,
        target_rows + 1,
    );
    let (mut lines, mut reasoning_headers) = physical_rows(lines, reasoning_headers, inner_width);
    let mut rows = lines.len();
    while rows < target_rows && !exhausted && blocks.len() < max_blocks {
        for _ in 0..target_rows - rows {
            match iter.next() {
                Some(ev) => blocks.push(display_block(ev, app, inner_width)),
                None => {
                    exhausted = true;
                    break;
                }
            }
            if blocks.len() >= max_blocks {
                break;
            }
        }
        lines = render_blocks(
            blocks.iter().rev(),
            agent,
            app,
            &mut reasoning_headers,
            inner_width,
            target_rows + 1,
        );
        (lines, reasoning_headers) = physical_rows(lines, reasoning_headers, inner_width);
        rows = lines.len();
    }

    // The event iterator can be exhausted while a single long reply still has
    // cached rows above the rendered tail. Do not cap scrolling at that tail.
    let clipped_reply = blocks.iter().any(|block| {
        if let EventBlock::Reply { session_id, id, .. } = block {
            app.wb
                .reasoning
                .get(session_id)
                .and_then(|r| r.replies.get(id))
                .is_some_and(|r| {
                    r.cache
                        .borrow()
                        .as_ref()
                        .is_some_and(|c| c.lines.len() > target_rows + 1)
                })
        } else {
            false
        }
    });
    EventLayout {
        key,
        lines,
        headers: reasoning_headers,
        complete: exhausted && !clipped_reply,
        budget: target_rows,
    }
}

/// Draw only the visible rows. Reuse a prefetched layout window when scrolling;
/// rebuild after content/style/width changes or when visiting older history.
pub(crate) fn draw_events(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(4).max(1) as usize;
    let height = area.height.saturating_sub(1).max(1) as usize;
    let session = app.selected_session_id();
    let session_ui = session.and_then(|id| app.wb.sessions.get(&id));
    let anchor = session_ui.and_then(|s| s.anchor);
    let offset = session_ui.map(|s| s.scroll).unwrap_or(0);
    let target = height.saturating_add(offset);
    let events: Vec<_> = app
        .events_for_selected()
        .filter(|e| anchor.is_none_or(|seq| e.seq <= seq))
        .collect();
    let mut toggles: Vec<_> = app
        .wb
        .reasoning_toggles
        .iter()
        .filter(|(id, _)| Some(*id) == session)
        .map(|(_, id)| *id)
        .collect();
    toggles.sort_unstable();
    let key = EventLayoutKey {
        session,
        width,
        boundary: session_ui.map(|s| s.conversation_start).unwrap_or(0),
        count: events.len(),
        first: events.first().map(|e| e.seq),
        last: events.last().map(|e| e.seq),
        tools: app.wb.tools_expanded,
        thoughts: app.wb.thoughts,
        toggles,
        tool_toggles: {
            let mut toggles: Vec<_> = app
                .wb
                .transcript
                .tool_toggles
                .iter()
                .filter(|(id, _)| Some(*id) == session)
                .map(|(_, tool)| tool.clone())
                .collect();
            toggles.sort();
            toggles
        },
    };
    let mut cache = app.wb.event_layout.borrow_mut();
    if cache
        .as_ref()
        .is_none_or(|c| c.key != key || (!c.complete && c.budget < target))
    {
        // Amortize history traversal over many wheel events. Live content is
        // bounded to a small window; scrolling extends that window in batches.
        let budget = target.div_ceil(256).saturating_mul(256);
        *cache = Some(build_event_layout(app, &events, key, budget));
        #[cfg(test)]
        app.wb.layout_builds.set(app.wb.layout_builds.get() + 1);
    }
    let layout = cache.as_ref().unwrap();
    if let Some(ui) = session_ui {
        ui.scroll_limit.set(
            layout
                .complete
                .then(|| layout.lines.len().saturating_sub(height)),
        );
    }
    let start = layout
        .lines
        .len()
        .saturating_sub(height)
        .saturating_sub(offset);
    let mut visible: Vec<_> = layout
        .lines
        .iter()
        .skip(start)
        .take(height)
        .cloned()
        .collect();
    if layout.lines.is_empty() {
        visible = events_empty_lines(app);
    }
    for (row, target) in &layout.headers {
        if *row >= start && *row - start < height {
            // Only the timer label changes during an active thought. Its
            // physical row is stable, so the body does not need re-layout.
            if let Target::Reasoning(id, block) = target {
                if let Some(thought) = app.wb.reasoning.get(id).and_then(|r| r.blocks.get(block)) {
                    let open = app.wb.thoughts ^ app.wb.reasoning_toggles.contains(&(*id, *block));
                    visible[*row - start] = thought_header(thought, open);
                }
            }
            crate::interaction::hit(
                app,
                Rect::new(
                    area.x + 2,
                    area.y + (*row - start) as u16,
                    area.width.saturating_sub(4),
                    1,
                ),
                target.clone(),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(
            visible
                .into_iter()
                .map(|line| {
                    crate::transcript::highlight(
                        line,
                        session
                            .map(|id| crate::transcript::query(app, id))
                            .unwrap_or(""),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .block(Block::default().padding(Padding::new(2, 2, 0, 1))),
        area,
    );
    if anchor.is_some() && area.height > 0 {
        let new_events = app
            .session_events()
            .filter(|e| anchor.is_some_and(|seq| e.seq > seq))
            .count();
        let label = format!("Back to latest · {new_events} new");
        buttons(
            frame,
            app,
            Rect::new(
                area.x + 1,
                area.bottom() - 1,
                area.width.saturating_sub(2),
                1,
            ),
            &[(&label, Target::Command("/latest"))],
        );
    }
}

fn thought_header(thought: &crate::reasoning::Thought, open: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {} ", if open { "▾" } else { "▸" }), THEME.dim),
        Span::styled(
            format!(
                "{} · {}s",
                if thought.ended.is_some() {
                    "Thought"
                } else {
                    "Thinking"
                },
                thought.duration()
            ),
            THEME.warning,
        ),
    ])
}

/// The event pane when it has nothing to show — a hint, not a void.
fn events_empty_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    if app.selected_session().is_none() {
        lines.extend([
            Line::from(Span::styled("  no session selected", THEME.dim)),
            Line::from(vec![
                Span::styled("  Menu → New space", THEME.accent),
                Span::styled("  new session", THEME.faint),
            ]),
        ]);
    } else if app.selected_session().is_some_and(
        |s| matches!(&s.session.state, agentmux_core::SessionState::Error(reason) if reason == "daemon restarted"),
    ) {
        lines.extend([
            Line::from(Span::styled("  New conversation", THEME.text)),
            Line::from(Span::styled("  Type a message and press Enter.", THEME.dim)),
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
    Reply {
        session_id: agentmux_core::SessionId,
        id: u64,
        end: usize,
    },
    Reasoning {
        session_id: agentmux_core::SessionId,
        id: u64,
    },
    /// Hidden lifecycle records still separate consecutive agent turns.
    Boundary,
    Msg {
        role: MsgRole,
        ts: DateTime<Utc>,
        text: String,
    },
    Static(Vec<Line<'static>>),
    Tool {
        id: String,
        title: Option<String>,
        status: Option<String>,
        details: Vec<Line<'static>>,
        session: agentmux_core::SessionId,
    },
}

/// The wrapped rows `block` will occupy when rendered alone — the phase-1
/// scan bound. `Msg` counts its header plus per-source-line body wrap,
/// an over-estimate of the merged contribution (merging drops headers);
/// the top-up pass in `draw_events` corrects the resulting shortfall.
fn block_rows(block: &EventBlock, width: usize) -> usize {
    match block {
        EventBlock::Reasoning { .. } | EventBlock::Reply { .. } => 1,
        EventBlock::Boundary => 0,
        EventBlock::Tool { title, details, .. } => {
            details
                .iter()
                .map(|line| wrapped_rows(line, width))
                .sum::<usize>()
                + wrapped_rows(
                    &Line::from(format!(
                        "  {}  completed",
                        title.as_deref().unwrap_or("Tool")
                    )),
                    width,
                )
        }
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
    app: &App,
    reasoning_headers: &mut Vec<(usize, crate::interaction::Target)>,
    width: usize,
    budget: usize,
) -> Vec<Line<'static>> {
    let blocks: Vec<_> = blocks.collect();
    let mut reply_ends = std::collections::HashMap::new();
    for block in &blocks {
        if let EventBlock::Reply {
            session_id,
            id,
            end,
        } = block
        {
            reply_ends
                .entry((*session_id, *id))
                .and_modify(|n: &mut usize| *n = (*n).max(*end))
                .or_insert(*end);
        }
    }
    let mut seen_replies = std::collections::HashSet::new();
    reasoning_headers.clear();
    let mut seen_reasoning = std::collections::HashSet::new();
    let mut out: Vec<Line> = Vec::new();
    let mut pending: Option<(MsgRole, DateTime<Utc>, String)> = None;
    let mut tools: std::collections::HashMap<&str, (usize, String, Option<String>)> =
        std::collections::HashMap::new();
    for block in blocks {
        match block {
            EventBlock::Reply { session_id, id, .. } => {
                if !seen_replies.insert((*session_id, *id)) {
                    continue;
                }
                flush_msg(&mut out, pending.take(), agent, width);
                let Some(reply) = app
                    .wb
                    .reasoning
                    .get(session_id)
                    .and_then(|r| r.replies.get(id))
                else {
                    continue;
                };
                let end = reply_ends[&(*session_id, *id)];
                let mut cached = reply.cache.borrow_mut();
                if cached
                    .as_ref()
                    .is_none_or(|c| c.width != width || c.end != end)
                {
                    *cached = Some(crate::reasoning::ReplyRender {
                        width,
                        end,
                        lines: crate::markdown::reflow(
                            full_prose(&reply.text[..end], THEME.text, width),
                            width,
                        ),
                    });
                }
                if !out.is_empty() {
                    out.push(Line::default());
                }
                let lines = &cached.as_ref().unwrap().lines;
                if lines.len() <= budget {
                    out.push(Line::styled(format!("  {agent}"), THEME.accent_bold));
                }
                out.extend(
                    lines
                        .iter()
                        .skip(lines.len().saturating_sub(budget))
                        .cloned(),
                );
                out.push(Line::default());
            }

            EventBlock::Reasoning { session_id, id } => {
                if !seen_reasoning.insert((*session_id, *id)) {
                    continue;
                }
                flush_msg(&mut out, pending.take(), agent, width);
                let Some(thought) = app
                    .wb
                    .reasoning
                    .get(session_id)
                    .and_then(|r| r.blocks.get(id))
                else {
                    continue;
                };
                let open = app.wb.thoughts ^ app.wb.reasoning_toggles.contains(&(*session_id, *id));
                if !out.is_empty() {
                    out.push(Line::default());
                }
                reasoning_headers.push((
                    out.len(),
                    crate::interaction::Target::Reasoning(*session_id, *id),
                ));
                out.push(thought_header(thought, open));
                if open {
                    let mut cache = thought.cache.borrow_mut();
                    if cache
                        .as_ref()
                        .is_none_or(|c| c.width != width || c.end != thought.text.len())
                    {
                        let mut lines =
                            full_prose(&thought.text, THEME.dim_italic, width.saturating_sub(4));
                        for line in &mut lines {
                            line.spans.insert(0, Span::styled("  │ ", THEME.faint));
                        }
                        *cache = Some(crate::reasoning::ReplyRender {
                            width,
                            end: thought.text.len(),
                            lines,
                        });
                    }
                    out.extend(cache.as_ref().unwrap().lines.iter().cloned());
                }
            }

            EventBlock::Boundary => {
                flush_msg(&mut out, pending.take(), agent, width);
                tools.clear();
            }
            EventBlock::Tool {
                id,
                title,
                status,
                details,
                session,
            } => {
                flush_msg(&mut out, pending.take(), agent, width);
                if tools.contains_key(id.as_str()) {
                    continue;
                }
                let entry = tools.entry(id).or_insert_with(|| {
                    let index = out.len();
                    out.push(Line::default());
                    (index, "Tool".into(), None)
                });
                if let Some(title) = title {
                    entry.1.clone_from(title);
                }
                if let Some(status) = status {
                    entry.2 = Some(status.clone());
                }
                out[entry.0] = Line::from(vec![
                    Span::styled("  ⚙ ", THEME.dim),
                    Span::styled(entry.1.clone(), THEME.dim),
                    Span::styled(
                        format!("  {}", entry.2.as_deref().unwrap_or("pending")),
                        THEME.tool_status(entry.2.as_deref()),
                    ),
                ]);
                reasoning_headers.push((entry.0, Target::Tool(*session, id.clone())));
                out.extend(details.iter().cloned());
            }
            EventBlock::Msg { role, ts, text } => match &mut pending {
                Some((r, _, buf)) if *r == *role => {
                    buf.push_str(text);
                }
                _ => {
                    flush_msg(&mut out, pending.take(), agent, width);
                    pending = Some((*role, *ts, text.clone()));
                }
            },
            EventBlock::Static(lines) if lines.is_empty() => {}
            EventBlock::Static(lines) => {
                flush_msg(&mut out, pending.take(), agent, width);
                out.extend(lines.iter().cloned());
            }
        }
    }
    flush_msg(&mut out, pending.take(), agent, width);
    out
}

/// Emit a coalesced message with a quiet role label and spaced prose.
fn flush_msg(
    out: &mut Vec<Line<'static>>,
    pending: Option<(MsgRole, DateTime<Utc>, String)>,
    agent: &str,
    width: usize,
) {
    let Some((role, _ts, text)) = pending else {
        return;
    };
    if text.trim().is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(Line::default());
    }
    if role == MsgRole::User {
        out.extend(crate::markdown::user_panel(&text, width));
        out.push(Line::default());
        return;
    }
    let header = match role {
        MsgRole::Agent => vec![Span::styled(format!("  {agent}"), THEME.accent_bold)],
        MsgRole::Thought => vec![Span::styled(
            format!("  {agent} · thinking"),
            THEME.dim_italic,
        )],
        MsgRole::User => vec![Span::styled(
            "  You",
            THEME.text.add_modifier(ratatui::style::Modifier::BOLD),
        )],
    };
    out.push(Line::from(header));
    let body_style = match role {
        MsgRole::Thought => THEME.dim_italic,
        _ => THEME.text,
    };
    out.extend(full_prose(&text, body_style, width));
    out.push(Line::default());
}

/// Markdown layout uses the current pane width, including during resize.
fn full_prose(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    if looks_like_diff(text) {
        return text
            .lines()
            .map(|line| {
                Line::styled(
                    format!("  {line}"),
                    if line.starts_with('+') {
                        THEME.success
                    } else if line.starts_with('-') {
                        THEME.error
                    } else {
                        style
                    },
                )
            })
            .collect();
    }
    crate::markdown::prose(text, style, width)
}

fn display_block(ev: &Event, app: &App, width: usize) -> EventBlock {
    if let Some((id, end)) = app
        .wb
        .reasoning
        .get(&ev.session_id)
        .and_then(|r| r.reply_events.get(&ev.seq))
    {
        return EventBlock::Reply {
            session_id: ev.session_id,
            id: *id,
            end: *end,
        };
    }

    if let Some(id) = app
        .wb
        .reasoning
        .get(&ev.session_id)
        .and_then(|r| r.events.get(&ev.seq))
    {
        return EventBlock::Reasoning {
            session_id: ev.session_id,
            id: *id,
        };
    }
    if !app.wb.tools_expanded {
        match &ev.kind {
            EventKind::StateChanged {
                to: agentmux_core::SessionState::Error(error),
                ..
            } => {
                let lines: Vec<_> = error
                    .lines()
                    .take(MAX_BLOCK_LINES + 1)
                    .enumerate()
                    .map(|(index, line)| {
                        Line::styled(
                            format!("{}{}", if index == 0 { "  ! " } else { BODY_INDENT }, line),
                            THEME.error,
                        )
                    })
                    .collect();
                return EventBlock::Static(cap_head(lines));
            }
            EventKind::StateChanged { .. } => return EventBlock::Boundary,
            EventKind::SessionUpdate(value) => {
                let update = value.get("update").unwrap_or(value);
                if matches!(
                    pi_shape::kind(update),
                    Some("agent_start" | "agent_end" | "agent_settled")
                ) {
                    return EventBlock::Boundary;
                }
            }
            _ => {}
        }
    }

    let block = event_block(ev);
    if matches!(
        &block,
        EventBlock::Msg {
            role: MsgRole::Thought,
            ..
        }
    ) && !app.wb.thoughts
    {
        return EventBlock::Static(Vec::new());
    }
    if let EventKind::SessionUpdate(v) = &ev.kind {
        let original = v.get("update").unwrap_or(v);
        let merged = original
            .get("toolCallId")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| crate::transcript::tool(app, ev.session_id, id));
        let update = merged.as_ref().map(|tool| &tool.value).unwrap_or(original);
        if matches!(
            update.get("sessionUpdate").and_then(|v| v.as_str()),
            Some("tool_call" | "tool_call_update")
        ) {
            let id = update.get("toolCallId").and_then(|value| value.as_str());
            let open = app.wb.tools_expanded
                ^ id.is_some_and(|id| {
                    app.wb
                        .transcript
                        .tool_toggles
                        .contains(&(ev.session_id, id.into()))
                });
            if let (Some(id), false) = (id, open) {
                return EventBlock::Tool {
                    id: id.into(),
                    title: update
                        .get("title")
                        .or_else(|| update.get("kind"))
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                    status: update
                        .get("status")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                    details: vec![],
                    session: ev.session_id,
                };
            }
            let mut lines = tool_call_lines(ev, update);
            lines.truncate(1);
            if !open {
                if let Some(line) = lines.first_mut() {
                    line.spans.push(Span::styled("  ···", THEME.faint));
                }
                return EventBlock::Static(lines);
            }
            if let Some(locations) = update.get("locations").and_then(|v| v.as_array()) {
                for loc in locations {
                    if let Some(path) = loc.get("path").and_then(|v| v.as_str()) {
                        lines.extend(full_prose(path, THEME.accent, width));
                    }
                }
            }
            if let Some(content) = update.get("content").and_then(|v| v.as_array()) {
                for item in content {
                    match item.get("type").and_then(|v| v.as_str()) {
                        Some("diff") => {
                            lines.extend(full_prose(
                                item.get("path").and_then(|v| v.as_str()).unwrap_or("diff"),
                                THEME.accent,
                                width,
                            ));
                            for (field, prefix, style) in [
                                ("oldText", "-", THEME.error),
                                ("newText", "+", THEME.success),
                            ] {
                                if let Some(text) = item.get(field).and_then(|v| v.as_str()) {
                                    lines
                                        .extend(text.lines().map(|l| {
                                            Line::styled(format!("  {prefix}{l}"), style)
                                        }));
                                }
                            }
                        }
                        Some("content") => {
                            if let Some(text) =
                                item.pointer("/content/text").and_then(|v| v.as_str())
                            {
                                lines.extend(full_prose(text, THEME.dim, width));
                            }
                        }
                        Some("terminal") => lines.push(Line::styled(
                            format!(
                                "  terminal {}",
                                item.get("terminalId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                            ),
                            THEME.dim,
                        )),
                        _ => {}
                    }
                }
            }
            for field in ["rawInput", "rawOutput"] {
                if let Some(value) = update.get(field) {
                    let text = value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default());
                    lines.extend(full_prose(&text, THEME.dim, width));
                }
            }
            if let Some(id) = id {
                return EventBlock::Tool {
                    id: id.into(),
                    title: update
                        .get("title")
                        .or_else(|| update.get("kind"))
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                    status: update
                        .get("status")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                    details: lines.into_iter().skip(1).collect(),
                    session: ev.session_id,
                };
            }
            return EventBlock::Static(lines);
        }
    }
    block
}

/// One event → its renderable block (timestamped; `Msg` defers text).
fn event_block(ev: &Event) -> EventBlock {
    match &ev.kind {
        EventKind::TitleChanged { .. } => EventBlock::Static(vec![]),
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
        EventKind::ConversationStarted => EventBlock::Static(vec![]),
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
/// Pi-native passthrough records (`{"type": "<kind>"}`, no
/// `sessionUpdate`) go to [`pi_event_block`].
fn session_update_block(ev: &Event, value: &serde_json::Value) -> EventBlock {
    let update = value.get("update").unwrap_or(value);
    if update.get("sessionUpdate").is_none() && pi_shape::kind(update).is_some() {
        return pi_event_block(ev, update);
    }
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

/// A pi-native `{"type": "<kind>", …}` record → its block. The pi
/// translator normalizes deltas and `tool_execution_*` into ACP shapes
/// upstream, so what lands here is passthrough: message plumbing and
/// turn scaffolding render as nothing at all (an empty `Static` — their
/// content already arrived via deltas), deltas render defensively as
/// `Msg` (persisted pre-translation logs, manual injection), and
/// lifecycle/extension records get one quiet `·`-prefixed line via
/// [`pi_shape::summary`].
fn pi_event_block(ev: &Event, update: &serde_json::Value) -> EventBlock {
    // Streaming deltas (defensive — normalized upstream into
    // agent_*_chunk; this covers records that arrived unnormalized).
    if let Some((role, text)) = pi_shape::delta(update) {
        let role = match role {
            pi_shape::Delta::Message => MsgRole::Agent,
            pi_shape::Delta::Thought => MsgRole::Thought,
        };
        return EventBlock::Msg {
            role,
            ts: ev.ts,
            text: text.to_string(),
        };
    }

    match pi_shape::kind(update) {
        // `error` assistant events deserve a visible line; the other
        // message_update sub-kinds (text_start/end, toolcall_*, done)
        // are provider plumbing.
        Some("message_update") => {
            let err = update
                .pointer("/assistantMessageEvent/error")
                .or_else(|| update.pointer("/assistantMessageEvent/message"))
                .and_then(|e| e.as_str());
            if update
                .pointer("/assistantMessageEvent/type")
                .and_then(|t| t.as_str())
                == Some("error")
            {
                return EventBlock::Static(vec![Line::from(vec![
                    ts_span(ev),
                    Span::styled("⚠ ", THEME.warning),
                    Span::styled(
                        format!("stream error: {}", err.unwrap_or("unknown")),
                        THEME.warning,
                    ),
                ])]);
            }
            EventBlock::Static(vec![])
        }
        // `message_end` restates the text the deltas already streamed —
        // showing it would double every reply.
        Some("message_end") | Some("message_start") | Some("turn_start") | Some("turn_end") => {
            EventBlock::Static(vec![])
        }
        // Direct RPC bash output streams as dim body text.
        Some("bash_execution_update") => match update.get("delta").and_then(|d| d.as_str()) {
            Some(delta) => EventBlock::Static(body_lines(delta, THEME.dim, true)),
            None => EventBlock::Static(vec![]),
        },
        _ => match pi_shape::summary(update) {
            Some(text) => EventBlock::Static(vec![Line::from(vec![
                ts_span(ev),
                Span::styled("· ", THEME.faint),
                Span::styled(text, THEME.faint),
            ])]),
            None => EventBlock::Static(vec![]),
        },
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
        EventBlock::Reasoning { .. } | EventBlock::Reply { .. } => Line::default(),
        EventBlock::Boundary => Line::default(),
        EventBlock::Tool { title, .. } => {
            Line::styled(title.unwrap_or_else(|| "Tool".into()), THEME.dim)
        }
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
        .highlight_symbol("▎");
    let mut state = ListState::default();
    state.select(Some(cursor));
    frame.render_stateful_widget(list, area, &mut state);
    let inner = pane("").inner(area);
    for (row, i) in (state.offset()..app.session_events().count())
        .take(inner.height as usize)
        .enumerate()
    {
        hit(
            app,
            Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            Target::Relay(i),
        );
    }
    buttons(
        frame,
        app,
        Rect::new(
            area.x + 1,
            area.bottom().saturating_sub(1),
            area.width.saturating_sub(2),
            1,
        ),
        &[("Cancel quote", Target::Close)],
    );
}

/// `Tab` diff/files panel: paths the selected session touched
/// (`FileEdited` events + `tool_call` locations), first-touch order.
pub(crate) fn draw_files(frame: &mut Frame, app: &App, area: Rect) {
    crate::files::draw(frame, app, area);
}

/// A centered modal rect — for the wizard and the permission notice.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
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
pub(crate) fn draw_wizard(frame: &mut Frame, app: &App) {
    let Some(wiz) = &app.wizard else {
        return;
    };
    let (title, rows, cursor): (&str, Vec<String>, Option<usize>) = match wiz.step {
        WizardStep::Project => (
            "Choose project",
            app.projects
                .iter()
                .map(|p| format!("{}  {}", p.name, p.root_path.display()))
                .chain(std::iter::once("+ Add project…".into()))
                .collect(),
            Some(wiz.project_cursor),
        ),
        WizardStep::ProjectPath => (
            "Add a Git project",
            vec![
                "Paste or type the absolute path to your Git repository:".into(),
                format!("{}▌", wiz.path),
                if wiz.registering {
                    "Adding project…".into()
                } else {
                    "Then click Next. The folder must already contain a Git repository.".into()
                },
            ],
            None,
        ),
        WizardStep::Workspace => (
            "Choose space",
            wiz.workspace_options(app)
                .map(|w| {
                    let members = app
                        .sessions
                        .iter()
                        .filter(|v| v.session.workspace_id == w.id)
                        .count();
                    format!("{} · {members} agents · {}", w.name, w.branch)
                })
                .chain(std::iter::once("+ Create new space…".into()))
                .chain(std::iter::once("Project directory".into()))
                .collect(),
            Some(wiz.workspace_cursor),
        ),
        WizardStep::WorkspaceName => (
            "Name the space",
            vec![
                format!("Name: {}▌", wiz.name),
                "A separate Git worktree for this task.".into(),
            ],
            None,
        ),
        WizardStep::Agent => (
            "Choose agent",
            wiz.agent_options(app)
                .map(|a| format!("{} ({})", a.name, a.id))
                .collect(),
            Some(wiz.agent_cursor),
        ),
    };
    let rect = centered(frame.area(), 72, (rows.len() as u16 + 10).clamp(13, 22));
    draw_backdrop(frame);
    frame.render_widget(Clear, rect);
    let operation = if wiz.adding { "Add agent" } else { "New space" };
    let block = pane(format!(" {operation} · {title} ")).border_style(THEME.border_focus);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::vertical([
        Constraint::Length(if wiz.step == WizardStep::Agent { 2 } else { 0 }),
        Constraint::Min(1),
        Constraint::Length(if wiz.error.is_some() { 3 } else { 1 }),
        Constraint::Length(2),
    ])
    .split(inner);
    let count = rows.len();
    if wiz.step == WizardStep::Agent {
        let target = match &wiz.workspace {
            Some(WorkspacePick::Existing { id, name }) => {
                let branch = app
                    .workspaces
                    .iter()
                    .find(|w| w.id == *id)
                    .map(|w| w.branch.as_str())
                    .unwrap_or("");
                format!("Space: {name}\nBranch: {branch}")
            }
            Some(WorkspacePick::Directory { name, .. }) => format!("Directory: {name}"),
            Some(WorkspacePick::New { name, .. }) => {
                format!("New space: {name}\nBranch: agentmux/{name}")
            }
            None => "No space selected".into(),
        };
        frame.render_widget(Paragraph::new(target).style(THEME.accent), parts[0]);
    }
    if wiz.submitting {
        frame.render_widget(Paragraph::new("Adding agent...").style(THEME.dim), parts[1]);
    } else if cursor.is_none() {
        frame.render_widget(
            Paragraph::new(rows.join("\n"))
                .wrap(Wrap { trim: false })
                .style(THEME.text),
            parts[1],
        );
    } else if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(format!("No available agents.\nConfig: $AGENTMUX_CONFIG or ~/.config/agentmux/config.toml\n{}", app.agents.iter().map(|a| format!("{} (unavailable)", a.name)).collect::<Vec<_>>().join("\n")))
                .wrap(Wrap { trim: false }).style(THEME.warning),
            parts[1],
        );
    } else {
        let items: Vec<_> = rows.into_iter().map(ListItem::new).collect();
        let mut state = ListState::default().with_selected(cursor);
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(THEME.selection)
                .highlight_symbol("› "),
            parts[1],
            &mut state,
        );
        for (row, i) in (state.offset()..count)
            .take(parts[1].height as usize)
            .enumerate()
        {
            hit(
                app,
                Rect::new(parts[1].x, parts[1].y + row as u16, parts[1].width, 1),
                Target::Wizard(i),
            );
        }
    }
    if let Some(status) = wiz.error.as_ref().or(app.status.as_ref()) {
        frame.render_widget(
            Paragraph::new(status.as_str())
                .style(THEME.warning)
                .wrap(Wrap { trim: false }),
            parts[2],
        );
    }
    if wiz.submitting {
        buttons(
            frame,
            app,
            parts[3],
            &[("Close (background)", Target::Command("/close-wizard"))],
        );
    } else if !wiz.registering {
        let mut controls = vec![];
        if wiz.step != WizardStep::Agent || count > 0 {
            controls.push((
                if wiz.step == WizardStep::Agent {
                    "Add agent"
                } else {
                    "Next"
                },
                Target::Key(crossterm::event::KeyCode::Enter),
            ));
        }
        if !wiz.adding {
            controls.push(("Back", Target::Close));
        }
        controls.push(("Cancel", Target::Command("/close-wizard")));
        buttons(frame, app, parts[3], &controls);
    }
}

/// The permission dialog: the agent's request is parked daemon-side —
/// `y`/`a`/`n`/`Esc` send `session/permission`. While the answer is in
/// flight (`pending`) the dialog stays up with an "answering…" row and
/// keys are ignored; the `PermissionResolved` event dismisses it.
pub(crate) fn draw_permission(frame: &mut Frame, app: &App) {
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
        lines.push(Line::styled("Answering…", THEME.dim));
    }
    if let Some(view) = app
        .sessions
        .iter()
        .find(|v| v.session.id == notice.session_id)
    {
        lines.insert(
            0,
            Line::styled(
                format!(
                    "{} / {}",
                    view.agent_name,
                    app.session_title(view.session.id)
                ),
                THEME.accent,
            ),
        );
    }
    if let Some(tool) = notice.request.get("toolCall") {
        let detail = tool
            .get("rawInput")
            .or_else(|| tool.get("content"))
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
            .unwrap_or_default();
        lines.extend(
            detail
                .lines()
                .map(|l| Line::styled(l.to_string(), THEME.text)),
        );
    }
    let width = 76.min(frame.area().width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let rows = paragraph.line_count(width.saturating_sub(2));
    let rect = centered(
        frame.area(),
        width,
        (rows.min(u16::MAX as usize) as u16)
            .saturating_add(5)
            .min(frame.area().height),
    );
    draw_backdrop(frame);
    crate::shell::clear_overlay(frame, rect);
    let block = pane(Line::from(Span::styled(
        format!(
            " permission requested — {} ",
            short_id(&notice.session_id.to_string())
        ),
        THEME.title,
    )))
    .border_style(THEME.warning);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).split(inner);
    let max_scroll = rows
        .saturating_sub(parts[0].height as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        paragraph.scroll((app.wb.permission_scroll.min(max_scroll), 0)),
        parts[0],
    );
    if !notice.pending {
        let mut choices = vec![
            (
                "Allow once",
                Target::Key(crossterm::event::KeyCode::Char('y')),
            ),
            ("Reject", Target::Key(crossterm::event::KeyCode::Char('n'))),
            ("Later", Target::Close),
        ];
        if notice.allows_always() {
            choices.push((
                "Always allow",
                Target::Key(crossterm::event::KeyCode::Char('a')),
            ));
        }
        buttons(frame, app, parts[1], &choices);
    }
}

/// Pull human-readable text out of an opaque `session/update` JSON
/// blob: known ACP shapes get their `text` payload, pi-native records
/// (`{"type": "<kind>"}`) get their delta/message/summary text, and
/// everything else falls back to a compact, truncated dump. (Kept for
/// the compact/relay path and tests; the rich renderer lives in
/// [`session_update_block`].)
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
    if tag.is_none() {
        // Pi-native record: dig by `type` instead of dumping JSON.
        if let Some((_, text)) = pi_shape::delta(update) {
            return text.to_string();
        }
        if let Some(text) = pi_shape::message_text(update) {
            return text;
        }
        if let Some(summary) = pi_shape::summary(update) {
            return summary;
        }
        if let Some(kind) = pi_shape::kind(update) {
            return format!("[{kind}]");
        }
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
            managed_worktree: true,
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
            native_session_file: None,
            native_terminal: false,
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

        let backend = TestBackend::new(120, 30);
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
        assert!(text.contains(" Send "), "input box: {text}");
        assert!(text.contains("Enter send"), "status bar: {text}");
    }

    #[test]
    fn draw_empty_app_shows_hint() {
        let app = App::new(vec![], vec![], vec![], vec![]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("no session selected"), "{text}");
        assert!(text.contains("new session"), "empty hint keys: {text}");
        assert!(text.contains("+ New space"), "onboarding hint: {text}");
        assert!(text.contains("New conversation"));
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

    /// Pi-native passthrough records dig meaningful text by `type` —
    /// never a raw JSON dump.
    #[test]
    fn session_update_text_handles_pi_shapes() {
        // A `message_update` text_delta yields the delta itself.
        let v = serde_json::json!({"type":"message_update","usage":{},
            "assistantMessageEvent":{"type":"text_delta","contentIndex":0,
            "delta":"pi says hi"}});
        assert_eq!(session_update_text(&v), "pi says hi");

        // `message_end` yields the joined message text.
        let v = serde_json::json!({"type":"message_end","message":
            {"role":"assistant","content":[{"type":"text","text":"all of it"}]}});
        assert_eq!(session_update_text(&v), "all of it");

        // Tool records and lifecycle get summaries/labels.
        let v = serde_json::json!({"type":"tool_execution_start","toolCallId":"t",
            "toolName":"edit","args":{"path":"src/x.rs"}});
        assert_eq!(session_update_text(&v), "tool edit started (src/x.rs)");
        let v = serde_json::json!({"type": "agent_settled"});
        assert_eq!(session_update_text(&v), "run settled");
        // Scaffolding without a summary still gets a tag, not JSON.
        let v = serde_json::json!({"type": "turn_start"});
        assert_eq!(session_update_text(&v), "[turn_start]");
    }

    /// A raw pi `text_delta` (defensive path — the conn normalizes
    /// these upstream) renders as agent prose, coalescing under one
    /// header like ACP chunks do.
    #[test]
    fn pi_delta_renders_as_agent_prose() {
        let app = app_with_events(vec![
            EventKind::SessionUpdate(serde_json::json!({"type":"message_update",
                "assistantMessageEvent":{"type":"text_delta","delta":"hello "}})),
            EventKind::SessionUpdate(serde_json::json!({"type":"message_update",
                "assistantMessageEvent":{"type":"text_delta","delta":"pi"}})),
        ]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("hello pi"), "pi deltas as prose: {text}");
        assert!(
            !text.contains("\"type\""),
            "no raw JSON in the pane: {text}"
        );
    }

    /// pi lifecycle/passthrough records render as quiet lines — or not
    /// at all — but never as truncated JSON walls.
    #[test]
    fn pi_lifecycle_renders_quietly_without_json() {
        let mut app = app_with_events(vec![
            EventKind::SessionUpdate(serde_json::json!({"type": "agent_start"})),
            EventKind::SessionUpdate(serde_json::json!({"type": "turn_start"})),
            EventKind::SessionUpdate(serde_json::json!({"type": "agent_end",
                "messages": [], "willRetry": false})),
            EventKind::SessionUpdate(serde_json::json!({"type": "agent_settled"})),
        ]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(!text.contains("run settled"), "{text}");
        assert!(!text.contains("agent run finished"), "{text}");
        app.wb.tools_expanded = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(
            text.contains("run settled"),
            "details retain lifecycle: {text}"
        );
        assert!(text.contains("agent run finished"), "{text}");
        // Scaffolding is invisible; nothing dumps raw JSON.
        assert!(!text.contains("turn_start"), "{text}");
        assert!(!text.contains("\"type\""), "no raw JSON: {text}");
    }

    /// A `message_update` whose `assistantMessageEvent` is `error`
    /// renders a warning line rather than vanishing.
    #[test]
    fn pi_message_error_renders_warning() {
        let app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "error", "error": "provider exploded"}
        }))]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("provider exploded"), "{text}");
    }

    // --- structured event rendering --------------------------------------

    #[test]
    fn collapsed_tool_updates_keep_title_and_latest_status() {
        let app = app_with_events(vec![
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"tool_call", "toolCallId":"same", "title":"Read config", "status":"in_progress"}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"tool_call_update", "toolCallId":"same", "status":"completed"}),
            ),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert_eq!(text.matches("Read config").count(), 1);
        assert!(text.contains("completed"));
        assert!(!text.contains("in_progress"));
    }

    #[test]
    fn running_status_appears_only_in_footer() {
        let app = app_with_events(vec![EventKind::StateChanged {
            from: SessionState::Ready,
            to: SessionState::Prompting,
        }]);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert_eq!(text.matches("Waiting ·").count(), 1, "{text}");
        assert!(text.lines().last().unwrap().contains("Waiting ·"), "{text}");
    }

    #[test]
    fn reasoning_stream_is_visible_foldable_and_restored_from_history() {
        let mut app = app_with_events(vec![
            EventKind::SessionUpdate(
                serde_json::json!({"type":"message_update", "assistantMessageEvent":{"type":"thinking_start"}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_thought_chunk", "content":{"text":"Inspect request. "}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_thought_chunk", "content":{"text":"Check source files."}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"type":"message_update", "assistantMessageEvent":{"type":"thinking_end"}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text":"Final answer here."}}),
            ),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(
            text.contains("Inspect request. Check source files."),
            "{text}"
        );
        assert_eq!(text.matches("Thought ·").count(), 1, "{text}");
        let target = app
            .wb
            .hits
            .borrow()
            .iter()
            .find_map(|h| {
                matches!(h.target, crate::interaction::Target::Reasoning(..))
                    .then(|| h.target.clone())
            })
            .expect("reasoning header is clickable");
        crate::interaction::activate(&mut app, target.clone());
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(!text.contains("Inspect request."));
        assert!(text.contains("Thought ·"));
        assert!(text.contains("Final answer here."));
        crate::interaction::activate(&mut app, target);
        let id = app.selected_session_id().unwrap();
        let history = app.events.clone();
        app.events.clear();
        app.wb.reasoning.clear();
        app.merge_history(id, history, false, None, 0);
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(buffer_text(terminal.backend()).contains("Inspect request. Check source files."));
    }

    #[test]
    fn hidden_lifecycle_preserves_message_boundaries() {
        let app = app_with_events(vec![
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text":"first reply"}}),
            ),
            EventKind::StateChanged {
                from: SessionState::Prompting,
                to: SessionState::Ready,
            },
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text":"second reply"}}),
            ),
        ]);
        let blocks: Vec<_> = app
            .events_for_selected()
            .map(|e| display_block(e, &app, 80))
            .collect();
        let lines = render_blocks(
            blocks.iter(),
            "agent",
            &app,
            &mut Vec::new(),
            80,
            usize::MAX,
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.to_string().trim() == "agent")
                .count(),
            2
        );
        assert!(!lines
            .iter()
            .any(|l| l.to_string().contains("first replysecond reply")));
    }

    #[test]
    fn markdown_formats_prose_but_preserves_code_and_incomplete_markers() {
        let lines = full_prose(
            "- **模型**: `deepseek`\n```rust\nlet pattern = \"**raw**\";\n```\npartial **bold",
            THEME.text,
            80,
        );
        let rendered = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("模型: deepseek"), "{rendered}");
        assert!(rendered.contains("\"**raw**\""));
        assert!(rendered.contains("partial **bold"));
        assert!(lines[0].spans.iter().any(|s| s.content == "模型"
            && s.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)));
    }

    #[test]
    fn a_single_long_reply_remains_scrollable_past_the_cached_tail() {
        let source = format!(
            "```rust\n{}\n```",
            (0..120)
                .map(|i| format!("let line_{i} = {i};\n"))
                .collect::<String>()
        );
        let mut app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"text":source}
        }))]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(buffer_text(terminal.backend()).contains("line_119"));
        for _ in 0..12 {
            app.scroll_by(true, 10);
            terminal.draw(|f| draw(f, &app)).unwrap();
        }
        assert!(buffer_text(terminal.backend()).contains("line_0"));
    }

    #[test]
    fn streamed_reply_keeps_fences_when_viewport_starts_in_middle() {
        let mut kinds = Vec::new();
        let reply = format!(
            "```rust\n{}\n```\nAfter code.",
            "    let 中文 = 42;\n".repeat(120)
        );
        for c in reply.chars() {
            kinds.push(EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate":"agent_message_chunk", "content":{"text":c.to_string()}
            })));
        }
        let app = app_with_events(kinds);
        let last = app.events_for_selected().last().unwrap();
        let block = display_block(last, &app, 42);
        let lines = render_blocks(
            std::iter::once(&block),
            "Pi",
            &app,
            &mut Vec::new(),
            42,
            usize::MAX,
        );
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("╭─ rust"));
        assert_eq!(text.matches("let 中文 = 42;").count(), 120);
        assert!(text.contains("After code."));
        // A scrolled anchor must not leak later text from the same cached reply.
        let early = app.events_for_selected().nth(40).unwrap();
        let block = display_block(early, &app, 30);
        let lines = render_blocks(
            std::iter::once(&block),
            "Pi",
            &app,
            &mut Vec::new(),
            30,
            usize::MAX,
        );
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("╭─ rust"));
        assert!(!text.contains("After code."));
    }

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
        // The conversation has one role header, regardless of chunk count.
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .content
                .chunks(80)
                .filter(|row| row.iter().map(|c| c.symbol()).collect::<String>().trim() == "claude")
                .count(),
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
        let mut app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
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
        app.wb.tools_expanded = true;
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
        let mut app = app_with_events(vec![EventKind::StateChanged {
            from: SessionState::Ready,
            to: SessionState::Prompting,
        }]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(!text.contains("ready → prompting"), "{text}");
        app.wb.tools_expanded = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(
            text.contains("ready → prompting"),
            "details preserve lifecycle: {text}"
        );
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
        let backend = TestBackend::new(120, 30);
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
        assert!(text.contains("New space · Choose project"), "{text}");
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
        assert!(text.contains("Allow once"), "{text}");
        assert!(text.contains("Always allow"), "{text}");
        assert!(text.contains("reject"), "{text}");

        // In-flight answer: hints swap to "answering…".
        app.permission.as_mut().unwrap().pending = true;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("Answering"), "{text}");
        assert!(!text.contains("Allow once"), "{text}");
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
        app.wb.files.source = crate::files::Source::Activity;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = buffer_text(terminal.backend());
        assert!(text.contains("Activity"), "{text}");
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
        // At 80 columns the conversation occupies the full width.
        let last_inner_y = h - 10;
        let x0 = 1;
        let last_inner: String = buf.content[last_inner_y * w + x0..last_inner_y * w + (w - 1)]
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            !last_inner.trim().is_empty(),
            "merged chunk stream must fill to the pane bottom: {last_inner:?}"
        );
    }

    #[test]
    fn history_repair_refreshes_grouping_even_when_event_count_is_unchanged() {
        let mut app = app_with_events(vec![
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text":"BEGIN"}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text":"_END"}}),
            ),
        ]);
        let id = app.selected_session_id().unwrap();
        // Model a projection that missed a delta before stream recovery.
        let mut partial = crate::reasoning::Reasoning::default();
        partial.observe(&app.events[0]);
        app.wb.reasoning.insert(id, partial);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(!buffer_text(terminal.backend()).contains("BEGIN_END"));
        let count = app.events.len();
        app.merge_history(id, app.events.clone(), false, None, 0);
        assert_eq!(app.events.len(), count);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(buffer_text(terminal.backend()).contains("BEGIN_END"));
        assert_eq!(app.wb.layout_builds.get(), 2);
    }

    #[test]
    fn history_fetch_waits_until_near_the_oldest_loaded_row() {
        let text = format!("```text\n{}\n```", "history line\n".repeat(400));
        let mut app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"text":text}
        }))]);
        let id = app.selected_session_id().unwrap();
        app.wb.sessions.entry(id).or_default().older = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        app.scroll_by(true, 3);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(!app.needs_older_history());
        app.scroll_by(true, 380);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(app.needs_older_history());
        app.wb.sessions.get_mut(&id).unwrap().history_retry_after =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert!(
            !app.needs_older_history(),
            "failed prefetch must not retry every frame"
        );
        app.wb.sessions.get_mut(&id).unwrap().history_retry_after = None;
        app.wb.sessions.get_mut(&id).unwrap().loading = true;
        assert!(!app.needs_older_history(), "one history request at a time");
        // A completed history page extends the cached viewport and clears its
        // previous bound. Scrolling back down must not keep fetching pages.
        app.merge_history(
            id,
            vec![Event {
                session_id: id,
                seq: 0,
                ts: chrono::DateTime::from_timestamp(0, 0).unwrap(),
                kind: EventKind::SessionUpdate(serde_json::json!({
                    "sessionUpdate":"user_message_chunk", "content":{"text":"older user prompt"}
                })),
            }],
            false,
            None,
            0,
        );
        app.scroll_by(true, 1000);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(buffer_text(terminal.backend()).contains("older user prompt"));
        assert!(!app.needs_older_history());
    }

    #[test]
    fn wheel_scrolling_reuses_layout_and_prefetches_older_rows() {
        let text = format!(
            "```rust\n{}\n```",
            (0..700)
                .map(|i| format!("let row_{i} = {i};\n"))
                .collect::<String>()
        );
        let mut app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"text":text}
        }))]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(buffer_text(terminal.backend()).contains("row_699"));
        for _ in 0..20 {
            app.scroll_by(true, 5);
            terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        }
        assert_eq!(
            app.wb.layout_builds.get(),
            1,
            "wheel movements should only move the viewport"
        );
        assert!(buffer_text(terminal.backend()).contains("row_599"));
        app.scroll_by(true, 200);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(
            app.wb.layout_builds.get(),
            2,
            "prefetch only when crossing the cached window"
        );
        assert!(buffer_text(terminal.backend()).contains("row_399"));
        app.follow_latest();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(app.wb.layout_builds.get(), 2);
        assert!(buffer_text(terminal.backend()).contains("row_699"));
    }

    #[test]
    fn cached_layout_refreshes_for_folding_resize_and_live_output() {
        let mut app = app_with_events(vec![
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_thought_chunk", "content":{"text":"cached thought"}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text":"reply"}}),
            ),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        app.scroll_by(true, 2);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(app.wb.layout_builds.get(), 1);
        let id = app.selected_session_id().unwrap();
        app.handle_event(Event {
            session_id: id,
            seq: 3,
            ts: Utc::now(),
            kind: EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate":"agent_message_chunk", "content":{"text":" newest"}
            })),
        });
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(
            app.wb.layout_builds.get(),
            1,
            "new output must not reflow a frozen history viewport"
        );
        assert!(!buffer_text(terminal.backend()).contains("newest"));
        app.wb.reasoning_toggles.insert((id, 1));
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(app.wb.layout_builds.get(), 2);
        assert!(!buffer_text(terminal.backend()).contains("cached thought"));
        terminal.backend_mut().resize(40, 20);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(app.wb.layout_builds.get(), 3);
        app.follow_latest();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(app.wb.layout_builds.get(), 4);
        assert!(buffer_text(terminal.backend()).contains("reply newest"));
    }

    #[test]
    #[ignore = "manual scrolling latency benchmark"]
    fn benchmark_scroll_latency() {
        let mut app = app_with_events(vec![
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_thought_chunk", "content":{"text": "分析输入状态与消息渲染路径，保留中文和完整上下文。\n\n".repeat(900)}}),
            ),
            EventKind::SessionUpdate(
                serde_json::json!({"sessionUpdate":"agent_message_chunk", "content":{"text": "```rust\nfn main() {}\n```"}}),
            ),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        let mut samples = Vec::new();
        for i in 0..120 {
            let start = std::time::Instant::now();
            app.scroll_by(i < 60, 10);
            terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(f64::total_cmp);
        println!(
            "scroll latency: median {:.2} ms, p95 {:.2} ms, max {:.2} ms",
            samples[60], samples[114], samples[119]
        );
    }

    /// Scratch: eyeball a representative frame.
    #[test]
    #[ignore]
    fn print_frame() {
        let ws = workspace("agentmux");
        let ws2 = workspace("review");
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
                    agent_name: "Pi".into(),
                    workspace_name: "agentmux".into(),
                },
                SessionView {
                    session: s2,
                    agent_name: "Codex".into(),
                    workspace_name: "agentmux".into(),
                },
                SessionView {
                    session: s3,
                    agent_name: "Claude".into(),
                    workspace_name: "review".into(),
                },
            ],
            vec![],
        );
        app.set_title_from_prompt(sid, "优化多 Agent 工作台");
        app.set_title_from_prompt(sid2, "检查交互与回归测试");
        app.set_title_from_prompt(sid3, "审核布局与可访问性");
        let kinds = vec![
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "参考 OpenCode，重新设计这个多 Agent 工作台。\n保留鼠标操作，让界面更简洁。"}
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "先检查当前的布局、事件流和输入状态。\n将思考与正文分开展示，工具执行保持紧凑，避免状态变化打断阅读。"}
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "我会把对话放在中心，任务与工作区信息收在右侧。\n先检查布局和配色，再调整输入框与消息样式。"}
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "inspect", "title": "Read tui/src/shell.rs", "status": "in_progress"
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "inspect", "status": "completed"
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "tool_call", "toolCallId": "edit", "title": "Edit theme.rs, shell.rs, ui.rs", "status": "completed"
            })),
            EventKind::SessionUpdate(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "### 布局已更新\n\n- **对话优先**：移除重复标题与状态流水。\n- **多 Agent 协作**：右侧切换任务，独立保留草稿。\n- **直接输入**：支持中文、多行粘贴和鼠标点击。\n\n代码现在有独立边框和语法高亮：\n\n```rust\nfn main() {\n    println!(\"你好，AgentMux!\");\n}\n```"}
            })),
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
        app.mode = InputMode::Editing;
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

        let width = std::env::var("AGENTMUX_PREVIEW_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(140);
        let height = std::env::var("AGENTMUX_PREVIEW_HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(38);
        app.wb.columns = width;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend().buffer();
        if let Ok(path) = std::env::var("AGENTMUX_PREVIEW_PATH") {
            let cells: Vec<_> = buf.content.iter().map(|c| serde_json::json!({
                "text": c.symbol(), "fg": format!("{:?}", c.fg), "bg": format!("{:?}", c.bg),
                "bold": c.modifier.contains(ratatui::style::Modifier::BOLD),
            })).collect();
            std::fs::write(
                path,
                serde_json::to_vec(
                    &serde_json::json!({"width": width, "height": height, "cells": cells}),
                )
                .unwrap(),
            )
            .unwrap();
        }
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
    #[test]
    fn long_messages_can_be_read_from_the_start_while_new_output_arrives() {
        let mut app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":(0..100).map(|i|format!("line-{i:03}\n")).collect::<String>()}
        }))]);
        app.scroll_by(true, 1000);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        let first = buffer_text(terminal.backend());
        assert!(first.contains("line-000"));
        let body = terminal.backend().buffer().content[80..880].to_vec();
        let id = app.selected_session_id().unwrap();
        app.handle_event(Event {
            session_id: id,
            seq: 999,
            ts: Utc::now(),
            kind: EventKind::Orchestrator("NEW STREAM EVENT".into()),
        });
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert_eq!(body, terminal.backend().buffer().content[80..880]);
        app.follow_latest();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(buffer_text(terminal.backend()).contains("NEW STREAM EVENT"));
    }

    #[test]
    fn expanded_tool_output_is_scrollable_beyond_the_old_line_cap() {
        let mut app = app_with_events(vec![EventKind::SessionUpdate(serde_json::json!({
            "sessionUpdate":"tool_call","title":"long tool","status":"completed",
            "rawOutput":(0..100).map(|i|format!("tool-line-{i:03}\n")).collect::<String>()
        }))]);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(!buffer_text(terminal.backend()).contains("tool-line"));
        app.wb.tools_expanded = true;
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(buffer_text(terminal.backend()).contains("tool-line-099"));
        app.scroll_by(true, 1000);
        terminal.draw(|f| draw_events(f, &app, f.area())).unwrap();
        assert!(buffer_text(terminal.backend()).contains("tool-line-000"));
    }
}
