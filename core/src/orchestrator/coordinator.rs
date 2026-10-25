//! Coordinator: runs user-approved delegation plans (see
//! `docs/superpowers/specs/2026-10-25-coordinator-design.md`).
//!
//! The pure rules live in [`crate::plan`]; this file does the I/O around
//! them — asking a proposer session for a plan, dispatching ready tasks as
//! ordinary prompts, recording results, and keeping `.agentmux/plan.md`
//! current. Plans are held in memory and persisted on every change; every
//! change is also announced as [`EventKind::PlanUpdated`] on the proposer
//! session.

use std::sync::Arc;

use anyhow::{anyhow, ensure, Context};
use chrono::Utc;

use super::Orchestrator;
use crate::id::{PlanId, SessionId, WorkspaceId};
use crate::model::{Event, EventKind, SessionState};
use crate::plan::{
    self, Plan, PlanState, PlanTask, PlanTaskSpec, TaskAction, TaskState, MAX_RESULT,
};
use crate::Result;

const MAX_GOAL: usize = 2_000;

impl Orchestrator {
    // ----- reads -----------------------------------------------------------

    pub fn plan_get(&self, plan_id: PlanId) -> Result<Plan> {
        self.plans
            .lock()
            .unwrap()
            .get(&plan_id)
            .cloned()
            .ok_or_else(|| anyhow!("no such plan {plan_id}"))
    }

