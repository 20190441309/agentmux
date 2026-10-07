#!/usr/bin/env python3
"""Pending work and busy-agent error feedback through an isolated mock daemon.

Run after cargo build --workspace, with pyte and wcwidth available.
No user daemon, sessions, credentials or worktrees are accessed.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time

from close_panel_pty import BIN, rpc
from multi_agent_pty import Terminal, prompts


def check(sock, project_id, width, height):
    workspace = rpc(sock, "workspace/create", {"project_id": project_id, "name": f"space{width}"})["workspace"]
    sessions = []
    for title in ["Writer", "Needs approval", "Broken adapter", "Finished work"]:
        session = rpc(sock, "session/create", {"workspace_id": workspace["id"], "agent_id": "mock"})["session"]
        rpc(sock, "session/title", {"session_id": session["id"], "title": f"{title} {width}"})
        sessions.append(session["id"])
    failures = {}
    workers = []

    def start_prompt(sid, message):
        def run():
            try:
                rpc(sock, "session/prompt", {"session_id": sid, "text": message}, timeout=40)
            except Exception as error:
                failures[sid] = str(error)
        worker = threading.Thread(target=run, daemon=True)
        workers.append(worker)
        worker.start()

    def session_state(sid):
        return next(session["state"] for session in rpc(sock, "session/list", {"workspace_id": workspace["id"]})["sessions"] if session["id"] == sid)

    terminal = Terminal(sock, width, height)
    draft = f"writer_draft_{width}_中文"
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(f"Writer {width}")
        terminal.paste(draft)
        start_prompt(sessions[0], "perm writer")
        start_prompt(sessions[1], "perm reviewer")
        start_prompt(sessions[2], "crash")
        start_prompt(sessions[3], "finishedprobe")
        terminal.wait(lambda: terminal.visible("Pending · 4") and terminal.visible("Needs permission"))
        assert terminal.visible(draft), "background work lost the writer draft"
        assert not terminal.visible(" Allow once "), "background permission opened the approval dialog"

        # An actual failing UI request must stay visible while the writer is busy.
        terminal.click(" Menu ")
        terminal.click("│ New space ")
        terminal.click("Add project")
        terminal.paste(str(Path(workspace["worktree_path"]) / "missing-project"))
        terminal.click(" Next ")
        terminal.wait(lambda: terminal.visible("project registration failed"))
        terminal.click(" Cancel ")
        terminal.wait(lambda: terminal.visible("Pending · 5") and terminal.visible("project registration failed"))
        assert terminal.visible("Needs permission"), "error feedback replaced the running status"
        assert terminal.visible(draft), "failed registration changed the draft"
        terminal.click(" Error " if width == 40 else " Error details ")
        terminal.wait(lambda: terminal.visible("Error details") and terminal.visible("project registration failed"))
        terminal.click(" Close ")
        terminal.wait(lambda: terminal.visible("Pending · 4"))

        terminal.click("Pending · 4")
        terminal.wait(lambda: terminal.visible("Permissions") and terminal.visible("Failed") and terminal.visible("Finished"))
        terminal.paste("ignored paste")
        terminal.click(f"Mock #2 / space{width} / Needs approval")
        terminal.wait(lambda: terminal.visible(" Allow once ") and terminal.visible("Needs approval"))
        terminal.click(" Allow once ")
        terminal.wait(lambda: session_state(sessions[1]) == "Ready")
        assert session_state(sessions[0]) == "WaitingPermission", "approved the wrong agent's request"
        terminal.wait(lambda: terminal.visible("Pending · 3"))

        terminal.click("Pending · 3")
        terminal.click(f"Mock #4 / space{width} / Finished work")
        terminal.wait(lambda: terminal.visible("finishedprobe") and terminal.visible("Pending · 2"))
        terminal.click("Pending · 2")
        terminal.click(f"Mock #3 / space{width} / Broken adapter")
        terminal.wait(lambda: terminal.visible("Error details") and terminal.visible(" Resume "))
        terminal.click(" Resume ")
        terminal.wait(lambda: session_state(sessions[2]) == "Ready" and terminal.visible("Pending · 1"))
        terminal.choose_conversation(f"Writer {width}")
        terminal.wait(lambda: terminal.visible(draft))
        assert "ignored paste" not in "\n".join(terminal.rows()), "inbox paste leaked into an editor"
        assert draft not in prompts(sock, sessions[0]), "navigation sent the draft"
        assert prompts(sock, sessions[2]) == ["crash"], "resume sent another agent's draft"

        # Resolve the remaining test request before joining its RPC worker.
        page = rpc(sock, "session/history", {"session_id": sessions[0]})
        for event in page["pending_permissions"]:
            request = event["kind"]["PermissionRequest"]
            rpc(sock, "session/permission", {"session_id": sessions[0], "request_id": request["request_id"], "outcome": "reject"})
        for worker in workers:
            worker.join(timeout=5)
            assert not worker.is_alive(), "test prompt still running"
        assert set(failures) == {sessions[2]}, failures
        terminal.send("\x7f" * (len(draft) + 5))
        print(f"PASS {width}x{height}: busy error banner + details, three pending categories, correct permission routing, completion acknowledgement, resume, drafts and read-only inbox")
    finally:
        if terminal.proc.poll() is None:
            for _ in range(4):
                terminal.send("\x1b")
            terminal.send("\x7f" * 200)
        terminal.close()
        for sid in sessions:
            try:
                rpc(sock, "session/kill", {"session_id": sid})
            except Exception:
                pass
        for worker in workers:
            worker.join(timeout=5)


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-attention-") as tmp:
        base = Path(tmp)
        repo = base / "repo"
        repo.mkdir()
        for args in [("init", "-b", "main"), ("config", "user.email", "test@example.com"), ("config", "user.name", "Attention Regression")]:
            subprocess.run(["git", "-C", str(repo), *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        (repo / "README.md").write_text("fixture\n")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-m", "fixture"], check=True, stdout=subprocess.DEVNULL)
        config = base / "config.toml"
        config.write_text('[[agents]]\nid="mock"\nname="Mock"\ncommand=' + json.dumps(str(BIN / "agentmux-mock-agent")) + "\n")
        sock = base / "daemon.sock"
        with (base / "server.log").open("w") as log:
            daemon = subprocess.Popen([str(BIN / "agentmux-server"), "--serve", "--socket", str(sock), "--data-dir", str(base / "data"), "--config", str(config)], stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while not sock.exists():
                    assert daemon.poll() is None, "isolated daemon exited"
                    assert time.monotonic() < deadline, "daemon startup timed out"
                    time.sleep(0.05)
                project = rpc(sock, "project/register", {"root_path": str(repo)})["project"]
                for width, height in [(40, 16), (80, 24), (120, 30), (160, 40)]:
                    check(sock, project["id"], width, height)
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
