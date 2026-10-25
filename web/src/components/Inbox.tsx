import { useMemo } from "react";
import { allSessions, decide, select, sessionLabel, setTab, useStore, type State } from "../store";
import { decisionFor, pendingPermissions } from "../transcript";
import type { PendingPermission } from "../transcript";

interface Entry { sid: string; perm: PendingPermission }

export function inboxEntries(s: State): Entry[] {
  const out: Entry[] = [];
  for (const sess of allSessions(s)) {
    for (const perm of pendingPermissions(s.events[sess.id] ?? [])) out.push({ sid: sess.id, perm });
  }
  return out.sort((a, b) => a.perm.ts.localeCompare(b.perm.ts));
}
export function inboxCount(s: State): number {
  const needsPlan = Object.values(s.plans).flat().filter((p) => p.state === "proposed" || p.state === "paused").length;
  return inboxEntries(s).length + needsPlan;
}

export function PermissionCard({ sid, perm, showSession }: { sid: string; perm: PendingPermission; showSession?: boolean }) {
  const name = useStore((s) => sessionLabel(s, sid));
  // "Always allow" is not offered from the web: grant scope stays a deliberate, local decision.
  const options = perm.options.filter((o) => o.kind !== "allow_always");
  const reject = perm.options.find((o) => o.kind.startsWith("reject"));
  return (
    <div className="card perm" role="group" aria-label="Permission request">
      {showSession && <button className="link" onClick={() => select(sid)}>{name}</button>}
      <div className="title">{perm.title}</div>
      {perm.detail && <pre>{perm.detail}</pre>}
      <div className="actions">
        {options.filter((o) => o.kind.startsWith("allow")).map((o) => (
          <button key={o.optionId} className="primary" onClick={() => decide(sid, perm.requestId, decisionFor(o))}>{o.name}</button>
        ))}
        <button className="danger" onClick={() => decide(sid, perm.requestId, reject ? decisionFor(reject) : "reject")}>
          {reject?.name ?? "Reject"}
        </button>
      </div>
    </div>
  );
}

export function Inbox() {
  // Selectors must return stable references (useSyncExternalStore); derive in render.
  const st = useStore((s) => s);
  const entries = useMemo(() => inboxEntries(st), [st.sessions, st.events]);
  const plans = useMemo(
    () => Object.values(st.plans).flat().filter((p) => p.state === "proposed" || p.state === "paused"),
    [st.plans],
  );
  if (entries.length === 0 && plans.length === 0) return <div className="empty">Nothing needs you right now.</div>;
  return (
    <div className="inbox">
      {entries.map((e) => <PermissionCard key={e.perm.requestId} sid={e.sid} perm={e.perm} showSession />)}
      {plans.map((p) => (
        <div className="card" key={p.id}>
          <div className="title">{p.state === "proposed" ? "Plan ready for review" : "Plan paused"}</div>
          <div className="muted">{p.goal}</div>
          <div className="actions"><button onClick={() => setTab("plans")}>Open plans</button></div>
        </div>
      ))}
    </div>
  );
}
