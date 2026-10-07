#!/usr/bin/env python3
"""ACP slash discovery through the real daemon/TUI, with isolated state.

Run after cargo build --workspace:
    python3 tui/tests/acp_commands_pty.py
    python3 tui/tests/acp_commands_pty.py --real-opencode

The real-agent check discovers commands and restores sessions without model
calls. It uses isolated HOME/XDG paths and does not read user credentials.
"""
import argparse
import codecs
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import struct
import subprocess
import tempfile
import termios
import time

import pyte
from close_panel_pty import BIN, rpc


def commands_in_history(sock, sid):
    page = rpc(sock, "session/history", {"session_id": sid})
    event = page["available_commands"]
    return [item["name"] for item in event["kind"]["SessionUpdate"]["update"]["availableCommands"]]


def check(sock, sid, width, env, real):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, width, 0, 0))
    proc = subprocess.Popen(
        [str(BIN / "agentmux-tui")], stdin=slave, stdout=slave, stderr=slave,
        env=dict(env, AGENTMUX_SOCK=str(sock), TERM="xterm-256color"),
    )
    screen = pyte.Screen(width, 30)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")()

    def rows():
        return ["".join(screen.buffer[y][x].data or " " for x in range(width)) for y in range(30)]

    def visible(text):
        return any(text in row for row in rows())

    def drain(seconds=0.15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.02)[0]:
                stream.feed(decoder.decode(os.read(master, 65536)))

    def wait(predicate):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
            assert proc.poll() is None, "TUI exited unexpectedly"
        raise AssertionError(f"{width} columns: timeout\n" + "\n".join(rows()))

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

    try:
        wait(lambda: visible("Ask anything"))
        send("/in")
        wait(lambda: visible("/init"))
        send("\t")
        assert visible("/init "), "Tab did not complete the ACP command"
        send("\x7f" * len("/init "))
        send("/revi")
        wait(lambda: visible("/review"))
        if real:
            # No model requests: discovery and safe rejection only.
            send("\x7f" * len("/revi"))
        else:
            send("\tbranch main\r")
            wait(lambda: visible("mock reply: /review branch"))
            send("/in")
            click("› /init")
            wait(lambda: visible("mock reply: /init"))
        send("/models\r")
        wait(lambda: visible("/models"))
        send("\x7f" * len("/models"))
        send("/mux quit\r")
        proc.wait(timeout=5)
        assert proc.returncode == 0
        page = rpc(sock, "session/history", {"session_id": sid})
        prompts = [
            event["kind"].get("SessionUpdate", {}).get("update", {}).get("content", {}).get("text")
            for event in page["events"] if isinstance(event["kind"], dict)
            and event["kind"].get("SessionUpdate", {}).get("update", {}).get("sessionUpdate") == "user_message_chunk"
        ]
        assert "/models" not in prompts, "native-only command became a model prompt"
        if real:
            assert not prompts, "real-agent smoke test must not call a model"
        else:
            assert "/review branch main" in prompts
            assert "/init" in prompts
        print(f"PASS {width} columns: ACP discovery, Tab, native-only draft protection" + ("" if real else ", mouse selection, verbatim command routing"))
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--real-opencode", action="store_true")
    real = parser.parse_args().real_opencode
    binary = shutil.which("opencode") if real else str(BIN / "agentmux-mock-agent")
    assert binary, "opencode is not installed"
    with tempfile.TemporaryDirectory(prefix="agentmux-acp-commands-") as tmp:
        base = Path(tmp)
        repo = base / "repo"
        repo.mkdir()
        for args in [["init", "-b", "main"], ["config", "user.email", "test@example.com"],
                     ["config", "user.name", "ACP Regression"], ["commit", "--allow-empty", "-m", "fixture"]]:
            subprocess.run(["git", *args], cwd=repo, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        # Keep the executable name so the mock also exercises OpenCode defaults.
        command = base / "opencode"
        command.symlink_to(binary)
        env = {
            "PATH": os.environ["PATH"], "HOME": str(base / "home"),
            "XDG_CONFIG_HOME": str(base / "config"), "XDG_DATA_HOME": str(base / "xdg-data"),
            "XDG_CACHE_HOME": str(base / "cache"), "XDG_STATE_HOME": str(base / "state"),
            "OPENCODE_DISABLE_AUTOUPDATE": "true", "OPENCODE_DISABLE_MODELS_FETCH": "true",
        }
        config = base / "agents.toml"
        config.write_text(
            '[[agents]]\nid="opencode"\nname="OpenCode"\nkind="acp"\ncommand='
            + json.dumps(str(command)) + '\nargs=["acp"]\n'
            + ('' if real else '[agents.env]\nMOCK_COMMANDS="init,review"\n')
        )
        sock = base / "daemon.sock"
        with (base / "server.log").open("w") as log:
            daemon = subprocess.Popen(
                [str(BIN / "agentmux-server"), "--serve", "--socket", str(sock),
                 "--data-dir", str(base / "data"), "--config", str(config)],
                env=env, stdout=log, stderr=log,
            )
            try:
                deadline = time.monotonic() + 20
                while not sock.exists():
                    assert daemon.poll() is None
                    assert time.monotonic() < deadline
                    time.sleep(0.05)
                project = rpc(sock, "project/register", {"root_path": str(repo)})["project"]
                ws = rpc(sock, "workspace/create", {"project_id": project["id"], "name": "test"})["workspace"]
                sid = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "opencode"})["session"]["id"]
                deadline = time.monotonic() + 10
                while True:
                    page = rpc(sock, "session/history", {"session_id": sid})
                    if page.get("available_commands"):
                        break
                    assert time.monotonic() < deadline
                    time.sleep(0.05)
                assert {"init", "review"} <= set(commands_in_history(sock, sid))
                if not real:
                    # Put the initial command catalog outside the latest page.
                    for i in range(70):
                        rpc(sock, "session/prompt", {"session_id": sid, "text": f"history {i}", "references": []})
                    rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "opencode"})
                    assert not any(
                        event["kind"].get("SessionUpdate", {}).get("update", {}).get("sessionUpdate") == "available_commands_update"
                        for event in rpc(sock, "session/history", {"session_id": sid})["events"]
                        if isinstance(event["kind"], dict)
                    )
                check(sock, sid, 120, env, real)
                rpc(sock, "session/kill", {"session_id": sid})
                rpc(sock, "session/resume", {"session_id": sid})
                assert {"init", "review"} <= set(commands_in_history(sock, sid))
                check(sock, sid, 40, env, real)
            finally:
                daemon.terminate()
                daemon.wait(timeout=10)


if __name__ == "__main__":
    main()
