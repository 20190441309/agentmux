import { useState } from "react";
import {
  planApprove, planCancel, planEdit, planPropose, planReassign, planResume, planTaskAction,
  sessionLabel, useStore, getState,
} from "../store";
import type { Plan, Session } from "../types";

const NONE: never[] = [];

const STATE_TEXT: Record<Plan["state"], string> = {
  proposing: "Waiting for the proposal…", proposed: "Review, then approve — nothing runs before you do.",
  running: "Running", paused: "Paused — needs your decision", done: "Done", cancelled: "Cancelled", failed: "Proposal failed",
};

function PlanCard({ plan, workspaceId }: { plan: Plan; workspaceId: string }) {
  const sessions = useStore((s) => s.sessions[workspaceId] ?? (NONE as Session[]));
  const label = (sid: string | null) => (sid ? sessionLabel(getState(), sid) : "unassigned");
  const editable = plan.state === "proposed";
  return (
    <div className={`card plan ${plan.state}`}>
      <div className="title">{plan.goal}</div>
      <div className="muted">{STATE_TEXT[plan.state]}</div>
      {plan.error && <div className="note error">{plan.error}</div>}
      {plan.warnings.map((w) => <div key={w} className="note warn">{w}</div>)}
      <ol className="tasks">
        {plan.tasks.map((t) => (
          <li key={t.id} className={`task ${t.state}`}>
            <div><span className={`pill ${t.state}`}>{t.state}</span> <strong>{t.title}</strong></div>
            <div className="muted">{t.prompt}</div>
            {t.depends_on.length > 0 && <div className="muted">after {t.depends_on.join(", ")}</div>}
            <div className="row">
              {editable || plan.state === "paused" ? (
                <select aria-label={`Assignee for ${t.title}`} value={t.assignee ?? ""}
                  onChange={(e) => (editable ? planEdit(plan, { taskId: t.id, sessionId: e.target.value }) : planReassign(plan.id, t.id, e.target.value))}>
                  <option value="" disabled>unassigned</option>
                  {sessions.map((s) => <option key={s.id} value={s.id}>{label(s.id)}</option>)}
                </select>
              ) : <span className="muted">{label(t.assignee)}</span>}
              {plan.state === "paused" && (t.state === "failed" || t.state === "skipped") && <button onClick={() => planTaskAction(plan.id, t.id, "retry")}>Retry</button>}
              {plan.state === "paused" && t.state === "failed" && <button onClick={() => planTaskAction(plan.id, t.id, "skip")}>Skip</button>}
            </div>
            {t.error && <div className="note error">{t.error}</div>}
            {t.result && <details><summary className="muted">Result</summary><pre>{t.result}</pre></details>}
          </li>
        ))}
      </ol>
      {plan.summary && <pre className="summary">{plan.summary}</pre>}
      <div className="actions">
        {plan.state === "proposed" && plan.tasks.length > 0 && <button className="primary" onClick={() => planApprove(plan.id)}>Approve</button>}
        {plan.state === "paused" && <button className="primary" onClick={() => planResume(plan.id)}>Resume</button>}
        {(plan.state === "proposed" || plan.state === "running" || plan.state === "paused") && <button className="danger" onClick={() => planCancel(plan.id)}>Cancel</button>}
      </div>
    </div>
  );
}

export function PlansView() {
  const sid = useStore((s) => s.selected);
  const wsId = useStore((s) => Object.entries(s.sessions).find(([, l]) => l.some((x) => x.id === s.selected))?.[0]);
  const plans = useStore((s) => (wsId ? s.plans[wsId] ?? (NONE as Plan[]) : (NONE as Plan[])));
  const [goal, setGoal] = useState("");
  if (!sid || !wsId) return <div className="empty">Select a session first — plans belong to its workspace, and that session proposes the split.</div>;
  return (
    <div className="plansview">
      <form className="propose" onSubmit={(e) => { e.preventDefault(); if (goal.trim()) { void planPropose(wsId, sid, goal.trim()); setGoal(""); } }}>
        <input value={goal} onChange={(e) => setGoal(e.target.value)} placeholder="Describe a goal to split across agents" aria-label="Goal" />
        <button className="primary" disabled={!goal.trim()}>Propose plan</button>
      </form>
      {plans.length === 0 && <div className="empty">No delegation plans yet.</div>}
      {plans.map((p) => <PlanCard key={p.id} plan={p} workspaceId={wsId} />)}
    </div>
  );
}
