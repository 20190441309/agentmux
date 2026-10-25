// Hand-written mirrors of the daemon's JSON shapes (core/src/model.rs,
// plan.rs, rpc.rs). Generated types (ts-rs) are planned; keep these small.

export type Id = string;

export interface Project { id: Id; root_path: string; name: string }
export interface Workspace { id: Id; project_id: Id; name: string; worktree_path: string; branch: string }
export type SessionState =
  | "Created" | "Connecting" | "Ready" | "Prompting" | "WaitingPermission" | "Done"
  | { Error: string };
export interface Session {
  id: Id; workspace_id: Id; agent_id: string; state: SessionState;
  native_terminal?: boolean; created_at: string;
}
export interface AgentProfile { id: string; name: string; available: boolean; adapter: Record<string, unknown> }

export type EventKind =
  | { TitleChanged: { title: string } }
  | "ConversationStarted"
  | { SessionUpdate: any }
  | { StateChanged: { from: SessionState; to: SessionState } }
  | { FileEdited: { path: string } }
  | { AgentExited: { code: number | null } }
  | { PermissionRequest: { request_id: string; request: any } }
  | { PermissionResolved: { request_id: string; outcome: string } }
  | { Orchestrator: string }
  | { FileOverlap: { path: string; others: Id[] } }
  | { CapabilitiesUpdated: any }
  | { PlanUpdated: Plan };

export interface SessionEvent { session_id: Id; seq: number; ts: string; kind: EventKind }

export interface HistoryPage {
  conversation_start: number;
  events: SessionEvent[];
  has_more: boolean;
  title: string | null;
  pending_permissions: SessionEvent[];
}

export type PlanState = "proposing" | "proposed" | "running" | "paused" | "done" | "cancelled" | "failed";
export type TaskState = "pending" | "ready" | "running" | "done" | "failed" | "skipped";
export interface PlanTask {
  id: string; title: string; prompt: string; assignee: Id | null; depends_on: string[];
  scope_paths: string[]; state: TaskState; dispatched: boolean; result: string | null;
  error: string | null; files_changed: string[];
}
export interface Plan {
  id: Id; workspace_id: Id; goal: string; proposer: Id; state: PlanState; tasks: PlanTask[];
  error: string | null; raw_reply: string | null; warnings: string[]; summary: string | null;
  created_at: string; updated_at: string;
}

export type PermissionDecision = "allow_once" | "allow_always" | "reject" | "cancel";

export function stateName(s: SessionState): string {
  return typeof s === "string" ? s : "Error";
}
export function stateError(s: SessionState): string | null {
  return typeof s === "string" ? null : s.Error;
}