    /// A workspace's plans, oldest first.
    pub fn plan_list(&self, workspace_id: WorkspaceId) -> Vec<Plan> {
        let mut plans: Vec<Plan> = self
            .plans
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.workspace_id == workspace_id)
            .cloned()
            .collect();
        plans.sort_by_key(|p| (p.created_at, p.id));
        plans
    }

    // ----- boot ------------------------------------------------------------

    /// Load persisted plans and make them honest about the restart: nothing
    /// is still running, so unfinished work waits for the user.
    pub(super) fn recover_plans(&self) {
        let plans = match self.sink.store.lock().unwrap().list_plans() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("agentmux: failed to load plans: {e:#}");
                return;
            }
        };
        for mut plan in plans {
            let before = plan.clone();
            match plan.state {
                PlanState::Proposing => {
                    plan.state = PlanState::Failed;
                    plan.error =
                        Some("the daemon restarted while the plan was being proposed".into());
                }
                PlanState::Running => {
                    for t in plan
                        .tasks
                        .iter_mut()
                        .filter(|t| t.state == TaskState::Running)
                    {
                        t.state = TaskState::Failed;
                        t.error = Some(
                            "daemon restarted; it is unknown whether this task completed".into(),
                        );
                    }
                    plan.state = PlanState::Paused;
                    plan.error = Some("the daemon restarted; resume the plan to continue".into());
                }
                _ => {}
            }
            if plan != before {
                plan.updated_at = Utc::now();
                if let Err(e) = self.sink.store.lock().unwrap().upsert_plan(&plan) {
                    eprintln!(
                        "agentmux: failed to persist recovered plan {}: {e:#}",
                        plan.id
                    );
                }
            }
            self.plans.lock().unwrap().insert(plan.id, plan);
        }
    }

    // ----- plumbing --------------------------------------------------------

    /// Apply `f` to a copy of the plan; if it succeeds and changed anything,
    /// persist, publish and return the new snapshot.
    fn mutate_plan<R>(
        &self,
        plan_id: PlanId,
        f: impl FnOnce(&mut Plan) -> Result<R>,
    ) -> Result<(R, Plan)> {
        let (out, snapshot, changed) = {
            let mut plans = self.plans.lock().unwrap();
            let current = plans
                .get(&plan_id)
                .ok_or_else(|| anyhow!("no such plan {plan_id}"))?;
            let mut next = current.clone();
            let out = f(&mut next)?;
            let changed = next != *current;
            if changed {
                next.updated_at = Utc::now();
                self.sink.store.lock().unwrap().upsert_plan(&next)?;
                plans.insert(plan_id, next.clone());
            }
            (out, next, changed)
        };
        if changed {
            self.publish_plan(&snapshot);
        }
        Ok((out, snapshot))
    }

    fn publish_plan(&self, plan: &Plan) {
        self.sink.ingest(&mut Event {
            session_id: plan.proposer,
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::PlanUpdated(Box::new(plan.clone())),
        });
        self.write_plan_md(plan);
    }

    /// Best-effort `.agentmux/plan.md`; a missing worktree must not break
    /// the plan.
    fn write_plan_md(&self, plan: &Plan) {
        let Ok(Some(ws)) = self.get_workspace(plan.workspace_id) else {
            return;
        };
        let dir = ws.worktree_path.join(crate::collab::SHARED_DIR);
        let md = plan::render_markdown(plan, &|s| self.session_label(s));
        if std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join("plan.md"), md))
            .is_err()
        {
            eprintln!("agentmux: could not write plan.md for plan {}", plan.id);
        }
    }

    /// `"<agent name> (<short id>)"` for display in prompts and plan.md.
    pub(super) fn session_label(&self, session_id: SessionId) -> String {
        let short: String = session_id.to_string().chars().take(8).collect();
        let name = self
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|s| {
                self.registry
                    .lock()
                    .unwrap()
                    .get(&s.agent_id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_else(|| "unknown agent".into());
        format!("{name} ({short})")
    }

    /// Sessions of the workspace that can take structured prompts.
    fn plan_candidates(&self, workspace_id: WorkspaceId) -> Result<Vec<SessionId>> {
        Ok(self
            .list_sessions(workspace_id)?
            .into_iter()
            .filter(|s| {
                !s.native_terminal
                    && !matches!(s.state, SessionState::Done | SessionState::Error(_))
            })
            .map(|s| s.id)
            .collect())
    }

    fn check_assignee(&self, workspace_id: WorkspaceId, session_id: SessionId) -> Result<()> {
        let s = self
            .get_session(session_id)?
            .ok_or_else(|| anyhow!("assignee {session_id} does not exist"))?;
        ensure!(
            s.workspace_id == workspace_id,
            "assignee {session_id} belongs to another workspace"
        );
        ensure!(
            !s.native_terminal,
            "native terminal sessions cannot be delegated to: they report no completion"
        );
        Ok(())
    }

    /// Agent text and edited files produced after `before_seq`.
    fn turn_output(&self, session_id: SessionId, before_seq: u64) -> (String, Vec<String>) {
        let mut text = String::new();
        let mut files = Vec::new();
        let Ok(events) = self.events(session_id) else {
            return (text, files);
        };
        for ev in events.iter().filter(|e| e.seq > before_seq) {
            match &ev.kind {
                EventKind::SessionUpdate(v) => {
                    let update = v.get("update").unwrap_or(v);
                    if update.get("sessionUpdate").and_then(|u| u.as_str())
                        == Some("agent_message_chunk")
                    {
                        if let Some(t) = update.pointer("/content/text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
                EventKind::FileEdited { path } => {
                    let p = path.display().to_string();
                    if !files.contains(&p) {
                        files.push(p);
                    }
                }
                _ => {}
            }
        }
        (text, files)
    }

    /// A turn's reply is persisted by the session's fan-out task, which may
    /// still be draining when `prompt` returns. Wait until the event log has
    /// been quiet for a few polls (bounded) before reading the turn back.
    async fn settle_events(&self, session_id: SessionId) {
        const POLL: std::time::Duration = std::time::Duration::from_millis(25);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut last = self.last_seq_of(session_id);
        let mut quiet = 0;
        while quiet < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(POLL).await;
            let now = self.last_seq_of(session_id);
            if now == last {
                quiet += 1;
            } else {
                quiet = 0;
                last = now;
            }
        }
    }

    fn last_seq_of(&self, session_id: SessionId) -> u64 {
        self.sink
            .store
            .lock()
            .unwrap()
            .last_seq(session_id)
            .unwrap_or(0)
    }

    // ----- propose / edit / approve -----------------------------------------

    /// Ask `proposer` to split `goal`. Returns at once with a `Proposing`
    /// plan; the proposer's turn runs in the background and the plan
    /// becomes `Proposed` (or `Failed` with the raw reply).
    pub fn plan_propose(
        self: &Arc<Self>,
        workspace_id: WorkspaceId,
        proposer: SessionId,
        goal: String,
    ) -> Result<Plan> {
        let goal = goal.trim().to_owned();
        ensure!(
            !goal.is_empty() && goal.chars().count() <= MAX_GOAL,
            "describe the goal in 1-{MAX_GOAL} characters"
        );
        let p = self
            .get_session(proposer)?
            .ok_or_else(|| anyhow!("no such session {proposer}"))?;
        ensure!(
            p.workspace_id == workspace_id,
            "the proposer is not in this workspace"
        );
        ensure!(
            !p.native_terminal,
            "a native terminal session cannot propose plans; pick a structured session"
        );
        ensure!(
            p.state == SessionState::Ready,
            "the proposer must be idle (state {:?})",
            p.state
        );
        let candidates = self.plan_candidates(workspace_id)?;
        let labelled: Vec<_> = candidates
            .iter()
            .map(|id| (*id, self.session_label(*id)))
            .collect();
        let prompt = plan::proposal_prompt(&goal, &labelled);

        let plan = Plan::new(workspace_id, proposer, goal);
        self.sink.store.lock().unwrap().upsert_plan(&plan)?;
        self.plans.lock().unwrap().insert(plan.id, plan.clone());
        self.publish_plan(&plan);

        let this = self.clone();
        let plan_id = plan.id;
        tokio::spawn(async move {
            this.run_proposal(plan_id, proposer, prompt, candidates)
                .await;
        });
        Ok(plan)
    }

    async fn run_proposal(
        self: Arc<Self>,
        plan_id: PlanId,
        proposer: SessionId,
        prompt: String,
        candidates: Vec<SessionId>,
    ) {
        let before = self.last_seq_of(proposer);
        let outcome = self.prompt(proposer, prompt, vec![]).await;
        self.settle_events(proposer).await;
        let (reply, _) = self.turn_output(proposer, before);
        let _ = self.mutate_plan(plan_id, |p| {
            if p.state != PlanState::Proposing {
                return Ok(()); // cancelled meanwhile
            }
            let parsed = match outcome {
                Ok(()) => plan::parse_proposal(&reply, &candidates),
                Err(e) => Err(anyhow!("the proposer's turn failed: {e:#}")),
            };
            match parsed {
                Ok(tasks) => {
                    p.warnings = plan::scope_warnings(&tasks);
                    p.tasks = tasks;
                    p.state = PlanState::Proposed;
                    p.error = None;
                }
                Err(e) => {
                    p.state = PlanState::Failed;
                    p.error = Some(format!("{e:#}"));
                    p.raw_reply = Some(plan::excerpt(&reply, 4_000));
                }
            }
            Ok(())
        });
    }

    /// Replace a `Proposed` plan's tasks with the user's edited version.
    pub fn plan_edit(&self, plan_id: PlanId, specs: Vec<PlanTaskSpec>) -> Result<Plan> {
        let workspace_id = self.plan_get(plan_id)?.workspace_id;
        let tasks: Vec<PlanTask> = specs.into_iter().map(Into::into).collect();
        plan::validate_tasks(&tasks)?;
        for t in &tasks {
            if let Some(a) = t.assignee {
                self.check_assignee(workspace_id, a)?;
            }
        }
        let (_, plan) = self.mutate_plan(plan_id, |p| {
            ensure!(
                p.state == PlanState::Proposed,
                "only a plan awaiting approval can be edited (state {:?})",
                p.state
            );
            p.warnings = plan::scope_warnings(&tasks);
            p.tasks = tasks;
            Ok(())
        })?;
        Ok(plan)
    }

    /// Approve a reviewed plan and start dispatching.
    pub fn plan_approve(self: &Arc<Self>, plan_id: PlanId) -> Result<Plan> {
        let snapshot = self.plan_get(plan_id)?;
        ensure!(
            snapshot.state == PlanState::Proposed,
            "plan is not awaiting approval (state {:?})",
            snapshot.state
        );
        plan::validate_tasks(&snapshot.tasks)?;
        for t in &snapshot.tasks {
            let a = t
                .assignee
                .with_context(|| format!("task {} has no assignee", t.id))?;
            self.check_assignee(snapshot.workspace_id, a)?;
        }
        let (_, plan) = self.mutate_plan(plan_id, |p| {
            ensure!(
                p.state == PlanState::Proposed,
                "plan changed while approving"
            );
            p.state = PlanState::Running;
            p.error = None;
            Ok(())
        })?;
        self.advance(plan_id);
        Ok(self.plan_get(plan_id).unwrap_or(plan))
    }

    // ----- running -----------------------------------------------------------

    /// Dispatch whatever is ready and settle the plan's state.
    fn advance(self: &Arc<Self>, plan_id: PlanId) {
        let result = self.mutate_plan(plan_id, |p| {
            if p.state != PlanState::Running {
                return Ok(Vec::new());
            }
            let ids = plan::dispatchable(p);
            for id in &ids {
                if let Some(t) = p.task_mut(id) {
                    t.state = TaskState::Running;
                    t.dispatched = true;
                    t.error = None;
                }
            }
            plan::settle(p);
            if p.state == PlanState::Done && p.summary.is_none() {
                p.summary = Some(plan::render_summary(p, &|s| self.session_label(s)));
            }
            Ok(ids)
        });
        let Ok((ids, _)) = result else { return };
        for task_id in ids {
            let this = self.clone();
            tokio::spawn(async move { this.run_task(plan_id, task_id).await });
        }
    }

    fn task_prompt(&self, plan: &Plan, task: &PlanTask) -> String {
        let others: Vec<String> = plan
            .tasks
            .iter()
            .filter(|t| t.id != task.id)
            .map(|t| {
                let who = t
                    .assignee
                    .map(|s| self.session_label(s))
                    .unwrap_or_default();
                format!("- {} {} ({who})", t.id, t.title)
            })
            .collect();
        let mut out = format!(
            "[agentmux plan] You are executing task {} \"{}\" of a plan shared with other agents \
             in this worktree.\nGoal: {}\n",
            task.id, task.title, plan.goal
        );
        if !task.scope_paths.is_empty() {
            out.push_str(&format!(
                "Stay within these paths: {}\n",
                task.scope_paths.join(", ")
            ));
        }
        if !others.is_empty() {
            out.push_str(&format!("Other tasks:\n{}\n", others.join("\n")));
        }
        out.push_str("Progress is tracked in .agentmux/plan.md (read-only for you).\n\n");
        out.push_str(&task.prompt);
        out
    }

    async fn run_task(self: Arc<Self>, plan_id: PlanId, task_id: String) {
        let Ok(plan) = self.plan_get(plan_id) else {
            return;
        };
        let Some(task) = plan.task(&task_id).cloned() else {
            return;
        };
        let Some(assignee) = task.assignee else {
            return;
        };
        let text = self.task_prompt(&plan, &task);
        let before = self.last_seq_of(assignee);
        let outcome = self.prompt(assignee, text, vec![]).await;
        self.settle_events(assignee).await;
        let (reply, files) = self.turn_output(assignee, before);
        self.finish_task(
            plan_id,
            &task_id,
            outcome.map_err(|e| format!("{e:#}")),
            reply,
            files,
        );
        self.advance(plan_id);
    }

    fn finish_task(
        &self,
        plan_id: PlanId,
        task_id: &str,
        outcome: std::result::Result<(), String>,
        reply: String,
        files: Vec<String>,
    ) {
        let _ = self.mutate_plan(plan_id, |p| {
            let Some(t) = p.task_mut(task_id) else {
                return Ok(());
            };
            if t.state != TaskState::Running {
                return Ok(()); // cancelled or reset by the user meanwhile
            }
            match outcome {
                Ok(()) => {
                    t.state = TaskState::Done;
                    t.result = Some(plan::excerpt(&reply, MAX_RESULT));
                    t.files_changed = files;
                    t.error = None;
                }
                Err(e) => {
                    t.state = TaskState::Failed;
                    t.error = Some(e);
                }
            }
            plan::settle(p);
            if p.state == PlanState::Done && p.summary.is_none() {
                p.summary = Some(plan::render_summary(p, &|s| self.session_label(s)));
            }
            Ok(())
        });
    }

    /// Retry / skip / reassign one task, then continue the plan.
    pub fn plan_task_action(
        self: &Arc<Self>,
        plan_id: PlanId,
        task_id: &str,
        action: TaskAction,
    ) -> Result<Plan> {
        let workspace_id = self.plan_get(plan_id)?.workspace_id;
        if let TaskAction::Reassign { session_id } = &action {
            self.check_assignee(workspace_id, *session_id)?;
        }
        let (_, plan) = self.mutate_plan(plan_id, |p| {
            ensure!(
                matches!(p.state, PlanState::Running | PlanState::Paused),
                "plan is not active (state {:?})",
                p.state
            );
            let t = p
                .task_mut(task_id)
                .ok_or_else(|| anyhow!("no such task {task_id}"))?;
            ensure!(
                t.state != TaskState::Running,
                "task {task_id} is running; cancel the plan to stop it"
            );
            match &action {
                TaskAction::Retry => {
                    ensure!(
                        matches!(t.state, TaskState::Failed | TaskState::Skipped),
                        "only failed or skipped tasks can be retried"
                    );
                    reset_task(t);
                }
                TaskAction::Skip => {
                    ensure!(t.state != TaskState::Done, "task {task_id} is already done");
                    t.state = TaskState::Skipped;
                    t.error = None;
                }
                TaskAction::Reassign { session_id } => {
                    ensure!(t.state != TaskState::Done, "task {task_id} is already done");
                    t.assignee = Some(*session_id);
                    if t.state == TaskState::Failed {
                        reset_task(t);
                    }
                }
            }
            p.state = PlanState::Running;
            p.error = None;
            Ok(())
        })?;
        self.advance(plan_id);
        Ok(self.plan_get(plan_id).unwrap_or(plan))
    }

    /// Continue a `Paused` plan (after a restart, or once blockers are fixed).
    pub fn plan_resume(self: &Arc<Self>, plan_id: PlanId) -> Result<Plan> {
        let (_, plan) = self.mutate_plan(plan_id, |p| {
            ensure!(
                p.state == PlanState::Paused,
                "plan is not paused (state {:?})",
                p.state
            );
            p.state = PlanState::Running;
            p.error = None;
            Ok(())
        })?;
        self.advance(plan_id);
        Ok(self.plan_get(plan_id).unwrap_or(plan))
    }

    /// Stop the plan: running tasks are cancelled, the rest skipped.
    pub async fn plan_cancel(&self, plan_id: PlanId) -> Result<Plan> {
        let (running, plan) = self.mutate_plan(plan_id, |p| {
            ensure!(
                !p.state.is_terminal(),
                "plan already ended (state {:?})",
                p.state
            );
            let mut running = Vec::new();
            for t in &mut p.tasks {
                match t.state {
                    TaskState::Running => {
                        t.state = TaskState::Failed;
                        t.error = Some("cancelled".into());
                        running.extend(t.assignee);
                    }
                    TaskState::Pending | TaskState::Ready => t.state = TaskState::Skipped,
                    _ => {}
                }
            }
            p.state = PlanState::Cancelled;
            p.error = None;
            Ok(running)
        })?;
        for session in running {
            let _ = self.cancel(session).await;
        }
        Ok(plan)
    }
}

fn reset_task(t: &mut PlanTask) {
    t.state = TaskState::Pending;
    t.dispatched = false;
    t.error = None;
    t.result = None;
    t.files_changed.clear();
}
