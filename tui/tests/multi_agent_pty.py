#!/usr/bin/env python3
"""Multi-agent onboarding through a real PTY and an isolated mock daemon.

Prerequisites: cargo build --workspace; python3 -m pip install pyte==0.8.2
Run: python3 tui/tests/multi_agent_pty.py
Does not restart or access the user's daemon, worktrees or sessions.
"""
import codecs
import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path

import pyte
from wcwidth import wcswidth
from close_panel_pty import BIN, rpc


class Terminal:
    def __init__(self, sock, width, height, env=None):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.original = termios.tcgetattr(self.slave)
        self.proc = subprocess.Popen(
            [str(BIN / "agentmux-tui")], stdin=self.slave, stdout=self.slave, stderr=self.slave,
            env=dict(os.environ, AGENTMUX_SOCK=str(sock), TERM="xterm-256color", **(env or {})),
        )
        self.width, self.height = width, height
        self.screen = pyte.Screen(width, height)
        self.stream = pyte.Stream(self.screen)
        self.decoder = codecs.getincrementaldecoder("utf-8")()

    def rows(self):
        return ["".join(self.screen.buffer[y][x].data for x in range(self.width))
                for y in range(self.height)]

    def visible(self, text):
        return any(text in row for row in self.rows())

    def drain(self, duration=0.08):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], 0.02)[0]:
                self.stream.feed(self.decoder.decode(os.read(self.master, 65536)))

    def wait(self, predicate, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if predicate():
                return
            assert self.proc.poll() is None, "TUI exited unexpectedly"
        raise AssertionError(f"{self.width}x{self.height}: timeout\n" + "\n".join(self.rows()))

    def send(self, text):
        os.write(self.master, text.encode())
        self.drain()

    def resize(self, width, height):
        self.drain()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.screen.resize(lines=height, columns=width)
        self.width, self.height = width, height
        self.proc.send_signal(signal.SIGWINCH)
        self.wait(lambda: self.visible(" Menu ") and self.visible("To:"))

    def paste(self, text):
        self.send("\x1b[200~" + text + "\x1b[201~")

    def click(self, text, area=None):
        self.wait(lambda: self.visible(text))
        for y, row in enumerate(self.rows()):
            x = row.find(text)
            if x >= 0:
                x = wcswidth(row[:x])
                if area is not None:
                    left, top, width, height = area
                    if not (left <= x < left + width and top <= y < top + height):
                        continue
                self.send(f"\x1b[<0;{x + 2};{y + 1}M\x1b[<0;{x + 2};{y + 1}m")
                return
        raise AssertionError("Click target not in the requested area: " + text)

    def choose_conversation(self, query):
        self.click(" Agents ")
        self.paste(query)
        self.send("\r")

    def close(self):
        try:
            if self.proc.poll() is None:
                self.send("/mux quit\r")
                self.proc.wait(timeout=5)
            assert self.proc.returncode == 0
            assert termios.tcgetattr(self.slave) == self.original, "raw mode leaked"
        finally:
            if self.proc.poll() is None:
                self.proc.terminate()
                self.proc.wait(timeout=5)
            os.close(self.master)
            os.close(self.slave)


def prompts(sock, sid):
    values = []
    for event in rpc(sock, "session/history", {"session_id": sid})["events"]:
        kind = event["kind"]
        if isinstance(kind, dict):
            value = kind.get("SessionUpdate", {})
            update = value.get("update", value)
            if update.get("sessionUpdate") == "user_message_chunk":
                values.append(update.get("content", {}).get("text"))
    return values


def check(sock, project_id, width, height):
    ws = rpc(sock, "workspace/create", {"project_id": project_id, "name": f"team{width}"})["workspace"]
    lead = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "mock"})["session"]
    rpc(sock, "session/prompt", {"session_id": lead["id"], "text": f"lead-{width}"})
    other = rpc(sock, "workspace/create", {"project_id": project_id, "name": f"other{width}"})["workspace"]
    rpc(sock, "session/create", {"workspace_id": other["id"], "agent_id": "mock"})
    sessions = lambda: rpc(sock, "session/list", {"workspace_id": ws["id"]})["sessions"]
    workspace_count = len(rpc(sock, "workspace/list", {"project_id": project_id})["workspaces"])
    name = f"回归测试{width}"
    terminal = Terminal(sock, width, height)
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(f"lead-{width}")
        # The title bar always leads with `space › conversation` on row 0.
        assert f"team{width}" in terminal.rows()[0], "header lost the current space"
        if width == 80:
            assert "alt+enter newline" in terminal.rows()[-1], "footer hint is clipped"
        terminal.paste(f"source_draft_{width}")
        if width == 80:
            for columns, lines in [(20, 8), (40, 16), (120, 30), (160, 40), (80, 24)]:
                terminal.resize(columns, lines)
                terminal.wait(lambda: terminal.visible("team80"))
            terminal.wait(lambda: terminal.visible("source_draft_80"))
            assert "source_draft_80" not in prompts(sock, lead["id"]), "resize submitted the draft"
        terminal.click("+ Agent")
        terminal.wait(lambda: terminal.visible(f"Space: team{width}"))
        terminal.click(" Cancel ")
        assert len(sessions()) == 1, "cancel created an agent"
        assert terminal.visible(f"source_draft_{width}"), "cancel lost draft"
        terminal.click("+ Agent")
        terminal.click("Mock (mock)")
        terminal.wait(lambda: len(sessions()) == 2)
        second = next(session for session in sessions() if session["id"] != lead["id"])
        terminal.wait(lambda: terminal.visible("To: Mock") and not terminal.visible("Add agent · Choose"))
        assert second["workspace_id"] == ws["id"]
        assert len(rpc(sock, "workspace/list", {"project_id": project_id})["workspaces"]) == workspace_count
        terminal.paste(f"second_probe_{width}")
        terminal.click(" Send ")
        terminal.wait(lambda: f"second_probe_{width}" in prompts(sock, second["id"]))
        assert f"second_probe_{width}" not in prompts(sock, lead["id"]), "message reached original agent"
        assert sum(text == f"second_probe_{width}" for text in prompts(sock, second["id"])) == 1
        terminal.click(" Menu ")
        terminal.click("Name agent conversation")
        terminal.wait(lambda: terminal.visible("Name agent conversation"))
        terminal.send("\x7f" * 60)
        terminal.paste(name)
        terminal.click(" Save ")
        terminal.wait(lambda: rpc(sock, "session/history", {"session_id": second["id"]})["title"] == name)
        terminal.choose_conversation(f"lead-{width}")
        terminal.wait(lambda: terminal.visible(f"source_draft_{width}"))
        # Clearing the unsent original draft must not affect the second agent.
        terminal.send("\x7f" * 60)
        terminal.click(" Agents ")
        terminal.wait(lambda: terminal.visible("Current space") and terminal.visible("Other spaces"))
        terminal.click(" Close ")
    finally:
        terminal.close()
    terminal = Terminal(sock, width, height)
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(name)
        terminal.wait(lambda: terminal.visible(name))
        for _ in range(80):
            if terminal.visible(f"second_probe_{width}"):
                break
            terminal.send("\x1b[5~")
        terminal.wait(lambda: terminal.visible(f"second_probe_{width}"))
        assert len(sessions()) == 2, "reopen duplicated agent"
    finally:
        terminal.close()
    print(f"PASS {width}x{height}: compact header, cancel, add in current space, route once, restore draft, rename, grouped picker, reopen; 80-column case also checks live resizing")


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-team-") as tmp:
        base = Path(tmp)
        repo = base / "repo"
        repo.mkdir()
        for args in [("init", "-b", "main"), ("config", "user.email", "test@example.com"),
                     ("config", "user.name", "TUI Regression")]:
            subprocess.run(["git", "-C", str(repo), *args], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        (repo / "README.md").write_text("fixture\n")
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-m", "fixture"], check=True, stdout=subprocess.DEVNULL)
        config = base / "config.toml"
        config.write_text('[[agents]]\nid="mock"\nname="Mock"\ncommand='
                          + json.dumps(str(BIN / "agentmux-mock-agent")) + "\n")
        sock = base / "daemon.sock"
        with (base / "server.log").open("w") as log:
            daemon = subprocess.Popen([str(BIN / "agentmux-server"), "--serve", "--socket", str(sock),
                                       "--data-dir", str(base / "data"), "--config", str(config)], stdout=log, stderr=log)
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
                if daemon.poll() is None:
                    daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
