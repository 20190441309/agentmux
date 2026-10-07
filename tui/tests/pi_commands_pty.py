#!/usr/bin/env python3
"""Pi slash commands through a real TUI and isolated daemon/fake Pi adapter.

Prerequisites: cargo build --workspace; python3 -m pip install pyte==0.8.2
Run: python3 tui/tests/pi_commands_pty.py
No user daemon, credentials or sessions are accessed.
"""
import codecs
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import threading
import json
import pyte
from close_panel_pty import BIN, ROOT, rpc


def check(sock, sid, width):
    title = f"commands-width-{width}"
    rpc(sock, "session/title", {"session_id": sid, "title": title})
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, width, 0, 0))
    proc = subprocess.Popen([str(BIN / "agentmux-tui")], stdin=slave, stdout=slave,
                            stderr=slave, env=dict(os.environ, AGENTMUX_SOCK=str(sock), TERM="xterm-256color"))
    screen = pyte.Screen(width, 30)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")()

    def drain(duration=0.12):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.02)[0]:
                stream.feed(decoder.decode(os.read(master, 65536)))

    def rows():
        return ["".join(screen.buffer[y][x].data or " " for x in range(width)) for y in range(30)]

    def visible(text):
        return any(text in row for row in rows())

    def wait(test):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            drain()
            if test():
                return
            assert proc.poll() is None, "TUI exited"
        raise AssertionError("Timed out:\n" + "\n".join(rows()))

    def send(text):
        os.write(master, text.encode())
        drain()

    def click(text):
        wait(lambda: visible(text))
        for y, row in enumerate(rows()):
            x = row.find(text)
            if x >= 0:
                send(f"\x1b[<0;{x+2};{y+1}M\x1b[<0;{x+2};{y+1}m")
                return

    def state():
        return rpc(sock, "session/pi", {"session_id": sid, "command": {"type": "get_state"}})

    try:
        wait(lambda: visible("Ask anything"))
        send("\x10")
        send(title + "\r")
        wait(lambda: visible(title) and visible("Ask anything"))
        send("/")
        wait(lambda: visible("/model"))
        send("fix")
        wait(lambda: visible("/fix-tests"))
        send("\t")
        send("marker\r")
        wait(lambda: visible("fake pi reply: /fix-tests marker"))
        assert not visible("Thought ·"), "invented reasoning for a text-only response"
        send("reasoningprobe tool\r")
        wait(lambda: visible("Thinking ·"))
        wait(lambda: visible("Inspect the request."))
        wait(lambda: visible("the relevant files."))
        wait(lambda: visible("fake pi reply: reasoningprobe"))
        click("Thought ·")
        wait(lambda: not visible("Inspect the request."))
        assert visible("fake pi reply: reasoningprobe"), "folding thoughts hid the answer"
        click("Thought ·")
        wait(lambda: visible("Inspect the request."))
        send("/model\r")
        wait(lambda: visible("fake/large"))
        click("fake/large")
        wait(lambda: state()["model"]["id"] == "large")
        click(" Close ")
        send("/thinking\r")
        wait(lambda: visible(" high"))
        click(" high")
        wait(lambda: state()["thinkingLevel"] == "high")
        click(" Close ")
        send("/mux info\r")
        wait(lambda: visible("Agent details"))
        click(" Refresh ")
        wait(lambda: visible("Model: large") and visible("Thinking: high"))
        assert visible("Usage: Unknown") and visible("Cost: Unknown"), "missing usage/cost was estimated"
        click(" Close ")
        send("/settings\r")
        wait(lambda: visible("Auto compaction:"))
        before = state()["autoCompactionEnabled"]
        click("Auto compaction:")
        wait(lambda: state()["autoCompactionEnabled"] != before)
        click(" Close ")
        send("/compact\r")
        wait(lambda: visible("Fake conversation compacted"))
        click(" Close ")
        send("/mux help\r")
        wait(lambda: visible("Getting started"))
        click(" Close ")
        send("/mux quit\r")
        proc.wait(timeout=5)
        assert proc.returncode == 0
        print(f"PASS {width} cols: streamed reasoning, mouse fold/unfold, separate answer, slash discovery, model/thinking/settings/compact RPC")
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(slave)
        os.close(master)


