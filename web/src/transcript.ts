import type { PermissionDecision, SessionEvent } from "./types";

// Turn the raw per-session event log into display items. Pure functions so
// they can be tested without a browser.

export type Item =
  | { type: "msg"; role: "user" | "agent" | "thought"; text: string; seq: number; ts: string }
  | { type: "tool"; id: string; title: string; kind: string; status: string; detail: string; seq: number; ts: string }
  | { type: "edit"; path: string; seq: number; ts: string }
  | { type: "note"; text: string; tone: "info" | "warn" | "error"; seq: number; ts: string };

export interface PermissionOption { optionId: string; name: string; kind: string }
export interface PendingPermission {
  requestId: string; seq: number; ts: string; title: string; detail: string; options: PermissionOption[];
}

const text = (u: any): string => u?.content?.text ?? u?.text ?? "";

export function buildItems(events: SessionEvent[]): Item[] {
  const items: Item[] = [];
  const tools = new Map<string, number>(); // toolCallId -> index in items
  // Only the latest conversation is shown (resume with a fresh adapter session resets it).
  let start = 0;
  events.forEach((e, i) => {
    if (e.kind === "ConversationStarted") start = i + 1;
    else if (typeof e.kind === "object" && "Orchestrator" in e.kind &&
      e.kind.Orchestrator.startsWith("session resumed with a fresh adapter session")) start = i;
  });
  for (const e of events.slice(start)) {
    const k = e.kind;
    if (typeof k === "string") continue;
    if ("SessionUpdate" in k) {
      const raw = k.SessionUpdate;
      const u = raw?.update ?? raw;
      const kind = u?.sessionUpdate as string | undefined;
      if (kind === "agent_message_chunk" || kind === "agent_thought_chunk" || kind === "user_message_chunk") {
        const role = kind === "agent_message_chunk" ? "agent" : kind === "agent_thought_chunk" ? "thought" : "user";
        const last = items[items.length - 1];
        // Agent/thought chunks stream; user prompts are whole messages.
        if (last && last.type === "msg" && last.role === role && role !== "user") last.text += text(u);
        else items.push({ type: "msg", role, text: text(u), seq: e.seq, ts: e.ts });
      } else if (kind === "tool_call" || kind === "tool_call_update") {
        const id = String(u.toolCallId ?? e.seq);
        const idx = tools.get(id);
        const detail = toolDetail(u);
        if (idx === undefined) {
          tools.set(id, items.length);
          items.push({
            type: "tool", id, title: u.title ?? "tool", kind: u.kind ?? "other",
            status: u.status ?? "pending", detail, seq: e.seq, ts: e.ts,
          });
        } else {
          const t = items[idx];
          if (t.type === "tool") {
            if (u.title) t.title = u.title;
            if (u.status) t.status = u.status;
            if (detail) t.detail = detail;
          }
        }
      }
    } else if ("FileEdited" in k) {
      items.push({ type: "edit", path: k.FileEdited.path, seq: e.seq, ts: e.ts });
    } else if ("AgentExited" in k) {
      items.push({ type: "note", text: `Agent exited${k.AgentExited.code != null ? ` (code ${k.AgentExited.code})` : ""}`, tone: "error", seq: e.seq, ts: e.ts });
    } else if ("FileOverlap" in k) {
      items.push({ type: "note", text: `${k.FileOverlap.path} was also edited recently by another session`, tone: "warn", seq: e.seq, ts: e.ts });
    } else if ("StateChanged" in k) {
      const to = k.StateChanged.to;
      if (typeof to === "object") items.push({ type: "note", text: to.Error, tone: "error", seq: e.seq, ts: e.ts });
    }
  }
  return items;
}

function toolDetail(u: any): string {
  const parts: string[] = [];
  if (u.rawInput) parts.push(typeof u.rawInput === "string" ? u.rawInput : JSON.stringify(u.rawInput, null, 2));
  for (const c of u.content ?? []) {
    const t = c?.content?.text ?? c?.text;
    if (t) parts.push(String(t));
    else if (c?.type === "diff") parts.push(`${c.path ?? ""}`.trim());
  }
  return parts.join("\n").slice(0, 4000);
}

/** Permission requests that have no matching `PermissionResolved` yet. */
export function pendingPermissions(events: SessionEvent[]): PendingPermission[] {
  const resolved = new Set<string>();
  for (const e of events) {
    if (typeof e.kind === "object" && "PermissionResolved" in e.kind) resolved.add(e.kind.PermissionResolved.request_id);
  }
  const out: PendingPermission[] = [];
  for (const e of events) {
    if (typeof e.kind !== "object" || !("PermissionRequest" in e.kind)) continue;
    const { request_id, request } = e.kind.PermissionRequest;
    if (resolved.has(request_id)) continue;
    const tc = request?.toolCall ?? {};
    out.push({
      requestId: request_id, seq: e.seq, ts: e.ts,
      title: tc.title ?? "The agent wants to run a tool",
      detail: toolDetail(tc),
      options: (request?.options ?? []) as PermissionOption[],
    });
  }
  return out;
}

/** Map an ACP option kind to the daemon's decision. */
export function decisionFor(opt: PermissionOption): PermissionDecision {
  switch (opt.kind) {
    case "allow_once": return "allow_once";
    case "allow_always": return "allow_always";
    default: return "reject";
  }
}

/** Insert events keeping order by seq and dropping duplicates. */
export function mergeEvents(existing: SessionEvent[], incoming: SessionEvent[]): SessionEvent[] {
  const bySeq = new Map<number, SessionEvent>();
  for (const e of existing) bySeq.set(e.seq, e);
  for (const e of incoming) bySeq.set(e.seq, e);
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

export function sessionTitle(events: SessionEvent[]): string | null {
  let title: string | null = null;
  for (const e of events) {
    if (typeof e.kind === "object" && "TitleChanged" in e.kind) title = e.kind.TitleChanged.title;
  }
  return title;
}
