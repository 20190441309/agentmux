//! Per-conversation projection of the provider's actual reasoning stream.
use std::collections::{BTreeMap, HashMap};

use agentmux_core::{pi_shape, Event, EventKind, SessionState};
use chrono::{DateTime, Utc};

#[derive(Default)]
pub struct Reasoning {
    pub blocks: BTreeMap<u64, Thought>,
    pub events: HashMap<u64, u64>,
    pub replies: BTreeMap<u64, Reply>,
    pub reply_events: HashMap<u64, (u64, usize)>,
    active_reply: Option<u64>,
    reply_ready_at: Option<DateTime<Utc>>,
    active: Option<u64>,
    last_seq: u64,
    pub started: Option<DateTime<Utc>>,
    pub phase: &'static str,
}

/// Complete reply context keeps fences intact when only the tail is visible.
#[derive(Default)]
pub struct Reply {
    pub text: String,
    pub cache: std::cell::RefCell<Option<ReplyRender>>,
}
pub struct ReplyRender {
    pub width: usize,
    pub end: usize,
    pub lines: Vec<ratatui::text::Line<'static>>,
}

pub struct Thought {
    pub cache: std::cell::RefCell<Option<ReplyRender>>,
    pub text: String,
    pub started: DateTime<Utc>,
    pub ended: Option<DateTime<Utc>>,
}

impl Thought {
    pub fn duration(&self) -> i64 {
        (self.ended.unwrap_or_else(Utc::now) - self.started)
            .num_seconds()
            .max(0)
    }
}

impl Reasoning {
    fn finish(&mut self, ts: DateTime<Utc>) {
        if let Some(id) = self.active.take() {
            self.blocks.get_mut(&id).unwrap().ended = Some(ts);
        }
    }

    fn thought(&mut self, event: &Event, text: &str) {
        let id = *self.active.get_or_insert(event.seq);
        let block = self.blocks.entry(id).or_insert_with(|| Thought {
            cache: Default::default(),
            text: String::new(),
            started: event.ts,
            ended: None,
        });
        block.text.push_str(text);
        self.events.insert(event.seq, id);
        self.phase = "Thinking";
    }

