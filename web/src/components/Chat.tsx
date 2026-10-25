import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import Markdown from "react-markdown";
import { cancelTurn, loadOlder, renameSession, sendPrompt, sessionLabel, useStore } from "../store";
import { buildItems, pendingPermissions, type Item } from "../transcript";
import { stateName } from "../types";
import { PermissionCard } from "./Inbox";

function ItemView({ item }: { item: Item }) {
  switch (item.type) {
    case "msg":
      return (
        <div className={`msg ${item.role}`}>
          {item.role === "thought" ? <details><summary>Thinking…</summary><Markdown skipHtml>{item.text}</Markdown></details>
            : <Markdown skipHtml>{item.text}</Markdown>}
        </div>
      );
    case "tool":
      return (
        <details className={`tool ${item.status}`}>
          <summary><span className="tdot" />{item.title} <span className="muted">{item.status}</span></summary>
          {item.detail && <pre>{item.detail}</pre>}
        </details>
      );
    case "edit":
      return <div className="edit">✎ edited <code>{item.path}</code></div>;
    case "note":
      return <div className={`note ${item.tone}`}>{item.text}</div>;
  }
}

export function Chat() {
  const sid = useStore((s) => s.selected);
  const events = useStore((s) => (s.selected ? s.events[s.selected] : undefined));
  const hasMore = useStore((s) => (s.selected ? s.hasMore[s.selected] : false));
  const label = useStore((s) => (s.selected ? sessionLabel(s, s.selected) : ""));
  const session = useStore((s) => Object.values(s.sessions).flat().find((x) => x.id === s.selected));
  const items = useMemo(() => buildItems(events ?? []), [events]);
  const perms = useMemo(() => pendingPermissions(events ?? []), [events]);
  const [text, setText] = useState("");
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [behind, setBehind] = useState(false);

  useEffect(() => { stick.current = true; setBehind(false); setText(""); }, [sid]);
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    if (stick.current) el.scrollTop = el.scrollHeight;
    else setBehind(true);
  }, [items.length, perms.length]);

  if (!sid || !session) return <div className="empty">Pick a session to see its conversation.</div>;
  const busy = stateName(session.state) === "Prompting" || stateName(session.state) === "WaitingPermission";
  const send = () => {
    const t = text.trim();
    if (!t) return;
    setText("");
    stick.current = true;
    sendPrompt(sid, t);
  };
  return (
    <div className="chatview">
      <div className="chathead">
        <strong className="grow">{label}</strong>
        <button className="ghost" onClick={() => { const t = prompt("Rename session", label); if (t?.trim()) void renameSession(sid, t.trim()); }}>Rename</button>
      </div>
      <div
        className="scroll" ref={scroller}
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
          if (stick.current) setBehind(false);
        }}
      >
        {hasMore && <button className="ghost more" onClick={() => loadOlder(sid)}>Load earlier messages</button>}
        {items.map((it) => <ItemView key={`${it.type}-${it.seq}`} item={it} />)}
        {perms.map((p) => <PermissionCard key={p.requestId} sid={sid} perm={p} />)}
        {items.length === 0 && perms.length === 0 && <div className="empty">No messages yet.</div>}
      </div>
      {behind && (
        <button className="jump" onClick={() => { const el = scroller.current!; el.scrollTop = el.scrollHeight; stick.current = true; setBehind(false); }}>
          New messages ↓
        </button>
      )}
      <form className="composer" onSubmit={(e) => { e.preventDefault(); send(); }}>
        <textarea
          rows={2} value={text} placeholder={busy ? "Agent is working… (you can still queue a message)" : "Message the agent"}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); send(); } }}
          aria-label="Message"
        />
        <div className="col">
          {busy && <button type="button" className="danger" onClick={() => cancelTurn(sid)}>Stop</button>}
          <button className="primary" disabled={!text.trim()}>Send</button>
        </div>
      </form>
    </div>
  );
}
