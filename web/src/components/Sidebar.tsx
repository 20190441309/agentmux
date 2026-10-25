import { useState } from "react";
import { allSessions, createSession, select, sessionLabel, useStore, type State } from "../store";
import { stateError, stateName, type Session } from "../types";
import { pendingPermissions } from "../transcript";

export function statusOf(s: State, sess: Session): { cls: string; text: string } {
  if ((s.events[sess.id] && pendingPermissions(s.events[sess.id]).length > 0) || sess.state === "WaitingPermission")
    return { cls: "wait", text: "needs approval" };
  const name = stateName(sess.state);
  if (name === "Error") return { cls: "err", text: stateError(sess.state) ?? "error" };
  if (name === "Prompting") return { cls: "run", text: "working" };
  if (name === "Done") return { cls: "done", text: "done" };
  return { cls: "idle", text: name === "Ready" ? "idle" : name.toLowerCase() };
}

export function Sidebar() {
  const st = useStore((s) => s);
  const [agent, setAgent] = useState<Record<string, string>>({});
  const avail = st.agents.filter((a) => a.available && !("Native" in a.adapter)) // native agents only attach in a terminal;
  if (st.projects.length === 0)
    return <div className="empty">No projects yet. Register one from the terminal UI (<code>/project</code>) and it will appear here.</div>;
  return (
    <div className="sidebar">
      {st.projects.map((p) => (
        <section key={p.id}>
          <h2>{p.name}</h2>
          {(st.workspaces[p.id] ?? []).map((w) => (
            <details key={w.id} open>
              <summary>{w.name} <span className="muted">{w.branch}</span></summary>
              <ul>
                {(st.sessions[w.id] ?? []).filter((x) => !x.native_terminal).map((sess) => {
                  const status = statusOf(st, sess);
                  return (
                    <li key={sess.id}>
                      <button className={`session ${st.selected === sess.id ? "sel" : ""}`} onClick={() => select(sess.id)}>
                        <span className={`dot ${status.cls}`} />
                        <span className="name">{sessionLabel(st, sess.id)}</span>
                        <span className={`muted st-${status.cls}`}>{status.text}</span>
                      </button>
                    </li>
                  );
                })}
              </ul>
              {avail.length > 0 && (
                <div className="newrow">
                  <select aria-label="Agent" value={agent[w.id] ?? avail[0].id} onChange={(e) => setAgent({ ...agent, [w.id]: e.target.value })}>
                    {avail.map((a) => <option key={a.id} value={a.id}>{a.name}</option>)}
                  </select>
                  <button onClick={() => createSession(w.id, agent[w.id] ?? avail[0].id)}>+ New session</button>
                </div>
              )}
            </details>
          ))}
        </section>
      ))}
      {allSessions(st).length === 0 && <div className="empty">No sessions yet — start one above.</div>}
    </div>
  );
}
