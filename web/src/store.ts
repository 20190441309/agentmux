import { useSyncExternalStore } from "react";
import {
  callOnFreshConnection, ConnStatus, pairWithCode, RpcClient, tokenStore,
} from "./rpc";
import { mergeEvents, sessionTitle } from "./transcript";
import type {
  AgentProfile, HistoryPage, PermissionDecision, Plan, Project, Session, SessionEvent, Workspace,
} from "./types";

export type Tab = "sessions" | "chat" | "inbox" | "plans";

export interface State {
  status: ConnStatus | "signed-out";
  projects: Project[];
  workspaces: Record<string, Workspace[]>;
  sessions: Record<string, Session[]>;
  agents: AgentProfile[];
  events: Record<string, SessionEvent[]>;
  hasMore: Record<string, boolean>;
  titles: Record<string, string>;
  plans: Record<string, Plan[]>;
  selected: string | null;
  tab: Tab;
  error: string | null;
}

const initial: State = {
  status: "signed-out", projects: [], workspaces: {}, sessions: {}, agents: [], events: {},
  hasMore: {}, titles: {}, plans: {}, selected: null, tab: "sessions", error: null,
};

let state: State = initial;
const listeners = new Set<() => void>();
let client: RpcClient | null = null;
let token: string | null = null;

function set(patch: Partial<State> | ((s: State) => Partial<State>)) {
  state = { ...state, ...(typeof patch === "function" ? patch(state) : patch) };
  listeners.forEach((l) => l());
}

export function useStore<T>(select: (s: State) => T): T {
  return useSyncExternalStore(
    (l) => { listeners.add(l); return () => listeners.delete(l); },
    () => select(state),
  );
}
export const getState = () => state;

// ---- connection ---------------------------------------------------------

export async function signIn(code: string, deviceName: string) {
  const t = await pairWithCode(code, deviceName);
  tokenStore.set(t);
  await start();
}

export function signOut() {
  client?.close();
  client = null;
  tokenStore.clear();
  token = null;
  set({ ...initial });
}

export async function start() {
  token = tokenStore.get();
  if (!token) { set({ status: "signed-out" }); return; }
  client?.close();
  let first = true;
  client = new RpcClient(token, onNotify, (status) => {
    if (status === "unauthorized") { tokenStore.clear(); set({ ...initial }); return; }
    set({ status });
    // After a drop the event bus may have skipped events: resync.
    if (status === "open" && !first) void resync();
    if (status === "open") first = false;
  });
  try {
    await client.connect();
  } catch { return; }
  await client.call("session/subscribe");
  await refreshAll();
}

const rpc = <T = any>(method: string, params: unknown = null) => {
  if (!client) return Promise.reject(new Error("not connected"));
  return client.call<T>(method, params);
};

function fail(e: unknown) {
  set({ error: e instanceof Error ? e.message : String(e) });
}
export const clearError = () => set({ error: null });

// ---- data ---------------------------------------------------------------

export async function refreshAll() {
  try {
    const [{ projects }, { agents }] = await Promise.all([rpc("project/list"), rpc("agent/list")]);
    const workspaces: Record<string, Workspace[]> = {};
    const sessions: Record<string, Session[]> = {};
    const plans: Record<string, Plan[]> = {};
    await Promise.all((projects as Project[]).map(async (p) => {
      const { workspaces: ws } = await rpc("workspace/list", { project_id: p.id });
      workspaces[p.id] = ws;
      await Promise.all((ws as Workspace[]).map(async (w) => {
        const [s, pl] = await Promise.all([
          rpc("session/list", { workspace_id: w.id }),
          rpc("plan/list", { workspace_id: w.id }).catch(() => ({ plans: [] })),
        ]);
        sessions[w.id] = s.sessions;
        plans[w.id] = pl.plans;
      }));
    }));
    set({ projects, agents, workspaces, sessions, plans });
    // Pending permission requests must be visible in the inbox without opening each chat.
    // History also carries titles, which the session list needs; load it in the background.
    void (async () => {
      for (const list of Object.values(sessions)) {
        for (const s of list) {
          if (!s.native_terminal && !state.events[s.id]) await loadHistory(s.id);
        }
      }
    })();
    if (state.selected) void loadHistory(state.selected);
  } catch (e) { fail(e); }
}

async function resync() {
  await refreshAll();
}

let refreshTimer: ReturnType<typeof setTimeout> | undefined;
const refreshSoon = () => {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => void refreshAll(), 300);
};

export async function loadHistory(sid: string, before?: number) {
  try {
    const page = await rpc<HistoryPage>("session/history", { session_id: sid, before_seq: before ?? null });
    set((s) => ({
      events: { ...s.events, [sid]: mergeEvents(s.events[sid] ?? [], [...page.events, ...page.pending_permissions]) },
      hasMore: { ...s.hasMore, [sid]: page.has_more },
      titles: page.title ? { ...s.titles, [sid]: page.title } : s.titles,
    }));
  } catch (e) { fail(e); }
}

