#!/usr/bin/env python3
"""U04/U05 keyboard workflows and dynamic filters with 24+ isolated sessions."""
import json
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from close_panel_pty import BIN, ROOT, rpc
from multi_agent_pty import Terminal, prompts
from workbench_features_pty import clean, menu


def check(sock, project, width, height):
    ws = rpc(sock, "workspace/create", {"project_id": project, "name": f"nav{width}"})["workspace"]
    other = rpc(sock, "workspace/create", {"project_id": project, "name": f"other{width}"})["workspace"]
    sessions = []
    for index in range(24):
        space = ws if index < 16 else other
        sid = rpc(sock, "session/create", {"workspace_id": space["id"], "agent_id": "mock"})["session"]["id"]
        title = f"Regression 中文 {width}" if index in [5, 17] else f"Task {width} {index}"
        rpc(sock, "session/title", {"session_id": sid, "title": title})
        sessions.append(sid)
    slow = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "pi"})["session"]["id"]
    rpc(sock, "session/title", {"session_id": slow, "title": f"Slow {width}"})
    terminal = Terminal(sock, width, height)
    worker = None
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(f"Task {width} 0")
        draft = f"NAV_DRAFT_{width}"
        terminal.paste(draft)
        terminal.click(" Agents ")
        terminal.paste(f"rg 中文 {width}")
        terminal.wait(lambda: terminal.visible(f"Regression 中文 {width}"))
        terminal.click("This space")
        assert not terminal.visible("Other spaces"), "current-space filter leaked another workspace"
        terminal.send("\r")
        terminal.wait(lambda: terminal.visible(f"Regression 中文 {width}"))
        terminal.paste(f"TARGET_{width}")
        terminal.send("\r")
        terminal.wait(lambda: f"TARGET_{width}" in prompts(sock, sessions[5]))
        assert not prompts(sock, sessions[17]), "same-named agent in another space received the prompt"
        terminal.choose_conversation(f"Task {width} 0")
        terminal.wait(lambda: terminal.visible(draft))
        terminal.send("\x1bOS")  # F4, then the workspace row can be folded by mouse.
        terminal.wait(lambda: terminal.visible(f"▾ nav{width}"))
        terminal.click(f"▾ nav{width}")
        terminal.wait(lambda: terminal.visible(f"▸ nav{width}"))
        terminal.send("\x1bOQ")
        terminal.choose_conversation(f"Slow {width}")
        # Selecting a member through the picker expands its collapsed group.
        terminal.send("\x7f" * 200)
        terminal.send("slow\r")
        terminal.wait(lambda: terminal.visible(" Stop "))
        terminal.paste("CANCEL_DRAFT")
        terminal.send("\x03")
        terminal.wait(lambda: terminal.visible(" Send "))
        assert terminal.visible("CANCEL_DRAFT"), "Ctrl+C discarded the editor draft"
        terminal.send("\x1bOR")
        terminal.send("\x03")
        terminal.send("\x1bOQ")
        terminal.wait(lambda: terminal.visible("CANCEL_DRAFT"))
        terminal.click(" Agents ")
        terminal.click("Running")
        terminal.wait(lambda: terminal.visible("No matching agents"))
        terminal.click("All")
        terminal.paste(f"Task {width} 0")
        terminal.send("\r")
        terminal.wait(lambda: terminal.visible(draft))
        clean(terminal)
        print(f"PASS {width}x{height}: 25 sessions, Unicode fuzzy search, combined workspace filter, same-name routing, fold/unfold, editor/reading focus, real slow-turn Ctrl+C, drafts")
    finally:
        if terminal.proc.poll() is None:
            clean(terminal)
        terminal.close()
        for sid in sessions + [slow]:
            rpc(sock, "session/kill", {"session_id": sid})


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-nav-") as temp:
        base = Path(temp)
        repo = base / "repo"
        repo.mkdir()
        for args in [("init", "-b", "main"), ("config", "user.email", "test@example.com"), ("config", "user.name", "Navigation Test")]:
            subprocess.run(["git", "-C", str(repo), *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        (repo / "README.md").write_text("fixture\n")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-m", "fixture"], check=True, stdout=subprocess.DEVNULL)
        config = base / "config.toml"
        config.write_text('[[agents]]\nid="mock"\nname="Mock"\ncommand=' + json.dumps(str(BIN / "agentmux-mock-agent")) + '\n[[agents]]\nid="pi"\nname="Fake Pi"\nkind="pi-rpc"\ncommand="python3"\nargs=[' + json.dumps(str(ROOT / "core/tests/fake_pi.py")) + ']\n')
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
                for width, height in [(40,16),(80,24),(120,30),(160,40)]:
                    check(sock, project["id"], width, height)
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
