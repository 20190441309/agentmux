//! Repair lossy live notifications from the authoritative, paginated log.
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use agentmux_client::DaemonClient;
use agentmux_core::{rpc::SessionHistoryResult, Event, EventKind, SessionId};
use tokio::sync::mpsc;

use crate::{app::App, UiMsg};

pub fn is_lag(event: &Event) -> bool {
    event.session_id.0.is_nil()
        && matches!(&event.kind, EventKind::Orchestrator(note)
        if note.starts_with("client event stream lagged;") || note.starts_with("subscriber lagged;"))
}

pub struct Recovery {
    seen: HashMap<SessionId, u64>,
    pending: HashMap<SessionId, u64>,
    active: HashMap<SessionId, u64>,
    retry_at: Instant,
}

impl Recovery {
    pub fn new(app: &App) -> Self {
        let mut seen: HashMap<_, _> = app.sessions.iter().map(|s| (s.session.id, 0)).collect();
        for event in &app.events {
            if let Some(seq) = seen.get_mut(&event.session_id) {
                *seq = (*seq).max(event.seq);
            }
        }
        // A 200-event initial page can begin in the middle of a streamed
        // message. Hydrate the current conversation in the background too.
        let pending = seen
            .keys()
            .filter(|id| app.wb.sessions.get(id).is_some_and(|s| s.older))
            .map(|id| (*id, 0))
            .collect();
        Self {
            seen,
            pending,
            active: HashMap::new(),
            retry_at: Instant::now(),
        }
    }

    fn request(&mut self, id: SessionId, after: u64) {
        let after = self.active.get(&id).copied().unwrap_or(after).min(after);
        self.pending
            .entry(id)
            .and_modify(|seq| *seq = (*seq).min(after))
            .or_insert(after);
    }

    /// Return false for transport notices and already loaded events.
    pub fn observe(&mut self, event: &Event) -> bool {
        if is_lag(event) {
            for (id, seq) in self.seen.clone() {
                self.request(id, seq);
            }
            return false;
        }
        if event.session_id.0.is_nil() || event.seq == 0 {
            return true;
        }
        let previous = self.seen.get(&event.session_id).copied().unwrap_or(0);
        if event.seq <= previous {
            return false;
        }
        if event.seq > previous.saturating_add(1) {
            self.request(event.session_id, previous);
        }
        self.seen.insert(event.session_id, event.seq);
        true
    }

    pub fn schedule(&mut self, tx: &mpsc::Sender<UiMsg>, socket: &Path) {
        if Instant::now() < self.retry_at {
            return;
        }
        let ids: Vec<_> = self
            .pending
            .keys()
            .filter(|id| !self.active.contains_key(id))
            .copied()
            .collect();
        for id in ids {
            let after = self.pending.remove(&id).unwrap();
            self.active.insert(id, after);
            let tx = tx.clone();
            let socket = socket.to_path_buf();
            tokio::spawn(async move {
                let result = fetch(&socket, id, after).await;
                let _ = tx
                    .send(UiMsg::Recovered {
                        session_id: id,
                        result,
                    })
                    .await;
            });
        }
    }

    pub fn finished(&mut self, id: SessionId, page: Option<&SessionHistoryResult>) {
        let after = self.active.remove(&id).unwrap_or(0);
        if let Some(page) = page {
            let latest = page.events.iter().map(|e| e.seq).max().unwrap_or(0);
            self.seen
                .entry(id)
                .and_modify(|seq| *seq = (*seq).max(latest))
                .or_insert(latest);
        } else {
            self.request(id, after);
            self.retry_at = Instant::now() + Duration::from_secs(1);
        }
    }
}

async fn fetch(socket: &Path, id: SessionId, after: u64) -> Result<SessionHistoryResult, String> {
    let mut client = DaemonClient::connect_existing(socket)
        .await
        .map_err(|e| e.to_string())?;
    client.set_request_timeout(Some(Duration::from_secs(10)));
    let mut page = client.history(id, None).await.map_err(|e| e.to_string())?;
    let floor = after.max(page.conversation_start);
    while let Some(first) = page.events.iter().map(|e| e.seq).min() {
        if !page.has_more || first <= floor.saturating_add(1) {
            break;
        }
        let older = client
            .history(id, Some(first))
            .await
            .map_err(|e| e.to_string())?;
        if older.events.is_empty() || older.events.iter().any(|e| e.seq >= first) {
            return Err("history pagination did not advance".into());
        }
        page.has_more = older.has_more;
        page.events.extend(older.events);
    }
    page.events.retain(|e| e.seq > floor);
    page.events.sort_by_key(|e| e.seq);
    Ok(page)
}

