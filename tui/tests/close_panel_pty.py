#!/usr/bin/env python3
"""Unix PTY regressions for terminal input and sending after daemon restart.

Run from the repository root:
    cargo build --workspace
    python3 -m pip install pyte==0.8.2
    python3 tui/tests/close_panel_pty.py

Uses an isolated daemon, Git repository and mock agent. No user sessions are
accessed. AGENTMUX_TEST_BIN_DIR can override the debug binary directory.
"""

import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time

import pyte


ROOT = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("AGENTMUX_TEST_BIN_DIR", ROOT / "target" / "debug"))


def rpc(sock, method, params, timeout=10):
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(timeout)
        conn.connect(str(sock))
        conn.sendall((json.dumps({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        }) + "\n").encode())
        with conn.makefile() as reader:
            response = json.loads(reader.readline())
        assert "error" not in response, response
        return response["result"]


def check_terminal(sock, width, height, session_id):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
    original_termios = termios.tcgetattr(slave)
    proc = subprocess.Popen(
        [str(BIN / "agentmux-tui"), "--restart-daemon"], stdin=slave, stdout=slave, stderr=slave,
        env=dict(os.environ, AGENTMUX_SOCK=str(sock), TERM="xterm-256color"),
    )
    screen = pyte.Screen(width, height)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")()
    writer = None

    def rows():
        return ["".join(screen.buffer[y][x].data or " " for x in range(width))
                for y in range(height)]

    def drain(seconds=0.1):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.02)[0]:
                stream.feed(decoder.decode(os.read(master, 65536)))

    def wait_for(predicate, timeout=5):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
            assert proc.poll() is None, f"TUI exited: {proc.returncode}"
        raise AssertionError(f"{width}x{height}: timeout\n" + "\n".join(rows()))

    def visible(label):
        return any(label in row for row in rows())

    def click(label):
        wait_for(lambda: visible(label))
        for y, row in enumerate(rows()):
            x = row.find(label)
            if x >= 0:
                os.write(master, f"\x1b[<0;{x+2};{y+1}M\x1b[<0;{x+2};{y+1}m".encode())
                drain()
                return

    def show_panel():
        click(" Menu ")
        click(" Show / hide sidebar ")

    try:
        wait_for(lambda: visible(" Help "))
        page = rpc(sock, "session/history", {"session_id": session_id})
        assert page["conversation_start"] == 0
        assert page["title"] == "previous_conversation_sentinel", "restart discarded the title"
        if width < 110:
            show_panel()
        click(" Close panel ")
        wait_for(lambda: visible(" Menu "))
        # More than crossterm's 1024-byte read buffer. Its default mio source
        # used to leave bytes unread and wait forever for a new readiness edge.
        payload = b"\x1b[<35;10;10M" * 1500 + b"close_probe"

        def send_burst():
            remaining = payload
            while remaining:
                try:
                    remaining = remaining[os.write(master, remaining):]
                except OSError:
                    return

        started = time.monotonic()
        writer = threading.Thread(target=send_burst, daemon=True)
        writer.start()
        wait_for(lambda: visible("close_probe"))
        writer.join(timeout=1)
        assert not writer.is_alive(), "terminal input writer is still blocked"
        latency = time.monotonic() - started
        show_panel()
        click(" Close panel ")
        assert visible("close_probe"), "closing/reopening lost the draft"
        # The TUI startup flag restarted the isolated daemon, exactly
        # like a persisted Error("daemon restarted") session in the UI.
        assert visible("Resume & send"), "terminated task still advertises plain Send"
        click(" Resume & send ")

        def prompt_count(text):
            events = rpc(sock, "session/history", {"session_id": session_id})["events"]
            count = 0
            for event in events:
                if not isinstance(event["kind"], dict):
                    continue
                value = event["kind"].get("SessionUpdate", {})
                update = value.get("update", value)
                if (update.get("sessionUpdate") == "user_message_chunk"
                        and update.get("content", {}).get("text") == text):
                    count += 1
            return count

        # Each dimension uses the same persisted session and adds one prompt.
        expected = {120: 1, 80: 2, 40: 3}[width]
        wait_for(lambda: prompt_count("close_probe") == expected)
        wait_for(lambda: visible(" Send "))
        restored = rpc(sock, "session/history", {"session_id": session_id})
        assert restored["conversation_start"] == 0, "restore created a new conversation"
        assert restored["title"] == "previous_conversation_sentinel", "restore discarded the title"
        os.write(master, "你是谁".encode())
        drain()
        os.write(master, b"\r")
        wait_for(lambda: prompt_count("你是谁") == expected)
        wait_for(lambda: visible(" Send "))
        assert prompt_count("close_probe") == expected, "prompt was sent twice"
        click(" Help ")
        wait_for(lambda: visible("Getting started"))
        click(" Close ")
        click(" Agents ")
        click(" Close ")
        click(" Menu ")
        # On small terminals, scroll the menu to expose its last entries.
        os.write(master, b"\x1b[F")
        drain()
        click(" Exit TUI ")
        proc.wait(timeout=5)
        assert proc.returncode == 0
        assert termios.tcgetattr(slave) == original_termios, "raw mode leaked"
        print(f"PASS {width}x{height}: burst→typing {latency:.2f}s; resume+send once, Chinese Enter send, reopen, help, tasks, exit")
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(slave)
        os.close(master)
        if writer:
            writer.join(timeout=1)