export function loadOlder(sid: string) {
  const first = state.events[sid]?.[0];
  if (first) void loadHistory(sid, first.seq);
}

function onNotify(method: string, params: any) {
  if (method !== "session/event") return;
  const ev = params as SessionEvent;
  const sid = ev.session_id;
  if (!sid || sid === "00000000-0000-0000-0000-000000000000") return;
  const known = Object.values(state.sessions).some((l) => l.some((s) => s.id === sid));
  set((s) => {
    const patch: Partial<State> = { events: { ...s.events, [sid]: mergeEvents(s.events[sid] ?? [], [ev]) } };
    const k = ev.kind;
    if (typeof k === "object") {
      if ("StateChanged" in k) {
        patch.sessions = Object.fromEntries(Object.entries(s.sessions).map(([w, l]) =>
          [w, l.map((x) => (x.id === sid ? { ...x, state: k.StateChanged.to } : x))]));
      } else if ("TitleChanged" in k) {
        patch.titles = { ...s.titles, [sid]: k.TitleChanged.title };
      } else if ("PlanUpdated" in k) {
        const plan = k.PlanUpdated;
        const list = s.plans[plan.workspace_id] ?? [];
        patch.plans = {
          ...s.plans,
          [plan.workspace_id]: list.some((p) => p.id === plan.id)
            ? list.map((p) => (p.id === plan.id ? plan : p)) : [plan, ...list],
        };
      }
    }
    return patch;
  });
  if (!known) refreshSoon();
}

// ---- navigation ---------------------------------------------------------

export function select(sid: string) {
  set({ selected: sid, tab: "chat" });
  if (!state.events[sid] || state.events[sid].length === 0) void loadHistory(sid);
}
export const setTab = (tab: Tab) => set({ tab });

// ---- actions ------------------------------------------------------------

export function sendPrompt(sid: string, text: string) {
  if (!token) return;
  callOnFreshConnection(token, "session/prompt", { session_id: sid, text, references: [] })
    .catch(fail);
}
export const cancelTurn = (sid: string) => rpc("session/cancel", { session_id: sid }).catch(fail);

export async function decide(sid: string, requestId: string, outcome: PermissionDecision) {
  try { await rpc("session/permission", { session_id: sid, request_id: requestId, outcome }); } catch (e) { fail(e); }
}

export async function createSession(workspaceId: string, agentId: string) {
  try {
    const { session } = await rpc("session/create", { workspace_id: workspaceId, agent_id: agentId });
    await refreshAll();
    select(session.id);
  } catch (e) { fail(e); }
}

export async function renameSession(sid: string, title: string) {
  try { await rpc("session/title", { session_id: sid, title }); } catch (e) { fail(e); }
}

const planCall = async (method: string, params: unknown) => {
  try {
    const { plan } = await rpc<{ plan: Plan }>(method, params);
    set((s) => {
      const list = s.plans[plan.workspace_id] ?? [];
      return { plans: { ...s.plans, [plan.workspace_id]: list.some((p) => p.id === plan.id)
        ? list.map((p) => (p.id === plan.id ? plan : p)) : [plan, ...list] } };
    });
  } catch (e) { fail(e); }
};
export const planPropose = (workspace_id: string, session_id: string, goal: string) =>
  planCall("plan/propose", { workspace_id, session_id, goal });
export const planApprove = (plan_id: string) => planCall("plan/approve", { plan_id });
export const planCancel = (plan_id: string) => planCall("plan/cancel", { plan_id });
export const planResume = (plan_id: string) => planCall("plan/resume", { plan_id });
export const planTaskAction = (plan_id: string, task_id: string, action: "retry" | "skip") =>
  planCall("plan/task_action", { plan_id, task_id, action });
export const planReassign = (plan_id: string, task_id: string, session_id: string) =>
  planCall("plan/task_action", { plan_id, task_id, action: "reassign", session_id });
export const planEdit = (plan: Plan, assign: { taskId: string; sessionId: string }) =>
  planCall("plan/edit", {
    plan_id: plan.id,
    tasks: plan.tasks.map((t) => ({
      id: t.id, title: t.title, prompt: t.prompt, depends_on: t.depends_on, scope_paths: t.scope_paths,
      assignee: t.id === assign.taskId ? assign.sessionId : t.assignee,
    })),
  });

// ---- helpers for views ---------------------------------------------------

export function allSessions(s: State): Session[] {
  return Object.values(s.sessions).flat();
}
export function sessionLabel(s: State, sid: string): string {
  const sess = allSessions(s).find((x) => x.id === sid);
  const title = s.titles[sid] ?? sessionTitle(s.events[sid] ?? []);
  if (title) return title;
  const agent = s.agents.find((a) => a.id === sess?.agent_id)?.name ?? sess?.agent_id ?? "session";
  return `${agent} · ${sid.slice(0, 4)}`;
}
