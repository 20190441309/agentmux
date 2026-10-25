import { useEffect, useState } from "react";
import { Chat } from "./components/Chat";
import { Inbox, inboxCount } from "./components/Inbox";
import { PlansView } from "./components/PlansView";
import { Sidebar } from "./components/Sidebar";
import { clearError, setTab, signIn, signOut, start, useStore, getState, type Tab } from "./store";

function deviceName(): string {
  const ua = navigator.userAgent;
  const browser = /Edg\//.test(ua) ? "Edge" : /Chrome\//.test(ua) ? "Chrome" : /Firefox\//.test(ua) ? "Firefox" : /Safari\//.test(ua) ? "Safari" : "Browser";
  const os = /Android/.test(ua) ? "Android" : /iPhone|iPad/.test(ua) ? "iOS" : /Mac/.test(ua) ? "macOS" : /Windows/.test(ua) ? "Windows" : /Linux/.test(ua) ? "Linux" : "";
  return `${browser}${os ? ` on ${os}` : ""}`;
}

function PairScreen({ initialError }: { initialError?: string }) {
  const [code, setCode] = useState("");
  const [err, setErr] = useState(initialError ?? "");
  const [busy, setBusy] = useState(false);
  const submit = async (c: string) => {
    setBusy(true);
    setErr("");
    try { await signIn(c, deviceName()); } catch (e) { setErr(e instanceof Error ? e.message : String(e)); }
    setBusy(false);
  };
  return (
    <main className="pair">
      <h1>agentmux</h1>
      <p>Enter the pairing code shown on the machine running agentmux, or open the pairing link it printed.</p>
      <p className="hint">Run <code>agentmux-server --pair</code> on that machine to get a new one.</p>
      <form onSubmit={(e) => { e.preventDefault(); void submit(code); }}>
        <input
          autoFocus autoCapitalize="characters" autoComplete="off" spellCheck={false}
          placeholder="PAIRING CODE" value={code} onChange={(e) => setCode(e.target.value)}
          aria-label="Pairing code"
        />
        <button className="primary" disabled={busy || code.trim().length < 6}>{busy ? "Pairing…" : "Pair this device"}</button>
      </form>
      {err && <p role="alert" className="error">{err}</p>}
    </main>
  );
}

const TABS: { id: Tab; label: string }[] = [
  { id: "sessions", label: "Sessions" },
  { id: "chat", label: "Chat" },
  { id: "inbox", label: "Inbox" },
  { id: "plans", label: "Plans" },
];

export function App() {
  const status = useStore((s) => s.status);
  const tab = useStore((s) => s.tab);
  const error = useStore((s) => s.error);
  const inbox = useStore(inboxCount);
  const [pairErr, setPairErr] = useState<string>();

  useEffect(() => {
    const m = /pair=([A-Za-z0-9]+)/.exec(location.hash);
    if (m) {
      history.replaceState(null, "", location.pathname + location.search); // keep the code out of history
      signIn(m[1], deviceName()).catch((e) => setPairErr(e instanceof Error ? e.message : String(e)));
    } else {
      void start();
    }
  }, []);

  if (status === "signed-out") return <PairScreen initialError={pairErr} />;

  return (
    <div className="app" data-tab={tab}>
      <header>
        <strong>agentmux</strong>
        <span className={`dot ${status}`} title={status} />
        <span className="grow" />
        {status !== "open" && <span className="muted">{status === "unauthorized" ? "signed out" : status}…</span>}
        <button className="ghost" onClick={() => { if (confirm("Sign out of this device?")) signOut(); }}>Sign out</button>
      </header>
      {error && (
        <div role="alert" className="banner error" onClick={clearError}>{error} <span className="muted">(tap to dismiss)</span></div>
      )}
      <div className="panes">
        <aside className="pane sessions"><Sidebar /></aside>
        <section className="pane chat"><Chat /></section>
        <section className="pane inbox"><Inbox /></section>
        <section className="pane plans"><PlansView /></section>
      </div>
      <nav className="tabs" aria-label="Sections">
        {TABS.map((t) => (
          <button key={t.id} className={tab === t.id ? "active" : ""} onClick={() => setTab(t.id)}>
            {t.label}{t.id === "inbox" && inbox > 0 && <span className="badge">{inbox}</span>}
          </button>
        ))}
      </nav>
    </div>
  );
}

export { getState };