def check_reopened_history(sock):
    """Reopening must retain the original conversation after native restore."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    proc = subprocess.Popen([str(BIN / "agentmux-tui")], stdin=slave, stdout=slave,
                            stderr=slave, env=dict(os.environ, AGENTMUX_SOCK=str(sock), TERM="xterm-256color"))
    screen = pyte.Screen(120, 40)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")()
    try:
        deadline = time.monotonic() + 8
        while True:
            if select.select([master], [], [], 0.05)[0]:
                stream.feed(decoder.decode(os.read(master, 65536)))
            visible = "\n".join(screen.display)
            if "Ask anything" in visible and "你是谁" in visible:
                break
            assert proc.poll() is None, "reopened TUI exited"
            assert time.monotonic() < deadline, visible
        os.write(master, b"/mux quit\r")
        proc.wait(timeout=5)
        assert proc.returncode == 0
        print("PASS reopen: original native conversation remains readable")
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(slave)
        os.close(master)


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-close-panel-") as tmp:
        base = Path(tmp)
        repo = base / "repo"
        repo.mkdir()

        def git(*args):
            subprocess.run(["git", "-C", str(repo), *args], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)

        git("init", "-b", "main")
        git("config", "user.email", "test@example.com")
        git("config", "user.name", "TUI Regression")
        (repo / "README.md").write_text("fixture\n")
        git("add", ".")
        git("commit", "-m", "fixture")
        config = base / "config.toml"
        config.write_text('[[agents]]\nid="mock"\nname="Mock"\ncommand='
                          + json.dumps(str(BIN / "agentmux-mock-agent")) + "\n")
        sock = base / "daemon.sock"
        with (base / "server.log").open("w") as log:
            command = [
                str(BIN / "agentmux-server"), "--serve", "--socket", str(sock),
                "--data-dir", str(base / "data"), "--config", str(config),
            ]
            daemon = subprocess.Popen(command, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while not sock.exists():
                    assert daemon.poll() is None, "isolated daemon exited"
                    assert time.monotonic() < deadline, "daemon startup timed out"
                    time.sleep(0.05)
                project = rpc(sock, "project/register", {"root_path": str(repo)})["project"]
                ws = rpc(sock, "workspace/create", {
                    "project_id": project["id"], "name": "test",
                })["workspace"]
                session = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "mock"})["session"]
                for width, height in [(120, 30), (80, 24), (40, 16)]:
                    rpc(sock, "session/prompt", {"session_id": session["id"],
                        "text": "previous_conversation_sentinel", "references": []})
                    check_terminal(sock, width, height, session["id"])
                    check_reopened_history(sock)
            finally:
                if sock.exists():
                    rpc(sock, "server/shutdown", None)
                    deadline = time.monotonic() + 5
                    while sock.exists() and time.monotonic() < deadline:
                        time.sleep(0.05)
                    assert not sock.exists(), "replacement daemon did not exit"
                if daemon.poll() is None:
                    daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
