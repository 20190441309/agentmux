import { describe, expect, it } from "vitest";
import { buildItems, decisionFor, mergeEvents, pendingPermissions } from "./transcript";
import type { EventKind, SessionEvent } from "./types";

const ev = (seq: number, kind: EventKind): SessionEvent => ({ session_id: "s", seq, ts: "2026-10-25T00:00:00Z", kind });
const upd = (seq: number, u: any) => ev(seq, { SessionUpdate: { update: u } });
const chunk = (seq: number, kind: string, t: string) => upd(seq, { sessionUpdate: kind, content: { text: t } });

describe("buildItems", () => {
  it("coalesces streamed agent chunks but keeps user prompts separate", () => {
    const items = buildItems([
      chunk(1, "user_message_chunk", "hi"), chunk(2, "user_message_chunk", "again"),
      chunk(3, "agent_message_chunk", "he"), chunk(4, "agent_message_chunk", "llo"),
    ]);
    expect(items.map((i) => i.type === "msg" && `${i.role}:${i.text}`)).toEqual(["user:hi", "user:again", "agent:hello"]);
  });

  it("merges tool call updates into one card", () => {
    const items = buildItems([
      upd(1, { sessionUpdate: "tool_call", toolCallId: "t1", title: "Edit a.rs", status: "pending" }),
      upd(2, { sessionUpdate: "tool_call_update", toolCallId: "t1", status: "completed" }),
    ]);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ type: "tool", title: "Edit a.rs", status: "completed" });
  });

  it("starts over at ConversationStarted", () => {
    const items = buildItems([chunk(1, "user_message_chunk", "old"), ev(2, "ConversationStarted"), chunk(3, "user_message_chunk", "new")]);
    expect(items).toHaveLength(1);
  });

  it("surfaces errors and overlap notices", () => {
    const items = buildItems([
      ev(1, { StateChanged: { from: "Ready", to: { Error: "daemon restarted" } } }),
      ev(2, { FileOverlap: { path: "a.rs", others: ["x"] } }),
    ]);
    expect(items.map((i) => i.type === "note" && i.tone)).toEqual(["error", "warn"]);
  });
});

describe("permissions", () => {
  const req = ev(1, { PermissionRequest: { request_id: "r1", request: { toolCall: { title: "Run cargo test" }, options: [{ optionId: "a", name: "Allow", kind: "allow_once" }] } } });
  it("lists only unresolved requests", () => {
    expect(pendingPermissions([req])).toHaveLength(1);
    expect(pendingPermissions([req, ev(2, { PermissionResolved: { request_id: "r1", outcome: "selected:a" } })])).toHaveLength(0);
  });
  it("maps option kinds to decisions", () => {
    expect(decisionFor({ optionId: "a", name: "", kind: "allow_always" })).toBe("allow_always");
    expect(decisionFor({ optionId: "a", name: "", kind: "reject_once" })).toBe("reject");
  });
});

describe("mergeEvents", () => {
  it("dedupes by seq and sorts", () => {
    const merged = mergeEvents([chunk(2, "agent_message_chunk", "b")], [chunk(1, "agent_message_chunk", "a"), chunk(2, "agent_message_chunk", "b")]);
    expect(merged.map((e) => e.seq)).toEqual([1, 2]);
  });
});
