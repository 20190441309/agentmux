#!/usr/bin/env python3
"""Read-only Git review through an isolated daemon and real terminal."""
import json
from pathlib import Path
import subprocess
import tempfile
import time

from close_panel_pty import BIN, rpc
from multi_agent_pty import Terminal, prompts


def git(root, *args):
    result = subprocess.run(["git", "-C", str(root), *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return result.stdout


def check(sock, project, width, height):
    ws = rpc(sock, "workspace/create", {"project_id": project, "name": f"files{width}"})["workspace"]
    root = Path(ws["worktree_path"])
    sid = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "mock"})["session"]["id"]
    rpc(sock, "session/title", {"session_id": sid, "title": f"Review {width}"})
    staged = [f"base_{index}" for index in range(60)]
    staged[5], staged[50] = "STAGED_5", "STAGED_50"
    (root / "manual.txt").write_text("\n".join(staged) + "\n")
    git(root, "add", "manual.txt")
    staged[5], staged[50] = "WORKING_5", "WORKING_50"
    (root / "manual.txt").write_text("\n".join(staged) + "\n")
    (root / "中文 新文件.txt").write_text("UNTRACKED_SENTINEL\n")
    (root / "old.txt").rename(root / "renamed.txt")
    git(root, "add", "old.txt", "renamed.txt")
    (root / "deleted.txt").unlink()
    (root / "binary.bin").write_bytes(b"\x00\x01")
    before = git(root, "status", "--porcelain=v1", "-z")
    terminal = Terminal(sock, width, height)
    draft = f"draft_{width}_中文"
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(f"Review {width}")
        terminal.paste(draft)
        terminal.click(" Menu ")
        terminal.click("│ Files ")
        terminal.click(" Find ")
        terminal.paste("manual.txt")
        terminal.wait(lambda: terminal.visible("› manual.txt"))
        terminal.click("› manual.txt")
        terminal.wait(lambda: terminal.visible("Back to chat") and terminal.visible("Next hunk"))
        terminal.click(" Next hunk ")
        terminal.wait(lambda: terminal.visible("WORKING_5"))
        terminal.click(" Next hunk ")
        terminal.wait(lambda: terminal.visible("WORKING_50"))
        terminal.click(" Staged ")
        terminal.wait(lambda: terminal.visible("Staged") and not terminal.visible("loading"))
        terminal.send("\x1b[H")
        terminal.click(" Next hunk ")
        terminal.wait(lambda: terminal.visible("STAGED_5") or terminal.visible("STAGED_50"))
        terminal.click(" Unstaged ")
        terminal.wait(lambda: terminal.visible("Unstaged") and not terminal.visible("loading"))
        terminal.wait(lambda: terminal.visible("WORKING_5") or terminal.visible("WORKING_50"))
        terminal.click(" Back to chat ")
        terminal.wait(lambda: terminal.visible(draft))
        assert draft not in prompts(sock, sid), "review submitted the draft"
        assert not prompts(sock, sid), "Git review sent a message to the agent"
        assert git(root, "status", "--porcelain=v1", "-z") == before, "Git review changed the index or worktree"
        terminal.send("\x7f" * (len(draft) + 5))
        print(f"PASS {width}x{height}: manual Git changes, filename search, HEAD/staged/unstaged scopes, wrapped hunk navigation, drafts, no Git writes")
    finally:
        if terminal.proc.poll() is None:
            for _ in range(3):
                terminal.send("\x1b")
            terminal.send("\x1bOF")
            terminal.send("\x7f" * 200)
        terminal.close()
        rpc(sock, "session/kill", {"session_id": sid})


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-files-") as temp:
        base = Path(temp)
        repo = base / "repo"
        repo.mkdir()
        git(repo, "init", "-b", "main")
        git(repo, "config", "user.email", "test@example.com")
        git(repo, "config", "user.name", "Git Review Test")
        (repo / "manual.txt").write_text("\n".join(f"base_{index}" for index in range(60)) + "\n")
        (repo / "old.txt").write_text("rename contents\n")
        (repo / "deleted.txt").write_text("deleted contents\n")
        git(repo, "add", ".")
        git(repo, "commit", "-m", "fixture")
        config = base / "config.toml"
        config.write_text('[[agents]]\nid="mock"\nname="Mock"\ncommand=' + json.dumps(str(BIN / "agentmux-mock-agent")) + "\n")
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
                for width, height in [(40, 16), (80, 24), (120, 30), (160, 40)]:
                    check(sock, project["id"], width, height)
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