pub fn apply(app: &mut App, id: SessionId, mut page: SessionHistoryResult) {
    // Repair state without replaying obsolete state transitions over newer ones.
    if let Some(latest) = page
        .events
        .iter()
        .rev()
        .find(|e| matches!(e.kind, EventKind::StateChanged { .. }))
    {
        if !app.events.iter().any(|e| {
            e.session_id == id
                && e.seq >= latest.seq
                && matches!(e.kind, EventKind::StateChanged { .. })
        }) {
            app.handle_event(latest.clone());
        }
    }
    for event in &page.events {
        if let EventKind::PermissionResolved { request_id, .. } = &event.kind {
            let newer_request = app.events.iter().any(|e| e.session_id == id && e.seq > event.seq
                && matches!(&e.kind, EventKind::PermissionRequest { request_id: key, .. } if key == request_id));
            if !newer_request {
                app.handle_event(event.clone());
            }
        }
    }
    for event in std::mem::take(&mut page.pending_permissions) {
        if let EventKind::PermissionRequest { request_id, .. } = &event.kind {
            let resolved = app.events.iter().any(|e| e.session_id == id && e.seq > event.seq
                && (matches!(&e.kind, EventKind::PermissionResolved { request_id: key, .. } if key == request_id)
                    || matches!(e.kind, EventKind::StateChanged { to: agentmux_core::SessionState::Done | agentmux_core::SessionState::Error(_), .. })
                    || e.resets_conversation_view()));
            if !resolved {
                app.handle_event(event);
            }
        }
    }
    let older = app.wb.sessions.get(&id).is_some_and(|s| s.older);
    page.has_more = older;
    app.merge_history_page(id, page);
    app.set_status("Output synced");
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmux_core::SessionState;
    use serde_json::json;

    fn event(id: SessionId, seq: u64, kind: EventKind) -> Event {
        Event {
            session_id: id,
            seq,
            ts: chrono::DateTime::from_timestamp(seq as i64, 0).unwrap(),
            kind,
        }
    }
    fn empty_app() -> App {
        App::new(vec![], vec![], vec![], vec![])
    }
    fn page(events: Vec<Event>) -> SessionHistoryResult {
        SessionHistoryResult {
            events,
            conversation_start: 0,
            has_more: false,
            title: None,
            title_seq: 0,
            pending_permissions: vec![],
            event_refs: vec![],
            pending_permission_refs: vec![],
            available_commands: None,
            available_commands_ref: None,
        }
    }

    #[test]
    fn gaps_coalesce_and_lag_without_a_following_event_still_repairs_all_sessions() {
        let mut recovery = Recovery::new(&empty_app());
        let id = SessionId::new();
        let other = SessionId::new();
        assert!(recovery.observe(&event(id, 1, EventKind::Orchestrator("first".into()))));
        assert!(recovery.observe(&event(other, 1, EventKind::Orchestrator("other".into()))));
        recovery.observe(&event(id, 700, EventKind::Orchestrator("tail".into())));
        recovery.observe(&event(id, 900, EventKind::Orchestrator("tail".into())));
        assert_eq!(recovery.pending[&id], 1);
        let lag = event(
            SessionId(uuid::Uuid::nil()),
            0,
            EventKind::Orchestrator("client event stream lagged; dropped 20 event(s)".into()),
        );
        assert!(!recovery.observe(&lag));
        assert_eq!(recovery.pending[&id], 1);
        assert_eq!(recovery.pending[&other], 1);
        assert!(!recovery.observe(&event(id, 900, EventKind::ConversationStarted)));
    }

    #[tokio::test]
    async fn repair_reads_multiple_pages_and_rebuilds_complete_reasoning_without_duplicates() {
        use std::io::{BufRead, Write};
        let id = SessionId::new();
        let events: Vec<_> = (1..=705)
            .map(|seq| {
                event(
                    id,
                    seq,
                    EventKind::SessionUpdate(json!({
                        "sessionUpdate":"agent_thought_chunk", "content":{"text":"中a"}
                    })),
                )
            })
            .collect();
        let socket =
            std::env::temp_dir().join(format!("agentmux-repair-{}.sock", uuid::Uuid::new_v4()));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let source = events.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut requests = 0;
            for line in reader.lines() {
                let request: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
                let before = request["params"]["before_seq"].as_u64().unwrap_or(u64::MAX);
                let eligible: Vec<_> = source.iter().filter(|e| e.seq < before).cloned().collect();
                let mut reply = page(eligible.iter().rev().take(128).cloned().collect());
                reply.events.reverse();
                reply.has_more = eligible.len() > reply.events.len();
                writeln!(
                    stream,
                    "{}",
                    json!({"jsonrpc":"2.0","id":request["id"],"result":reply})
                )
                .unwrap();
                requests += 1;
            }
            requests
        });
        let recovered = fetch(&socket, id, 1).await.unwrap();
        assert_eq!(recovered.events.len(), 704);
        let mut app = empty_app();
        for i in [0, 301, 704] {
            app.handle_event(events[i].clone());
        }
        apply(&mut app, id, recovered);
        assert_eq!(app.events.len(), 705);
        assert_eq!(
            app.wb.reasoning[&id].blocks.values().next().unwrap().text,
            "中a".repeat(705)
        );
        assert!(
            tokio::task::spawn_blocking(move || server.join().unwrap())
                .await
                .unwrap()
                >= 6,
            "must go beyond the latest page"
        );
        std::fs::remove_file(socket).unwrap();
    }

    #[test]
    fn repaired_state_does_not_override_newer_live_state() {
        let mut app = empty_app();
        let id = SessionId::new();
        let latest = event(
            id,
            50,
            EventKind::StateChanged {
                from: SessionState::Ready,
                to: SessionState::Prompting,
            },
        );
        app.handle_event(latest);
        apply(
            &mut app,
            id,
            page(vec![event(
                id,
                40,
                EventKind::StateChanged {
                    from: SessionState::Prompting,
                    to: SessionState::Ready,
                },
            )]),
        );
        assert_eq!(app.wb.reasoning[&id].phase, "Waiting");
        assert_eq!(app.events.last().unwrap().seq, 50);
    }
}
