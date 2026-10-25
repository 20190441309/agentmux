import { execFileSync, spawn, ChildProcess } from "node:child_process";
import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "../..");
const bin = (n: string) => path.join(root, "target/debug", n);

export interface Daemon {
  url: string;
  pairLink(): string;
  rpc(method: string, params?: unknown): Promise<any>;
  stop(): void;
  ids: { workspace: string; sessionA: string; sessionB: string };
}

/** Start an isolated daemon with the web gateway, the mock agent and one seeded workspace. */
export async function startDaemon(port: number): Promise<Daemon> {
  const dir = mkdtempSync(path.join(tmpdir(), "agentmux-e2e-"));
  const repo = path.join(dir, "repo");
  mkdirSync(repo);
  const git = (...a: string[]) => execFileSync("git", ["-C", repo, ...a], { stdio: "ignore" });
  git("init", "-b", "main");
  git("config", "user.email", "t@example.com");
  git("config", "user.name", "t");
  writeFileSync(path.join(repo, "a.txt"), "x\n");
  git("add", ".");
  git("commit", "-m", "init");

  const config = path.join(dir, "config.toml");
  writeFileSync(
    config,
    `[[agents]]\nid = "mock"\nname = "Mock"\ncommand = ${JSON.stringify(bin("agentmux-mock-agent"))}\n[agents.env]\nMOCK_PLAN = "1"\n`,
  );
  const sock = path.join(dir, "d.sock");
  const common = ["--socket", sock, "--data-dir", path.join(dir, "data"), "--config", config];
  const proc: ChildProcess = spawn(
    bin("agentmux-server"),
    ["--serve", ...common, "--web-listen", `127.0.0.1:${port}`],
    { stdio: "ignore" },
  );
  for (let i = 0; i < 100; i++) {
    if (await new Promise<boolean>((r) => net.connect(sock).once("connect", () => r(true)).once("error", () => r(false)))) break;
    await new Promise((r) => setTimeout(r, 100));
  }

  let nextId = 1;
  const rpc = (method: string, params: unknown = null) =>
    new Promise<any>((resolve, reject) => {
      const c = net.connect(sock);
      let buf = "";
      c.on("data", (d) => {
        buf += d;
        if (!buf.includes("\n")) return;
        c.end();
        const msg = JSON.parse(buf.split("\n")[0]);
        msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
      });
      c.on("error", reject);
      c.write(JSON.stringify({ jsonrpc: "2.0", id: nextId++, method, params }) + "\n");
    });

  const { project } = await rpc("project/register", { root_path: repo });
  const { workspace } = await rpc("workspace/create", { project_id: project.id, name: "e2e" });
  const a = (await rpc("session/create", { workspace_id: workspace.id, agent_id: "mock" })).session;
  const b = (await rpc("session/create", { workspace_id: workspace.id, agent_id: "mock" })).session;
  await rpc("session/title", { session_id: a.id, title: "Alpha" });
  await rpc("session/title", { session_id: b.id, title: "Beta" });

  return {
    url: `http://127.0.0.1:${port}`,
    pairLink: () => execFileSync(bin("agentmux-server"), ["--pair", ...common]).toString().split("\n")[0].replace(/^\s+/, ""),
    rpc,
    stop: () => proc.kill(),
    ids: { workspace: workspace.id, sessionA: a.id, sessionB: b.id },
  };
}
