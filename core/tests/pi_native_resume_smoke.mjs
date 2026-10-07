// Optional real-Pi smoke test; no prompts, model calls, or user sessions.
// cargo build --workspace
// node core/tests/pi_native_resume_smoke.mjs /path/to/pi-coding-agent/dist/core/session-manager.js
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { connect } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

assert(process.argv[2], "Pass the installed Pi SDK session-manager.js path");
const { SessionManager, CURRENT_SESSION_VERSION } = await import(pathToFileURL(resolve(process.argv[2])).href);
const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const base = await mkdtemp(join(tmpdir(), "agentmux-native-pi-"));
const socket = join(base, "daemon.sock");
let daemon;
let logs = "";

async function rpc(method, params = null) {
    const peer = connect(socket);
    peer.setEncoding("utf8");
    let buffer = "";
    try {
        return await new Promise((accept, reject) => {
            const timeout = setTimeout(() => {
                peer.destroy();
                reject(new Error(`Timed out: ${method}\n${logs}`));
            }, 15000);
            peer.on("error", (error) => { clearTimeout(timeout); reject(error); });
            peer.on("connect", () => peer.write(JSON.stringify({ jsonrpc: "2.0", method, params, id: 1 }) + "\n"));
            peer.on("data", (chunk) => {
                buffer += chunk;
                const end = buffer.indexOf("\n");
                if (end < 0) return;
                clearTimeout(timeout);
                const response = JSON.parse(buffer.slice(0, end));
                if (response.error) reject(new Error(response.error.message));
                else accept(response.result);
            });
        });
    } finally {
        peer.destroy();
    }
}

async function start() {
    daemon = spawn(join(root, "target/debug/agentmux-server"), [
        "--serve", "--socket", socket, "--data-dir", join(base, "data"), "--config", join(base, "config.toml"),
    ], { stdio: ["ignore", "pipe", "pipe"] });
    for (const output of [daemon.stdout, daemon.stderr]) {
        output.setEncoding("utf8");
        output.on("data", (data) => { logs = (logs + data).slice(-16000); });
    }
    for (let retry = 0; retry < 100; retry++) {
        assert(daemon.exitCode === null, logs);
        try { await rpc("server/status"); return; } catch { await delay(50); }
    }
    throw new Error(`Daemon did not start\n${logs}`);
}

async function stop() {
    const current = daemon;
    if (!current || current.exitCode !== null) return;
    const exited = once(current, "exit");
    const timeout = setTimeout(() => current.kill("SIGKILL"), 5000);
    try {
        try { await rpc("server/shutdown"); } catch { current.kill(); }
        await exited;
    } finally {
        clearTimeout(timeout);
    }
}

try {
    const repo = join(base, "repo");
    await mkdir(repo);
    await mkdir(join(base, "home"));
    for (const args of [["init", "-b", "main"], ["config", "user.email", "test@example.com"], ["config", "user.name", "test"]]) {
        execFileSync("git", args, { cwd: repo, stdio: "ignore" });
    }
    await writeFile(join(repo, "README.md"), "# isolated test\n");
    execFileSync("git", ["add", "."], { cwd: repo });
    execFileSync("git", ["commit", "-m", "init"], { cwd: repo, stdio: "ignore" });
    const config = [
        "[[agents]]", 'id = "pi"', 'name = "Isolated real Pi"', 'kind = "pi-rpc"', 'command = "pi"',
        `args = ${JSON.stringify(["--mode", "rpc", "--session-dir", join(base, "sessions"), "--no-extensions", "--no-skills", "--no-prompt-templates", "--no-themes"])}`,
        "[agents.env]", `HOME = ${JSON.stringify(join(base, "home"))}`,
        `PI_CODING_AGENT_DIR = ${JSON.stringify(join(base, "pi-agent"))}`,
    ].join("\n");
    await writeFile(join(base, "config.toml"), config);
    await start();
    const { project } = await rpc("project/register", { root_path: repo });
    const { workspace } = await rpc("workspace/create", { project_id: project.id, name: "native" });
    const { session } = await rpc("session/create", { workspace_id: workspace.id, agent_id: "pi" });
    assert(session.native_session_file, "Pi native locator was not persisted");
    const state = await rpc("session/pi", { session_id: session.id, command: { type: "get_state" } });
    await rpc("session/kill", { session_id: session.id });

    // Pi lazily writes empty sessions. Build a fixture with its reported ID,
    // then use the installed SDK to append valid native conversation entries.
    await writeFile(session.native_session_file, JSON.stringify({
        type: "session", version: CURRENT_SESSION_VERSION, id: session.acp_session_id,
        timestamp: new Date().toISOString(), cwd: workspace.worktree_path,
    }) + "\n");
    const native = SessionManager.open(session.native_session_file);
    native.appendMessage({ role: "user", content: [{ type: "text", text: "native_context_secret" }], timestamp: Date.now() });
    native.appendMessage({
        role: "assistant", content: [{ type: "text", text: "native_fixture_reply" }],
        api: state.model?.api ?? "anthropic-messages", provider: state.model?.provider ?? "anthropic",
        model: state.model?.id ?? "claude-sonnet-4-6", timestamp: Date.now(), stopReason: "stop",
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    });
    await stop();
    await start();
    await rpc("session/resume", { session_id: session.id });
    const restored = await rpc("session/pi", { session_id: session.id, command: { type: "get_state" } });
    assert.equal(restored.sessionId, session.acp_session_id);
    assert.equal(restored.sessionFile, session.native_session_file);
    const context = await rpc("session/pi", { session_id: session.id, command: { type: "get_messages" } });
    assert(JSON.stringify(context.messages).includes("native_context_secret"), "Original native context was lost");
    assert(JSON.stringify(context.messages).includes("native_fixture_reply"));
    console.log("PASS real Pi: native identity/file persisted, daemon restarted, original native context restored; zero model calls");
} finally {
    await stop();
    await rm(base, { recursive: true, force: true });
}
