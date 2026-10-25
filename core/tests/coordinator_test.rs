//! Coordinator end-to-end: propose → review → approve → run → recover,
//! against the real mock agent (`MOCK_PLAN=1` answers plan requests).

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentmux_core::orchestrator::Orchestrator;
use agentmux_core::plan::{Plan, PlanState, PlanTask, PlanTaskSpec, TaskAction, TaskState};
use agentmux_core::{
    AdapterKind, AgentId, AgentProfile, AgentRegistry, Config, EventKind, PlanId, Project,
    ProjectId, SessionId, Store, WorkspaceId,
};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
}

fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.email", "t@example.com"]);
    git(repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "# test\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "init"]);
    dir
}

struct Env {
    _repo: TempDir,
    _data: TempDir,
    orch: Arc<Orchestrator>,
    ws: WorkspaceId,
}

async fn setup() -> Env {
    let repo = init_repo();
    let data = tempfile::tempdir().unwrap();
    let store = Store::open(data.path()).unwrap();
    let project_id = ProjectId::new();
    store
        .insert_project(&Project {
            id: project_id,
            root_path: repo.path().to_path_buf(),
            name: "p".into(),
        })
        .unwrap();
    let profile = AgentProfile {
        id: AgentId::new("mock"),
        name: "Mock".into(),
        adapter: AdapterKind::Acp {
            command: common::mock_agent_binary(),
            args: vec![],
        },
        env: BTreeMap::from([("MOCK_PLAN".to_string(), "1".to_string())]),
        available: true,
    };
    let cfg = Config {
        agents: vec![profile],
        ..Config::default()
    };
    let orch = Arc::new(Orchestrator::new(
        store,
        AgentRegistry::from_config(&cfg),
        data.path().to_path_buf(),
    ));
    let ws = orch
        .create_workspace(project_id, "plan", "main")
        .await
        .unwrap();
    Env {
        _repo: repo,
        _data: data,
        orch,
        ws,
    }
}

async fn session(env: &Env) -> SessionId {
    env.orch
        .create_session(env.ws, &AgentId::new("mock"), None)
        .await
        .unwrap()
}

