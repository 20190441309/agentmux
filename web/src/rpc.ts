// JSON-RPC 2.0 over a WebSocket, one text frame per message — the same
// protocol the daemon speaks on its unix socket.

export type Notify = (method: string, params: any) => void;
export type ConnStatus = "connecting" | "open" | "reconnecting" | "unauthorized";

export class RpcError extends Error {
  constructor(public code: number, message: string) { super(message); }
}

const TOKEN_KEY = "agentmux.token";
export const tokenStore = {
  get: () => localStorage.getItem(TOKEN_KEY),
  set: (t: string) => localStorage.setItem(TOKEN_KEY, t),
  clear: () => localStorage.removeItem(TOKEN_KEY),
};

export function wsUrl(loc: Pick<Location, "protocol" | "host"> = location): string {
  return `${loc.protocol === "https:" ? "wss" : "ws"}://${loc.host}/ws`;
}

export async function pairWithCode(code: string, name: string): Promise<string> {
  const r = await fetch("/api/pair", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ code, name }),
  });
  if (r.status === 429) throw new Error("Too many attempts. Wait a few minutes and try again.");
  if (!r.ok) throw new Error("That code is invalid or expired. Ask for a new one.");
  return (await r.json()).token as string;
}

interface Pending { resolve: (v: any) => void; reject: (e: Error) => void }

export class RpcClient {
  private ws: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private closed = false;
  private attempt = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private opened: (() => void) | null = null;

  constructor(
    private token: string,
    private onNotify: Notify,
    private onStatus: (s: ConnStatus) => void,
    private opts: { reconnect: boolean } = { reconnect: true },
  ) {}

  connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.opened = resolve;
      this.open(reject);
    });
  }

  private open(onFirstFail?: (e: Error) => void) {
    this.onStatus(this.attempt === 0 ? "connecting" : "reconnecting");
    const ws = new WebSocket(wsUrl(), ["agentmux.v1", `token.${this.token}`]);
    this.ws = ws;
    ws.onopen = () => {
      this.attempt = 0;
      this.onStatus("open");
      this.opened?.();
      this.opened = null;
    };
    ws.onmessage = (ev) => {
      let msg: any;
      try { msg = JSON.parse(ev.data); } catch { return; }
      if (msg.id !== undefined && msg.id !== null && this.pending.has(msg.id)) {
        const p = this.pending.get(msg.id)!;
        this.pending.delete(msg.id);
        if (msg.error) p.reject(new RpcError(msg.error.code, msg.error.message));
        else p.resolve(msg.result);
      } else if (msg.method) {
        this.onNotify(msg.method, msg.params);
      }
    };
    ws.onclose = () => {
      for (const p of this.pending.values()) p.reject(new Error("connection closed"));
      this.pending.clear();
      if (this.closed) return;
      void this.afterClose(onFirstFail);
    };
  }

  private async afterClose(onFirstFail?: (e: Error) => void) {
    // A refused upgrade is indistinguishable from a network drop in the
    // browser; ask the API whether the token is still good.
    try {
      const r = await fetch("/api/whoami", { headers: { authorization: `Bearer ${this.token}` } });
      if (r.status === 401) {
        this.closed = true;
        this.onStatus("unauthorized");
        onFirstFail?.(new Error("unauthorized"));
        return;
      }
    } catch { /* offline: fall through to retry */ }
    if (!this.opts.reconnect) {
      onFirstFail?.(new Error("connection failed"));
      return;
    }
    const delay = Math.min(30_000, 500 * 2 ** this.attempt++);
    this.timer = setTimeout(() => !this.closed && this.open(), delay);
  }

  call<T = any>(method: string, params: unknown = null): Promise<T> {
    return new Promise((resolve, reject) => {
      if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
        reject(new Error("not connected"));
        return;
      }
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  close() {
    this.closed = true;
    clearTimeout(this.timer);
    this.ws?.close();
  }
}

/** One-shot connection for calls that hold the line for a long time
 *  (`session/prompt` blocks until the turn ends). */
export async function callOnFreshConnection<T = any>(token: string, method: string, params: unknown): Promise<T> {
  const c = new RpcClient(token, () => {}, () => {}, { reconnect: false });
  await c.connect();
  try { return await c.call<T>(method, params); } finally { c.close(); }
}
