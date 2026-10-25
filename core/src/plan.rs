//! Delegation plans: the data model and the pure logic around them.
//!
//! A [`Plan`] is a user-approved split of one goal into [`PlanTask`]s, each
//! assigned to a session of the same workspace. Everything here is free of
//! I/O so the rules (proposal parsing, dependency validation, readiness,
//! settling) are unit-testable; the orchestrator's coordinator drives them.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{PlanId, SessionId, WorkspaceId};
use crate::Result;

/// Most tasks one plan may hold.
pub const MAX_TASKS: usize = 20;
const MAX_TITLE: usize = 120;
const MAX_PROMPT: usize = 20_000;
/// Stored result excerpt per task.
pub const MAX_RESULT: usize = 1_500;
/// Marker the proposal prompt carries; mock agents key off it in tests.
pub const PROPOSAL_MARKER: &str = "<agentmux-plan-request/>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// The proposer is still producing a plan.
    Proposing,
    /// A parsed plan awaiting the user's review and approval.
    Proposed,
    Running,
    /// Needs the user: a task failed, or the daemon restarted mid-run.
    Paused,
    Done,
    Cancelled,
    /// The proposal could not be turned into a plan.
    Failed,
}

impl PlanState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Ready,
    Running,
    Done,
    Failed,
    Skipped,
}

impl TaskState {
    /// Dependents may proceed past a finished or deliberately skipped task.
    pub fn satisfies_dependents(self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub assignee: Option<SessionId>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Paths the task is expected to touch (prefix match).
    #[serde(default)]
    pub scope_paths: Vec<String>,
    pub state: TaskState,
    /// Set the moment the prompt is sent; a task is never sent twice
    /// unless the user explicitly retries it.
    #[serde(default)]
    pub dispatched: bool,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub files_changed: Vec<String>,
}

impl PlanTask {
    pub fn new(id: &str, title: &str, prompt: &str) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            prompt: prompt.into(),
            assignee: None,
            depends_on: Vec::new(),
            scope_paths: Vec::new(),
            state: TaskState::Pending,
            dispatched: false,
            result: None,
            error: None,
            files_changed: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub workspace_id: WorkspaceId,
    pub goal: String,
    pub proposer: SessionId,
    pub state: PlanState,
    pub tasks: Vec<PlanTask>,
    /// Why the plan failed or paused, in plain words.
    #[serde(default)]
    pub error: Option<String>,
    /// The proposer's unparsed reply when parsing failed, so the user can
    /// read it instead of guessing what went wrong.
    #[serde(default)]
    pub raw_reply: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Mechanical wrap-up written when the plan completes.
    #[serde(default)]
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Plan {
    pub fn new(workspace_id: WorkspaceId, proposer: SessionId, goal: String) -> Self {
        let now = Utc::now();
        Self {
            id: PlanId::new(),
            workspace_id,
            goal,
            proposer,
            state: PlanState::Proposing,
            tasks: Vec::new(),
            error: None,
            raw_reply: None,
            warnings: Vec::new(),
            summary: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn task(&self, id: &str) -> Option<&PlanTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut PlanTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }
}

/// A task as the user edits it during review (runtime fields are reset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTaskSpec {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub assignee: Option<SessionId>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub scope_paths: Vec<String>,
}

impl From<PlanTaskSpec> for PlanTask {
    fn from(s: PlanTaskSpec) -> Self {
        PlanTask {
            assignee: s.assignee,
            depends_on: s.depends_on,
            scope_paths: s.scope_paths,
            ..PlanTask::new(&s.id, &s.title, &s.prompt)
        }
    }
}

/// What the user can do to one task of a running or paused plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskAction {
    /// Send a failed or skipped task again.
    Retry,
    /// Treat the task as done-without-work so dependents can proceed.
    Skip,
    /// Hand the task to another session of the workspace.
    Reassign { session_id: SessionId },
}

// ---------------------------------------------------------------------------
// Proposal parsing
// ---------------------------------------------------------------------------

/// The prompt asking `proposer` to split `goal`. `candidates` are the
/// sessions tasks may be assigned to: `(session id, agent label)`.
pub fn proposal_prompt(goal: &str, candidates: &[(SessionId, String)]) -> String {
    let mut out = format!(
        "{PROPOSAL_MARKER}\nSplit the goal below into tasks for the agents listed, who share one \
         git worktree. Do NOT start any work and do not edit files; only answer with the plan.\n\n\
         Goal: {goal}\n\nAgents (assign tasks by session id):\n"
    );
    for (id, label) in candidates {
        out.push_str(&format!("- {id} — {label}\n"));
    }
    out.push_str(
        "\nReply with ONE JSON object in a ```json fenced block, exactly this shape:\n\
         {\"tasks\":[{\"id\":\"t1\",\"title\":\"short title\",\"prompt\":\"full instructions for the agent\",\
         \"depends_on\":[],\"scope_paths\":[\"src/\"],\"assignee\":\"<session id>\"}]}\n\
         Rules: ids are unique and made of letters, digits, '-' or '_'; depends_on lists task ids that \
         must finish first; scope_paths lists the paths the task is expected to modify (give parallel \
         tasks disjoint paths); at most 20 tasks.",
    );
    out
}

#[derive(Deserialize)]
struct RawProposal {
    tasks: Vec<RawTask>,
}

#[derive(Deserialize)]
struct RawTask {
    id: String,
    title: String,
    prompt: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    scope_paths: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
}

/// Pull the plan JSON out of an agent reply: a fenced block, else the whole
/// reply. No repair is attempted — a reply that is not valid JSON of the
/// requested shape is an error the user sees.
fn extract_json(reply: &str) -> Result<Value> {
    let mut candidates = Vec::new();
    let mut rest = reply;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let Some(end) = after[body_start..].find("```") else {
            break;
        };
        candidates.push(after[body_start..body_start + end].trim());
        rest = &after[body_start + end + 3..];
    }
    candidates.push(reply.trim());
    // Prefer the last fenced block that parses (agents may quote the
    // template first and answer after it).
    for c in candidates.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(c) {
            if v.get("tasks").is_some() {
                return Ok(v);
            }
        }
    }
    bail!("the reply did not contain a JSON object with a \"tasks\" array")
}