async fn wait_plan(
    orch: &Orchestrator,
    id: PlanId,
    what: &str,
    pred: impl Fn(&Plan) -> bool,
) -> Plan {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let plan = orch.plan_get(id).unwrap();
        if pred(&plan) {
            return plan;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {plan:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn propose_edit_approve_run_to_done_with_blackboard_and_events() {
    let env = setup().await;
    let a = session(&env).await;
    let mut bus = env.orch.subscribe();

    let plan = env.orch.plan_propose(env.ws, a, "ship it".into()).unwrap();
    assert_eq!(plan.state, PlanState::Proposing);
    let plan = wait_plan(&env.orch, plan.id, "proposal", |p| {
        p.state == PlanState::Proposed
    })
    .await;
    assert_eq!(plan.tasks.len(), 2);
    assert_eq!(plan.tasks[0].assignee, Some(a));
    assert_eq!(plan.tasks[1].depends_on, vec!["t1"]);

    // The user edits a title during review.
    let specs: Vec<PlanTaskSpec> = plan
        .tasks
        .iter()
        .map(|t| PlanTaskSpec {
            id: t.id.clone(),
            title: if t.id == "t1" {
                "renamed".into()
            } else {
                t.title.clone()
            },
            prompt: t.prompt.clone(),
            assignee: t.assignee,
            depends_on: t.depends_on.clone(),
            scope_paths: t.scope_paths.clone(),
        })
        .collect();
    let edited = env.orch.plan_edit(plan.id, specs).unwrap();
    assert_eq!(edited.tasks[0].title, "renamed");

    env.orch.plan_approve(plan.id).unwrap();
    let done = wait_plan(&env.orch, plan.id, "completion", |p| {
        p.state == PlanState::Done
    })
    .await;
    assert!(done
        .tasks
        .iter()
        .all(|t| t.state == TaskState::Done && t.dispatched));
    assert!(done.tasks[0]
        .result
        .as_deref()
        .unwrap()
        .contains("do the first part"));
    assert!(done.summary.as_deref().unwrap().contains("renamed"));

    let ws = env.orch.get_workspace(env.ws).unwrap().unwrap();
    let md = std::fs::read_to_string(ws.worktree_path.join(".agentmux/plan.md")).unwrap();
    assert!(md.contains("# Plan: ship it") && md.contains("- [x] **t1**"));

    // Events were persisted on the proposer and broadcast.
    assert!(env
        .orch
        .read_events(a)
        .unwrap()
        .iter()
        .any(|e| matches!(&e.kind, EventKind::PlanUpdated(p) if p.state == PlanState::Done)));
    let mut saw = false;
    while let Ok(e) = bus.try_recv() {
        saw |= matches!(e.kind, EventKind::PlanUpdated(_));
    }
    assert!(saw);
    // The second task ran strictly after the first (same session, dependency).
    let events = env.orch.read_events(a).unwrap();
    let text = serde_json::to_string(&events).unwrap();
    assert!(text.find("do the first part").unwrap() < text.find("do the second part").unwrap());
}

#[tokio::test]
async fn validation_blocks_bad_edits_and_unassigned_approval() {
    let env = setup().await;
    let a = session(&env).await;
    let plan = env.orch.plan_propose(env.ws, a, "goal".into()).unwrap();
    let plan = wait_plan(&env.orch, plan.id, "proposal", |p| {
        p.state == PlanState::Proposed
    })
    .await;

    let spec = |id: &str, deps: &[&str], who: Option<SessionId>| PlanTaskSpec {
        id: id.into(),
        title: id.into(),
        prompt: "p".into(),
        assignee: who,
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
        scope_paths: vec![],
    };
    // Cycle, unknown assignee.
    assert!(env
        .orch
        .plan_edit(
            plan.id,
            vec![spec("x", &["y"], Some(a)), spec("y", &["x"], Some(a))]
        )
        .is_err());
    assert!(env
        .orch
        .plan_edit(plan.id, vec![spec("x", &[], Some(SessionId::new()))])
        .is_err());
    // A valid edit that leaves a task unassigned cannot be approved.
    env.orch
        .plan_edit(plan.id, vec![spec("x", &[], None)])
        .unwrap();
    let err = env.orch.plan_approve(plan.id).unwrap_err();
    assert!(format!("{err:#}").contains("no assignee"), "{err:#}");
    // Once approved the plan is no longer editable.
    env.orch
        .plan_edit(plan.id, vec![spec("x", &[], Some(a))])
        .unwrap();
    env.orch.plan_approve(plan.id).unwrap();
    assert!(env
        .orch
        .plan_edit(plan.id, vec![spec("x", &[], Some(a))])
        .is_err());
}

#[tokio::test]
async fn a_failed_task_pauses_the_plan_and_reassign_finishes_it() {
    let env = setup().await;
    let a = session(&env).await;
    let b = session(&env).await;
    env.orch.kill(b).await.unwrap(); // b can no longer take prompts

    let plan = env.orch.plan_propose(env.ws, a, "goal".into()).unwrap();
    let plan = wait_plan(&env.orch, plan.id, "proposal", |p| {
        p.state == PlanState::Proposed
    })
    .await;
    let specs: Vec<PlanTaskSpec> = plan
        .tasks
        .iter()
        .map(|t| PlanTaskSpec {
            id: t.id.clone(),
            title: t.title.clone(),
            prompt: t.prompt.clone(),
            assignee: Some(if t.id == "t1" { b } else { a }),
            depends_on: t.depends_on.clone(),
            scope_paths: vec![],
        })
        .collect();
    env.orch.plan_edit(plan.id, specs).unwrap();
    env.orch.plan_approve(plan.id).unwrap();

    let paused = wait_plan(&env.orch, plan.id, "pause", |p| {
        p.state == PlanState::Paused
    })
    .await;
    assert_eq!(paused.task("t1").unwrap().state, TaskState::Failed);
    assert_eq!(
        paused.task("t2").unwrap().state,
        TaskState::Pending,
        "blocked, not run"
    );
    assert!(paused.error.as_deref().unwrap().contains("t1"));

    // Only failed/skipped tasks may be retried; a pending one may not.
    assert!(env
        .orch
        .plan_task_action(plan.id, "t2", TaskAction::Retry)
        .is_err());
    env.orch
        .plan_task_action(plan.id, "t1", TaskAction::Reassign { session_id: a })
        .unwrap();
    let done = wait_plan(&env.orch, plan.id, "completion", |p| {
        p.state == PlanState::Done
    })
    .await;
    assert_eq!(done.task("t1").unwrap().assignee, Some(a));
}

#[tokio::test]
async fn skipping_a_failed_task_unblocks_dependents_and_cancel_ends_the_plan() {
    let env = setup().await;
    let a = session(&env).await;
    let b = session(&env).await;
    env.orch.kill(b).await.unwrap();

    let plan = env.orch.plan_propose(env.ws, a, "goal".into()).unwrap();
    let plan = wait_plan(&env.orch, plan.id, "proposal", |p| {
        p.state == PlanState::Proposed
    })
    .await;
    let specs: Vec<PlanTaskSpec> = plan
        .tasks
        .iter()
        .map(|t| PlanTaskSpec {
            id: t.id.clone(),
            title: t.title.clone(),
            prompt: t.prompt.clone(),
            assignee: Some(if t.id == "t1" { b } else { a }),
            depends_on: t.depends_on.clone(),
            scope_paths: vec![],
        })
        .collect();
    env.orch.plan_edit(plan.id, specs).unwrap();
    env.orch.plan_approve(plan.id).unwrap();
    wait_plan(&env.orch, plan.id, "pause", |p| {
        p.state == PlanState::Paused
    })
    .await;

    env.orch
        .plan_task_action(plan.id, "t1", TaskAction::Skip)
        .unwrap();
    let done = wait_plan(&env.orch, plan.id, "completion", |p| {
        p.state == PlanState::Done
    })
    .await;
    assert_eq!(done.task("t1").unwrap().state, TaskState::Skipped);
    assert_eq!(done.task("t2").unwrap().state, TaskState::Done);
    assert!(
        env.orch.plan_cancel(plan.id).await.is_err(),
        "ended plans cannot be cancelled"
    );

    // Cancelling a plan awaiting approval ends it.
    let p2 = env.orch.plan_propose(env.ws, a, "again".into()).unwrap();
    wait_plan(&env.orch, p2.id, "proposal", |p| {
        p.state == PlanState::Proposed
    })
    .await;
    let cancelled = env.orch.plan_cancel(p2.id).await.unwrap();
    assert_eq!(cancelled.state, PlanState::Cancelled);
    assert!(env.orch.plan_approve(p2.id).is_err());
}

#[tokio::test]
async fn propose_rejects_busy_native_foreign_and_empty_requests() {
    let env = setup().await;
    let a = session(&env).await;
    assert!(env.orch.plan_propose(env.ws, a, "   ".into()).is_err());
    assert!(env
        .orch
        .plan_propose(env.ws, SessionId::new(), "x".into())
        .is_err());
    assert!(env
        .orch
        .plan_propose(WorkspaceId::new(), a, "x".into())
        .is_err());
}

#[tokio::test]
async fn restart_pauses_running_plans_and_fails_proposals() {
    let data = tempfile::tempdir().unwrap();
    let ws = WorkspaceId::new();
    let proposer = SessionId::new();
    {
        let store = Store::open(data.path()).unwrap();
        let mut running = Plan::new(ws, proposer, "running".into());
        running.state = PlanState::Running;
        let mut t1 = PlanTask::new("t1", "t", "p");
        t1.state = TaskState::Running;
        t1.assignee = Some(proposer);
        running.tasks = vec![t1, PlanTask::new("t2", "t", "p")];
        store.upsert_plan(&running).unwrap();
        let proposing = Plan::new(ws, proposer, "proposing".into());
        store.upsert_plan(&proposing).unwrap();
    }
    let store = Store::open(data.path()).unwrap();
    let orch = Orchestrator::new(
        store,
        AgentRegistry::from_config(&Config::default()),
        data.path().to_path_buf(),
    );
    let plans = orch.plan_list(ws);
    assert_eq!(plans.len(), 2);
    let running = plans.iter().find(|p| p.goal == "running").unwrap();
    assert_eq!(running.state, PlanState::Paused);
    assert_eq!(running.task("t1").unwrap().state, TaskState::Failed);
    assert_eq!(running.task("t2").unwrap().state, TaskState::Pending);
    let proposing = plans.iter().find(|p| p.goal == "proposing").unwrap();
    assert_eq!(proposing.state, PlanState::Failed);
}