    fn observe_reply(&mut self, event: &Event) {
        let EventKind::SessionUpdate(value) = &event.kind else {
            if matches!(
                event.kind,
                EventKind::StateChanged {
                    to: SessionState::Ready,
                    ..
                }
            ) {
                self.reply_ready_at = Some(event.ts);
            }
            if matches!(
                event.kind,
                EventKind::StateChanged {
                    to: SessionState::Prompting | SessionState::Done | SessionState::Error(_),
                    ..
                } | EventKind::AgentExited { .. }
            ) {
                self.active_reply = None;
                self.reply_ready_at = None;
            }
            // Ready can be persisted before fan-out drains the final deltas.
            // Message-end or the next prompt remains the reply boundary.
            return;
        };
        let value = value.get("update").unwrap_or(value);
        let update = value.get("sessionUpdate").and_then(|v| v.as_str());
        let delta = if update == Some("agent_message_chunk") {
            value.pointer("/content/text").and_then(|v| v.as_str())
        } else {
            pi_shape::delta(value)
                .and_then(|(role, text)| matches!(role, pi_shape::Delta::Message).then_some(text))
        };
        if let Some(text) = delta {
            if self.reply_ready_at.is_some_and(|ready| event.ts >= ready) {
                self.active_reply = None;
                self.reply_ready_at = None;
            }
            let id = *self.active_reply.get_or_insert(event.seq);
            let reply = self.replies.entry(id).or_default();
            reply.text.push_str(text);
            self.reply_events.insert(event.seq, (id, reply.text.len()));
        } else if matches!(
            update,
            Some("user_message_chunk" | "agent_thought_chunk" | "tool_call" | "tool_call_update")
        ) || matches!(
            pi_shape::kind(value),
            Some("message_start" | "message_end" | "agent_start" | "agent_end" | "agent_settled")
        ) || pi_shape::delta(value).is_some()
        {
            self.active_reply = None;
            self.reply_ready_at = None;
        }
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.last_seq {
            return;
        }
        if event.resets_conversation_view() {
            *self = Self::default();
        }
        self.last_seq = event.seq;
        self.observe_reply(event);
        match &event.kind {
            EventKind::StateChanged {
                to: SessionState::Prompting,
                ..
            } => {
                self.finish(event.ts);
                self.started = Some(event.ts);
                self.phase = "Waiting";
            }
            EventKind::StateChanged {
                to: SessionState::Ready | SessionState::Done | SessionState::Error(_),
                ..
            }
            | EventKind::AgentExited { .. } => {
                self.finish(event.ts);
                self.phase = "";
            }
            EventKind::SessionUpdate(value) => {
                let value = value.get("update").unwrap_or(value);
                match value.get("sessionUpdate").and_then(|v| v.as_str()) {
                    Some("agent_thought_chunk") => self.thought(
                        event,
                        value
                            .pointer("/content/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    ),
                    Some("agent_message_chunk") => {
                        self.finish(event.ts);
                        self.phase = "Responding";
                    }
                    Some("user_message_chunk") => {
                        self.finish(event.ts);
                        self.started = Some(event.ts);
                        self.phase = "Waiting";
                    }
                    Some("tool_call" | "tool_call_update") => {
                        self.finish(event.ts);
                        self.phase = if matches!(
                            value.get("status").and_then(|v| v.as_str()),
                            Some("completed" | "failed")
                        ) {
                            "Working"
                        } else {
                            "Running tool"
                        };
                    }
                    _ => {
                        if let Some((role, text)) = pi_shape::delta(value) {
                            match role {
                                pi_shape::Delta::Thought => self.thought(event, text),
                                pi_shape::Delta::Message => {
                                    self.finish(event.ts);
                                    self.phase = "Responding";
                                }
                            }
                        } else if pi_shape::kind(value) == Some("message_update") {
                            match value
                                .pointer("/assistantMessageEvent/type")
                                .and_then(|v| v.as_str())
                            {
                                Some("thinking_start") => {
                                    self.finish(event.ts);
                                    self.thought(event, "");
                                }
                                Some("thinking_end" | "error") => {
                                    self.finish(event.ts);
                                    self.phase = "Working";
                                }
                                Some("text_start") => {
                                    self.finish(event.ts);
                                    self.phase = "Responding";
                                }
                                _ => {}
                            }
                        } else if matches!(
                            pi_shape::kind(value),
                            Some("message_end" | "agent_end" | "agent_settled")
                        ) {
                            self.finish(event.ts);
                            self.phase = "Working";
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl crate::app::App {
    pub fn run_status(&self) -> Option<String> {
        if !self.wb.connected {
            return None;
        }
        let view = self.selected_session()?;
        let id = view.session.id;
        if !matches!(
            view.session.state,
            SessionState::Prompting | SessionState::WaitingPermission
        ) && !self.wb.in_flight.contains(&id)
        {
            return None;
        }
        let state = self.wb.reasoning.get(&id);
        let phase = if matches!(view.session.state, SessionState::WaitingPermission) {
            "Needs permission"
        } else {
            state
                .map(|s| s.phase)
                .filter(|s| !s.is_empty())
                .unwrap_or("Waiting")
        };
        let elapsed = state
            .and_then(|s| s.started)
            .map(|t| (Utc::now() - t).num_seconds().max(0))
            .unwrap_or(0);
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        Some(format!(
            "{} {phase} · {elapsed}s",
            spinner[(Utc::now().timestamp_millis() / 125).rem_euclid(8) as usize]
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentmux_core::SessionId;
    use serde_json::json;

    fn event(seq: u64, kind: EventKind) -> Event {
        Event {
            session_id: SessionId::new(),
            seq,
            ts: DateTime::from_timestamp(seq as i64, 0).unwrap(),
            kind,
        }
    }
    fn raw(seq: u64, kind: &str, text: &str) -> Event {
        event(
            seq,
            EventKind::SessionUpdate(json!({"type":"message_update",
            "assistantMessageEvent":{"type":kind,"delta":text}})),
        )
    }

    #[test]
    fn ready_before_final_deltas_does_not_split_the_reply() {
        let mut state = Reasoning::default();
        state.observe(&raw(1, "text_delta", "BEGIN_ANSW"));
        state.observe(&event(
            2,
            EventKind::StateChanged {
                from: SessionState::Prompting,
                to: SessionState::Ready,
            },
        ));
        let mut late = raw(3, "text_delta", "ER");
        late.ts = DateTime::from_timestamp(1, 500_000_000).unwrap();
        state.observe(&late);
        assert_eq!(state.replies.len(), 1);
        assert_eq!(state.replies[&1].text, "BEGIN_ANSWER");
        state.observe(&event(
            4,
            EventKind::StateChanged {
                from: SessionState::Ready,
                to: SessionState::Prompting,
            },
        ));
        state.observe(&raw(5, "text_delta", "next reply"));
        assert_eq!(state.replies.len(), 2);
        assert_eq!(state.replies[&5].text, "next reply");
    }
    #[test]
    fn streams_join_once_and_finish_before_the_answer() {
        let mut state = Reasoning::default();
        state.observe(&raw(1, "thinking_start", ""));
        let chunk = raw(2, "thinking_delta", "First. ");
        state.observe(&chunk);
        state.observe(&chunk);
        state.observe(&event(
            3,
            EventKind::SessionUpdate(json!({
            "sessionUpdate":"agent_thought_chunk", "content":{"text":"Second."}})),
        ));
        state.observe(&raw(4, "thinking_end", ""));
        assert_eq!(state.blocks[&1].text, "First. Second.");
        assert_eq!(state.blocks[&1].duration(), 3);
        assert_eq!(state.phase, "Working");
        state.observe(&raw(5, "text_delta", "answer"));
        assert_eq!(state.phase, "Responding");
        assert_eq!(state.blocks.len(), 1);
        state.observe(&raw(6, "thinking_start", ""));
        state.observe(&raw(7, "thinking_delta", "Next step."));
        assert_eq!(state.blocks.len(), 2);
        assert_eq!(state.blocks[&6].text, "Next step.");
    }
    #[test]
    fn text_only_turns_do_not_invent_reasoning_and_restart_discards_old_blocks() {
        let mut state = Reasoning::default();
        state.observe(&raw(1, "text_delta", "answer"));
        assert!(state.blocks.is_empty());
        state.observe(&raw(2, "thinking_delta", "thought"));
        state.observe(&event(
            3,
            EventKind::StateChanged {
                from: SessionState::Prompting,
                to: SessionState::Error("cancelled".into()),
            },
        ));
        assert!(state.blocks[&2].ended.is_some());
        state.observe(&event(4, EventKind::ConversationStarted));
        assert!(state.blocks.is_empty());
        assert!(state.events.is_empty());
    }
}