/// Parse and validate a proposer reply into tasks. Assignees that are not
/// in `candidates` are left unassigned for the user to fill in.
pub fn parse_proposal(reply: &str, candidates: &[SessionId]) -> Result<Vec<PlanTask>> {
    let value = extract_json(reply)?;
    let raw: RawProposal = serde_json::from_value(value)
        .map_err(|e| anyhow!("the plan JSON has the wrong shape: {e}"))?;
    let tasks: Vec<PlanTask> = raw
        .tasks
        .into_iter()
        .map(|t| {
            let assignee = t
                .assignee
                .and_then(|a| a.parse::<uuid::Uuid>().ok())
                .map(SessionId)
                .filter(|s| candidates.contains(s));
            PlanTask {
                assignee,
                depends_on: t.depends_on,
                scope_paths: t.scope_paths,
                ..PlanTask::new(&t.id, &t.title, &t.prompt)
            }
        })
        .collect();
    validate_tasks(&tasks)?;
    Ok(tasks)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Structural rules: sane ids and sizes, known dependencies, no cycles.
pub fn validate_tasks(tasks: &[PlanTask]) -> Result<()> {
    ensure!(!tasks.is_empty(), "a plan needs at least one task");
    ensure!(
        tasks.len() <= MAX_TASKS,
        "a plan may hold at most {MAX_TASKS} tasks"
    );
    let mut ids = HashSet::new();
    for t in tasks {
        ensure!(valid_id(&t.id), "invalid task id {:?}", t.id);
        ensure!(ids.insert(t.id.as_str()), "duplicate task id {}", t.id);
        ensure!(
            !t.title.trim().is_empty() && t.title.chars().count() <= MAX_TITLE,
            "task {} needs a title of 1-{MAX_TITLE} characters",
            t.id
        );
        ensure!(
            !t.prompt.trim().is_empty() && t.prompt.chars().count() <= MAX_PROMPT,
            "task {} needs a prompt of 1-{MAX_PROMPT} characters",
            t.id
        );
    }
    for t in tasks {
        for d in &t.depends_on {
            ensure!(d != &t.id, "task {} depends on itself", t.id);
            ensure!(
                ids.contains(d.as_str()),
                "task {} depends on unknown task {d}",
                t.id
            );
        }
    }
    // Kahn's algorithm: anything left over is on a cycle.
    let mut indegree: HashMap<&str, usize> = tasks
        .iter()
        .map(|t| {
            (
                t.id.as_str(),
                t.depends_on.iter().collect::<HashSet<_>>().len(),
            )
        })
        .collect();
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut seen = 0;
    while let Some(id) = queue.pop() {
        seen += 1;
        for t in tasks {
            if t.depends_on.iter().any(|d| d == id) {
                let n = indegree.get_mut(t.id.as_str()).unwrap();
                *n -= 1;
                if *n == 0 {
                    queue.push(t.id.as_str());
                }
            }
        }
    }
    ensure!(seen == tasks.len(), "the task dependencies contain a cycle");
    Ok(())
}

fn path_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn reaches(tasks: &[PlanTask], from: &str, to: &str) -> bool {
    let Some(t) = tasks.iter().find(|t| t.id == from) else {
        return false;
    };
    t.depends_on
        .iter()
        .any(|d| d == to || reaches(tasks, d, to))
}

/// Warnings for tasks that may run in parallel yet declare overlapping
/// paths. Advisory only — overlap detection at runtime is the safety net.
pub fn scope_warnings(tasks: &[PlanTask]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in tasks.iter().enumerate() {
        for b in &tasks[i + 1..] {
            if reaches(tasks, &a.id, &b.id) || reaches(tasks, &b.id, &a.id) {
                continue;
            }
            for pa in &a.scope_paths {
                if let Some(pb) = b.scope_paths.iter().find(|pb| path_overlap(pa, pb)) {
                    out.push(format!(
                        "tasks {} and {} can run in parallel but both touch {pa} / {pb}",
                        a.id, b.id
                    ));
                    break;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Scheduling rules
// ---------------------------------------------------------------------------

/// Ids of tasks that may be sent now: `Pending`/`Ready` with every
/// dependency satisfied, an assignee, and that assignee not already busy
/// with a running task of this plan.
pub fn dispatchable(plan: &Plan) -> Vec<String> {
    if plan.state != PlanState::Running {
        return Vec::new();
    }
    let mut busy: HashSet<SessionId> = plan
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Running)
        .filter_map(|t| t.assignee)
        .collect();
    let mut out = Vec::new();
    for t in &plan.tasks {
        if !matches!(t.state, TaskState::Pending | TaskState::Ready) {
            continue;
        }
        let deps_ok = t
            .depends_on
            .iter()
            .all(|d| plan.task(d).is_some_and(|d| d.state.satisfies_dependents()));
        let Some(assignee) = t.assignee else { continue };
        if deps_ok && busy.insert(assignee) {
            out.push(t.id.clone());
        }
    }
    out
}

/// Recompute the plan's state from its tasks after any change.
///
/// - every task finished/skipped → `Done`
/// - nothing running or dispatchable but work remains blocked by a failed
///   task → `Paused` (the user must retry, reassign or skip)
pub fn settle(plan: &mut Plan) {
    if plan.state.is_terminal() || plan.state == PlanState::Proposed {
        return;
    }
    if plan.tasks.iter().all(|t| t.state.satisfies_dependents()) {
        plan.state = PlanState::Done;
        plan.error = None;
        return;
    }
    let running = plan.tasks.iter().any(|t| t.state == TaskState::Running);
    if running || !dispatchable_ignoring_state(plan).is_empty() {
        plan.state = PlanState::Running;
        return;
    }
    if let Some(failed) = plan.tasks.iter().find(|t| t.state == TaskState::Failed) {
        plan.state = PlanState::Paused;
        plan.error = Some(format!(
            "task {} failed: {}",
            failed.id,
            failed.error.as_deref().unwrap_or("unknown error")
        ));
    } else if plan.state == PlanState::Running {
        // Unassigned or otherwise stuck, with nothing failed.
        plan.state = PlanState::Paused;
        plan.error = Some("remaining tasks cannot start (missing assignee?)".into());
    }
}

fn dispatchable_ignoring_state(plan: &Plan) -> Vec<String> {
    let mut probe = plan.clone();
    probe.state = PlanState::Running;
    dispatchable(&probe)
}

/// Excerpt `text` to at most `max` characters on a char boundary.
pub fn excerpt(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// The `.agentmux/plan.md` blackboard rendering. `label` names a session.
pub fn render_markdown(plan: &Plan, label: &dyn Fn(SessionId) -> String) -> String {
    let mut out = format!(
        "# Plan: {}\n\nState: {:?}\n",
        plan.goal.replace('\n', " "),
        plan.state
    );
    if let Some(e) = &plan.error {
        out.push_str(&format!("\nAttention: {e}\n"));
    }
    out.push_str("\n## Tasks\n");
    for t in &plan.tasks {
        let mark = match t.state {
            TaskState::Done => "x",
            TaskState::Skipped => "-",
            _ => " ",
        };
        let who = t.assignee.map(label).unwrap_or_else(|| "unassigned".into());
        out.push_str(&format!(
            "\n- [{mark}] **{}** {} — {who} ({:?})\n",
            t.id, t.title, t.state
        ));
        if !t.depends_on.is_empty() {
            out.push_str(&format!("  - after: {}\n", t.depends_on.join(", ")));
        }
        if !t.scope_paths.is_empty() {
            out.push_str(&format!("  - scope: {}\n", t.scope_paths.join(", ")));
        }
        if let Some(r) = &t.result {
            out.push_str(&format!("  - result: {}\n", r.replace('\n', " ")));
        }
        if let Some(e) = &t.error {
            out.push_str(&format!("  - error: {}\n", e.replace('\n', " ")));
        }
    }
    if let Some(s) = &plan.summary {
        out.push_str(&format!("\n## Summary\n\n{s}\n"));
    }
    out
}

/// Mechanical wrap-up: per task result excerpts plus the files each touched.
pub fn render_summary(plan: &Plan, label: &dyn Fn(SessionId) -> String) -> String {
    let mut out = String::new();
    for t in &plan.tasks {
        let who = t.assignee.map(label).unwrap_or_else(|| "unassigned".into());
        out.push_str(&format!("- {} {} [{:?}, {who}]\n", t.id, t.title, t.state));
        if let Some(r) = &t.result {
            out.push_str(&format!(
                "  result: {}\n",
                excerpt(r, 300).replace('\n', " ")
            ));
        }
        if !t.files_changed.is_empty() {
            out.push_str(&format!("  files: {}\n", t.files_changed.join(", ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, deps: &[&str]) -> PlanTask {
        let mut task = PlanTask::new(id, &format!("title {id}"), "do it");
        task.depends_on = deps.iter().map(|d| d.to_string()).collect();
        task
    }

    fn plan_with(tasks: Vec<PlanTask>) -> Plan {
        let mut p = Plan::new(WorkspaceId::new(), SessionId::new(), "goal".into());
        p.tasks = tasks;
        p.state = PlanState::Running;
        p
    }

    #[test]
    fn parses_a_fenced_reply_and_filters_unknown_assignees() {
        let a = SessionId::new();
        let reply = format!(
            "Sure!\n```json\n{{\"tasks\":[{{\"id\":\"t1\",\"title\":\"A\",\"prompt\":\"p\",\
             \"assignee\":\"{a}\",\"scope_paths\":[\"src/\"]}},{{\"id\":\"t2\",\"title\":\"B\",\
             \"prompt\":\"q\",\"depends_on\":[\"t1\"],\"assignee\":\"{}\"}}]}}\n```\nDone.",
            SessionId::new()
        );
        let tasks = parse_proposal(&reply, &[a]).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].assignee, Some(a));
        assert_eq!(
            tasks[1].assignee, None,
            "unknown assignee must not be guessed"
        );
        assert_eq!(tasks[1].depends_on, vec!["t1"]);
    }

    #[test]
    fn bare_json_parses_but_prose_and_wrong_shapes_do_not() {
        let ok = r#"{"tasks":[{"id":"a","title":"x","prompt":"y"}]}"#;
        assert!(parse_proposal(ok, &[]).is_ok());
        for bad in [
            "I think we should split this into two parts.",
            r#"{"tasks":"nope"}"#,
            r#"{"tasks":[]}"#,
            r#"{"tasks":[{"id":"a","title":"x"}]}"#,
            "```json\n{not json}\n```",
        ] {
            assert!(parse_proposal(bad, &[]).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn the_last_valid_fenced_block_wins_over_a_quoted_template() {
        let reply = "Template:\n```json\n{\"tasks\":[{\"id\":\"t1\",\"title\":\"template\",\"prompt\":\"p\"}]}\n```\n\
                     Answer:\n```json\n{\"tasks\":[{\"id\":\"real\",\"title\":\"x\",\"prompt\":\"p\"}]}\n```";
        assert_eq!(parse_proposal(reply, &[]).unwrap()[0].id, "real");
    }

    #[test]
    fn validation_catches_structure_errors() {
        assert!(
            validate_tasks(&[t("a", &[]), t("a", &[])]).is_err(),
            "duplicate"
        );
        assert!(validate_tasks(&[t("a", &["zzz"])]).is_err(), "unknown dep");
        assert!(validate_tasks(&[t("a", &["a"])]).is_err(), "self dep");
        assert!(
            validate_tasks(&[t("a", &["b"]), t("b", &["a"])]).is_err(),
            "cycle"
        );
        assert!(validate_tasks(&[t("bad id", &[])]).is_err(), "id charset");
        let mut empty = t("a", &[]);
        empty.prompt = "  ".into();
        assert!(validate_tasks(&[empty]).is_err());
        let many: Vec<_> = (0..=MAX_TASKS).map(|i| t(&format!("t{i}"), &[])).collect();
        assert!(validate_tasks(&many).is_err());
        assert!(validate_tasks(&[t("a", &[]), t("b", &["a"]), t("c", &["a", "b"])]).is_ok());
    }

    #[test]
    fn scope_warnings_flag_only_parallel_overlap() {
        let mut a = t("a", &[]);
        a.scope_paths = vec!["src/".into()];
        let mut b = t("b", &[]);
        b.scope_paths = vec!["src/lib.rs".into()];
        let mut c = t("c", &["a"]);
        c.scope_paths = vec!["src/main.rs".into()];
        let w = scope_warnings(&[a, b, c]);
        assert_eq!(w.len(), 1, "{w:?}"); // a~b only; a→c overlaps but is ordered
        assert!(w[0].contains("tasks a and b"));
    }

    #[test]
    fn dispatch_respects_dependencies_assignees_and_busy_sessions() {
        let (s1, s2) = (SessionId::new(), SessionId::new());
        let mut a = t("a", &[]);
        a.assignee = Some(s1);
        let mut b = t("b", &[]);
        b.assignee = Some(s1); // same session as a: must wait its turn
        let mut c = t("c", &[]);
        c.assignee = Some(s2);
        let mut d = t("d", &["a"]);
        d.assignee = Some(s2);
        let e = t("e", &[]); // unassigned
        let plan = plan_with(vec![a, b, c, d, e]);
        assert_eq!(dispatchable(&plan), vec!["a", "c"]);
    }

    #[test]
    fn skipped_dependencies_unblock_and_settle_reaches_done() {
        let s = SessionId::new();
        let mut a = t("a", &[]);
        a.assignee = Some(s);
        a.state = TaskState::Skipped;
        let mut b = t("b", &["a"]);
        b.assignee = Some(s);
        let mut plan = plan_with(vec![a, b]);
        assert_eq!(dispatchable(&plan), vec!["b"]);
        plan.task_mut("b").unwrap().state = TaskState::Done;
        settle(&mut plan);
        assert_eq!(plan.state, PlanState::Done);
    }

    #[test]
    fn a_failure_pauses_only_when_nothing_else_can_move() {
        let (s1, s2) = (SessionId::new(), SessionId::new());
        let mut a = t("a", &[]);
        a.assignee = Some(s1);
        a.state = TaskState::Failed;
        a.error = Some("boom".into());
        let mut b = t("b", &["a"]);
        b.assignee = Some(s1);
        let mut c = t("c", &[]);
        c.assignee = Some(s2);
        let mut plan = plan_with(vec![a, b, c]);
        settle(&mut plan);
        assert_eq!(plan.state, PlanState::Running, "c can still run");
        plan.task_mut("c").unwrap().state = TaskState::Done;
        settle(&mut plan);
        assert_eq!(plan.state, PlanState::Paused);
        assert!(plan.error.as_deref().unwrap().contains("boom"));
    }

    #[test]
    fn markdown_lists_tasks_state_and_results() {
        let s = SessionId::new();
        let mut a = t("a", &[]);
        a.assignee = Some(s);
        a.state = TaskState::Done;
        a.result = Some("all good".into());
        let plan = plan_with(vec![a, t("b", &["a"])]);
        let md = render_markdown(&plan, &|_| "claude".into());
        assert!(md.contains("- [x] **a**") && md.contains("claude") && md.contains("all good"));
        assert!(md.contains("- [ ] **b**") && md.contains("unassigned") && md.contains("after: a"));
    }

    #[test]
    fn excerpt_truncates_on_char_boundaries() {
        assert_eq!(excerpt("héllo", 2), "hé…");
        assert_eq!(excerpt("  short ", 10), "short");
    }
}