def check_burst(sock, sid):
    """A stopped TUI falls behind > 1,000 persisted Unicode delta events."""
    master, slave = pty.openpty()
    width, height = 120, 70
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
    proc = subprocess.Popen([str(BIN / "agentmux-tui")], stdin=slave, stdout=slave,
                            stderr=slave, env=dict(os.environ, AGENTMUX_SOCK=str(sock), TERM="xterm-256color"))
    screen = pyte.Screen(width, height)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")()
    def text():
        # pyte.display can index an empty wide-character continuation cell.
        return "\n".join("".join(screen.buffer[y][x].data for x in range(width))
                         for y in range(height))
    def drain():
        if select.select([master], [], [], 0.05)[0]:
            stream.feed(decoder.decode(os.read(master, 65536)))
    def wait_for(check):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            drain()
            if check(): return
            assert proc.poll() is None, "TUI exited during burst"
        raise AssertionError("Burst timeout:\n" + text())
    stopped = False
    try:
        wait_for(lambda: " Help " in text())
        # Resume preserves earlier native messages. Compare this turn's deltas,
        # not every prior turn in the conversation.
        prior = rpc(sock, "session/history", {"session_id": sid})["events"]
        start_seq = max((event["seq"] for event in prior), default=0)
        os.kill(proc.pid, signal.SIGSTOP)
        stopped = True
        errors = []
        def prompt():
            try:
                rpc(sock, "session/prompt", {"session_id": sid, "text": "burstprobe", "references": []})
            except Exception as error:
                errors.append(error)
        worker = threading.Thread(target=prompt, daemon=True)
        worker.start()
        worker.join(timeout=15)
        assert not worker.is_alive() and not errors, errors
        expected_thought = "BEGIN_THOUGHT" + "思考完整性校验-12345-" * 35 + "END_THOUGHT"
        expected_answer = "BEGIN_ANSWER" + "中文回复完整校验-abc123-" * 35 + "END_ANSWER"
        events, before = [], None
        while True:
            page = rpc(sock, "session/history", {"session_id": sid, "before_seq": before})
            events = page["events"] + events
            if not page["has_more"]: break
            before = page["events"][0]["seq"]
        def persisted(role):
            parts = []
            for event in events:
                if event["seq"] <= start_seq:
                    continue
                kind = event["kind"]
                if isinstance(kind, dict):
                    value = kind.get("SessionUpdate", {})
                    value = value.get("update", value)
                    if value.get("sessionUpdate") == role:
                        parts.append(value["content"]["text"])
            return "".join(parts)
        assert persisted("agent_thought_chunk") == expected_thought, "adapter lost reasoning before persistence"
        assert persisted("agent_message_chunk") == expected_answer, "adapter lost text before persistence"
        os.kill(proc.pid, signal.SIGCONT)
        stopped = False
        def compact():
            # The sidebar occupies the last 34 cells; its labels must not
            # interrupt the wrapped conversation when comparing stream bytes.
            chat = "\n".join("".join(screen.buffer[y][x].data for x in range(width - 34)) for y in range(height))
            return "".join(chat.replace("│", "").split())
        try:
            wait_for(lambda: expected_thought in compact() and expected_answer in compact())
        except AssertionError:
            Path("/tmp/agentmux-burst-failure-events.json").write_text(json.dumps(events, ensure_ascii=False))
            raise
        assert compact().count(expected_thought) == compact().count(expected_answer) == 1
        assert "lagged;" not in text() and "dropped" not in text()
        os.write(master, b"/mux quit\r")
        proc.wait(timeout=5)
        assert proc.returncode == 0
        print("PASS delayed TUI: >1,000 Unicode deltas restored completely, no duplicate text or lag notices")
    finally:
        if stopped: os.kill(proc.pid, signal.SIGCONT)
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(slave)
        os.close(master)


