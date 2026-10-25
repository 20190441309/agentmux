#!/usr/bin/env python3
"""Delegation plans through a real PTY and an isolated mock daemon.

Prerequisites: cargo build --workspace; python3 -m pip install pyte==0.8.2
Run: python3 tui/tests/plans_pty.py
Walks /delegate -> review -> Approve -> done using buttons after the
first command, at narrow and wide sizes, and checks tasks run only after approval, once each.
"""
import json
import subprocess
import tempfile
import time
from pathlib import Path

from wcwidth import wcswidth

from close_panel_pty import BIN, rpc
from multi_agent_pty import Terminal, prompts


def git(root, *args):
    subprocess.run(["git", "-C", str(root), *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def check(sock, project, width, height):
    ws = rpc(sock, "workspace/create", {"project_id": project, "name": f"plans{width}"})["workspace"]
    sid = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "mock"})["session"]["id"]
    rpc(sock, "session/title", {"session_id": sid, "title": f"Plan {width}"})
    terminal = Terminal(sock, width, height)
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(f"Plan {width}")
        terminal.paste("/mux delegate ship it")
        terminal.send("\r")
        terminal.wait(lambda: terminal.visible(" Approve "), timeout=20)
        # Nothing may run before approval: the agent only saw the proposal request.
        assert not any("do the first part" in (p or "") for p in prompts(sock, sid)), "task ran before approval"
        # The hint text also says "Approve"; take the last occurrence (the button).
        rows = terminal.rows()
        y = max(i for i, r in enumerate(rows) if " Approve " in r)
        x = wcswidth(rows[y][: rows[y].rfind(" Approve ")])
        terminal.send(f"\x1b[<0;{x + 3};{y + 1}M\x1b[<0;{x + 3};{y + 1}m")
        deadline = time.monotonic() + 20
        while True:
            plans = rpc(sock, "plan/list", {"workspace_id": ws["id"]})["plans"]
            if plans and plans[0]["state"] == "done":
                break
            assert time.monotonic() < deadline, plans
            terminal.drain()
        terminal.wait(lambda: terminal.visible("Done"))
        sent = [p for p in prompts(sock, sid) if p and "[agentmux plan]" in p]
        assert len(sent) == 2, f"each task must be sent exactly once, got {len(sent)}"
        terminal.click(" Close ")
        terminal.wait(lambda: not terminal.visible(" Close ") or not terminal.visible("Delegation plan"))
        print(f"PASS {width}x{height}: /delegate, review, approve, run to done, tasks sent once")
    finally:
        terminal.close()


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-plans-") as temp:
        base = Path(temp)
        repo = base / "repo"
        repo.mkdir()
        git(repo, "init", "-b", "main")
        git(repo, "config", "user.email", "test@example.com")
        git(repo, "config", "user.name", "Plan Test")
        (repo / "a.txt").write_text("x\n")
        git(repo, "add", ".")
        git(repo, "commit", "-m", "fixture")
        config = base / "config.toml"
        config.write_text(
            '[[agents]]\nid="mock"\nname="Mock"\ncommand=' + json.dumps(str(BIN / "agentmux-mock-agent"))
            + '\n[agents.env]\nMOCK_PLAN="1"\n'
        )
        sock = base / "daemon.sock"
        with (base / "server.log").open("w") as log:
            daemon = subprocess.Popen([str(BIN / "agentmux-server"), "--serve", "--socket", str(sock), "--data-dir", str(base / "data"), "--config", str(config)], stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while not sock.exists():
                    assert daemon.poll() is None
                    assert time.monotonic() < deadline
                    time.sleep(0.05)
                project = rpc(sock, "project/register", {"root_path": str(repo)})["project"]
                for width, height in [(40, 16), (80, 24), (120, 30)]:
                    check(sock, project["id"], width, height)
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
