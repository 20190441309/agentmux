// Read-only lifecycle tracking plus single-writer coordination. Native Pi
// owns every command, model, extension dialog and terminal widget.
import { connect } from "node:net";
import { existsSync, writeFileSync } from "node:fs";

export default function agentmuxNative(pi) {
    const socket = process.env.AGENTMUX_NATIVE_SOCKET;
    const session_id = process.env.AGENTMUX_NATIVE_SESSION;
    const token = process.env.AGENTMUX_NATIVE_TOKEN;
    if (!socket || !session_id || !token) return;
    function call(method, session_file) {
        return new Promise((accept, reject) => {
            const peer = connect(socket); let data = "";
            const timer = setTimeout(() => { peer.destroy(); reject(new Error("Agent manager is unavailable")); }, 5000);
            peer.setEncoding("utf8");
            peer.on("error", (error) => { clearTimeout(timer); reject(error); });
            peer.on("connect", () => peer.write(JSON.stringify({ jsonrpc: "2.0", method, params: { session_id, token, session_file }, id: 1 }) + "\n"));
            peer.on("data", (chunk) => {
                data += chunk; const end = data.indexOf("\n"); if (end < 0) return;
                clearTimeout(timer); peer.destroy();
                try { const reply = JSON.parse(data.slice(0, end)); if (reply.error) reject(new Error(reply.error.message)); else accept(reply.result); }
                catch (error) { reject(error); }
            });
        });
    }
    pi.on("session_before_switch", async (event, ctx) => {
        if (!event.targetSessionFile) return;
        try { await call("session/native/check", event.targetSessionFile); }
        catch (error) { ctx.ui.notify(error.message, "error"); return { cancel: true }; }
    });
    pi.on("session_start", async (_event, ctx) => {
        const file = ctx.sessionManager.getSessionFile(); if (!file) return;
        try {
            // Persist the SDK's header so an empty /new has a stable identity.
            if (!existsSync(file)) {
                const header = ctx.sessionManager.getHeader();
                if (header) writeFileSync(file, JSON.stringify(header) + "\n", { flag: "wx", mode: 0o600 });
            }
            await call("session/native/report", file);
        } catch (error) { ctx.ui.notify(error.message, "error"); ctx.shutdown(); }
    });
}