def check_scroll(sock, sid):
    """Long Markdown/thought history must not stall wheel input or typing."""
    rpc(sock, "session/prompt", {"session_id": sid, "text": "scrollprobe", "references": []})
    master, slave = pty.openpty()
    width, height = 120, 40
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
    proc = subprocess.Popen([str(BIN / "agentmux-tui")], stdin=slave, stdout=slave,
                            stderr=slave, env=dict(os.environ, AGENTMUX_SOCK=str(sock), TERM="xterm-256color"))
    screen = pyte.Screen(width, height)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")()
    def text():
        return "\n".join("".join(screen.buffer[y][x].data for x in range(width))
                         for y in range(height))
    def wait_for(check, timeout=8):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.01)[0]:
                stream.feed(decoder.decode(os.read(master, 65536)))
            if check(): return
            assert proc.poll() is None, "TUI exited during scroll"
        raise AssertionError("Scroll timeout:\n" + text())
    try:
        wait_for(lambda: "SCROLL_REPLY_END" in text())
        # Exercise both directions and multiple prefetch boundaries, then type
        # immediately after the wheel burst. Input must not queue behind redraws.
        latencies = []
        for _ in range(3):
            start = time.monotonic()
            os.write(master, b"\x1b[<64;10;10M" * 200 + b"scrollcheck")
            wait_for(lambda: "THOUGHT_ROW_" in text() and "scrollcheck" in text(), timeout=2)
            latencies.append(time.monotonic() - start)
            os.write(master, b"\x7f" * len("scrollcheck"))
            os.write(master, b"\x1b[<65;10;10M" * 200)
            wait_for(lambda: "SCROLL_REPLY_END" in text() and "Back to latest" not in text(), timeout=2)
        # Folding hit targets remain aligned after scrolling to the top.
        # Stop at this block rather than overshooting into older retained turns.
        for _ in range(180):
            if "Thought ·" in text() and "THOUGHT_ROW_0000" in text():
                break
            os.write(master, b"\x1b[<64;10;10M" * 5)
            deadline = time.monotonic() + 0.04
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.005)[0]:
                    stream.feed(decoder.decode(os.read(master, 65536)))
        wait_for(lambda: "Thought ·" in text() and "THOUGHT_ROW_0000" in text())
        thought_row = next(y for y, row in enumerate(screen.display) if "THOUGHT_ROW_0000" in row)
        for y in range(thought_row - 1, -1, -1):
            row = screen.display[y]
            x = row.find("Thought ·")
            if x >= 0:
                os.write(master, f"\x1b[<0;{x+2};{y+1}M".encode())
                break
        wait_for(lambda: "THOUGHT_ROW_" not in text())
        os.write(master, b"/mux quit\r")
        proc.wait(timeout=5)
        assert proc.returncode == 0
        print(f"PASS long history: 1,800 thought rows + code; wheel bursts, typing, return to latest and fold; max wheel-to-input {max(latencies):.3f}s")
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(slave)
        os.close(master)


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-pi-commands-") as tmp:
        base = Path(tmp)
        repo = base / "repo"
        repo.mkdir()
        def git(*args):
            subprocess.run(["git", "-C", str(repo), *args], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        git("init", "-b", "main")
        git("config", "user.email", "test@example.com")
        git("config", "user.name", "Slash Regression")
        (repo / "README.md").write_text("fixture\n")
        git("add", ".")
        git("commit", "-m", "fixture")
        config = base / "config.toml"
        config.write_text('[[agents]]\nid="pi"\nname="Fake Pi"\nkind="pi-rpc"\ncommand="python3"\nargs=['
                          + json.dumps(str(ROOT / "core/tests/fake_pi.py")) + ']\n')
        sock = base / "daemon.sock"
        with (base / "server.log").open("w") as log:
            daemon = subprocess.Popen([str(BIN / "agentmux-server"), "--serve", "--socket", str(sock),
                                       "--data-dir", str(base / "data"), "--config", str(config)], stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while not sock.exists():
                    assert daemon.poll() is None
                    assert time.monotonic() < deadline
                    time.sleep(0.05)
                project = rpc(sock, "project/register", {"root_path": str(repo)})["project"]
                ws = rpc(sock, "workspace/create", {"project_id": project["id"], "name": "test"})["workspace"]
                sid = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "pi"})["session"]["id"]
                primary_sid = sid
                # Multiple agents make the shared-context preamble non-empty.
                rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "pi"})
                for width in [120, 40]:
                    if width == 40:
                        # Resume retains old thoughts; each command check needs a fresh transcript.
                        sid = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "pi"})["session"]["id"]
                    check(sock, sid, width)
                sid = primary_sid
                rpc(sock, "session/kill", {"session_id": sid})
                rpc(sock, "session/resume", {"session_id": sid})
                check_burst(sock, sid)
                rpc(sock, "session/kill", {"session_id": sid})
                rpc(sock, "session/resume", {"session_id": sid})
                check_scroll(sock, sid)
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
