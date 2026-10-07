#!/usr/bin/env python3
"""U04-U08 terminal workflows with isolated agents and deterministic editor/clipboard."""
import json
from pathlib import Path
import shlex
import subprocess
import tempfile
import time

from close_panel_pty import BIN, rpc
from multi_agent_pty import Terminal, prompts


def menu(terminal, label):
    terminal.click(" Menu ")
    terminal.send("\x1b[H")
    for _ in range(40):
        if terminal.visible("│ " + label):
            terminal.click("│ " + label)
            return
        terminal.send("\x1b[B")
    raise AssertionError("Menu item unavailable: " + label)


def clean(terminal):
    for _ in range(5):
        terminal.send("\x1b")
    terminal.send("\x1bOQ")  # F2 returns to the composer.
    terminal.send("\x1b[F")
    terminal.send("\x7f" * 500)


def check(sock, project, base, width, height):
    ws = rpc(sock, "workspace/create", {"project_id": project, "name": f"features{width}"})["workspace"]
    root = Path(ws["worktree_path"])
    ids = []
    for title in [f"Source {width}", f"Receiver {width}"]:
        sid = rpc(sock, "session/create", {"workspace_id": ws["id"], "agent_id": "mock"})["session"]["id"]
        rpc(sock, "session/title", {"session_id": sid, "title": title})
        ids.append(sid)
    source = f"搜索文本_{width}\n```rust\n    中文代码();\n```\n搜索结束_{width}\n"
    rpc(sock, "session/prompt", {"session_id": ids[0], "text": source})
    control = base / f"editor-control-{width}.json"
    clip = base / f"clipboard-{width}.txt"
    clip_control = base / f"clipboard-control-{width}.json"
    clip_control.write_text(json.dumps({"fail": False}))
    control.write_text(json.dumps({"exit": 0, "suffix": f"\nEDITOR_SUCCESS_{width}"}))
    editor = " ".join(shlex.quote(str(arg)) for arg in ["python3", base / "editor helper.py", control])
    clipboard = " ".join(shlex.quote(str(arg)) for arg in ["python3", base / "clipboard helper.py", clip, clip_control])
    terminal = Terminal(sock, width, height, {"VISUAL": editor, "AGENTMUX_CLIPBOARD_COMMAND": clipboard, "TMPDIR": str(base)})
    draft = f"draft_{width}_保留"
    context_area = (max(0, (width - 96) // 2), 1, min(width, 96), height - 2)
    try:
        terminal.wait(lambda: terminal.visible(" Help "))
        terminal.choose_conversation(f"Source {width}")
        terminal.paste(draft)
        # All direct-focus keys retain the recipient and existing draft.
        terminal.send("\x1bOQ")
        terminal.send("\x1bOR")
        terminal.send("\x1bOS")
        terminal.send("\x1bOQ")
        terminal.wait(lambda: terminal.visible(draft))

        menu(terminal, "Search conversation")
        terminal.paste(f"搜索文本_{width}")
        terminal.wait(lambda: terminal.visible("You ·"))
        terminal.send("\r")
        terminal.wait(lambda: terminal.visible(" Copy "))
        terminal.click(" Copy ")
        terminal.wait(lambda: clip.exists())
        assert clip.read_text() == source, "message copy lost raw content or added UI decoration"
        terminal.click(" Copy code ")
        terminal.wait(lambda: clip.read_text() == "    中文代码();\n")
        previous = set(base.glob("agentmux-copy-*"))
        clip_control.write_text(json.dumps({"fail": True}))
        terminal.click(" Copy ")
        terminal.wait(lambda: bool(set(base.glob("agentmux-copy-*")) - previous))
        saved = next(iter(set(base.glob("agentmux-copy-*")) - previous))
        assert saved.read_text() == source, "clipboard fallback lost the original message"
        assert saved.stat().st_mode & 0o777 == 0o600, "copy fallback is not private"
        clip_control.write_text(json.dumps({"fail": False}))
        terminal.send("\x1b")
        terminal.wait(lambda: terminal.visible(draft))

        menu(terminal, "Edit draft externally")
        terminal.wait(lambda: terminal.visible(f"EDITOR_SUCCESS_{width}"))
        assert draft not in prompts(sock, ids[0]), "editor automatically sent the draft"
        control.write_text(json.dumps({"exit": 1, "suffix": "MUST_NOT_APPLY"}))
        menu(terminal, "Edit draft externally")
        terminal.wait(lambda: terminal.visible("Editor cancelled"))
        assert not terminal.visible("MUST_NOT_APPLY"), "failed editor replaced the draft"

        control.write_text(json.dumps({"exit": 0, "suffix": f"\nCONTEXT_EDIT_{width}"}))
        menu(terminal, "Workspace context")
        if width >= 110:
            terminal.click(" Open ")
        terminal.wait(lambda: terminal.visible("Workspace context"))
        terminal.wait(lambda: terminal.visible(" Edit "))
        terminal.click(" Edit ", context_area)
        terminal.wait(lambda: terminal.visible(f"CONTEXT_EDIT_{width}") or terminal.visible("Unsaved context edit"))
        terminal.click(" Save ", context_area)
        terminal.wait(lambda: f"CONTEXT_EDIT_{width}" in (root / ".agentmux/context.md").read_text())
        assert terminal.visible("Workspace context"), "context save stole focus"
        terminal.click(" Edit ", context_area)
        terminal.wait(lambda: terminal.visible("Unsaved context edit"))
        changed = "CONCURRENT_OTHER_EDIT\n"
        (root / ".agentmux/context.md").write_text(changed)
        terminal.click(" Save ", context_area)
        terminal.wait(lambda: terminal.visible("changed while editing") or terminal.visible("changed;"))
        assert (root / ".agentmux/context.md").read_text() == changed, "context conflict overwrote external changes"
        terminal.send("\x1b")
        terminal.send("\x1bOQ")
        terminal.wait(lambda: terminal.visible(f"EDITOR_SUCCESS_{width}"))

        menu(terminal, "Agent details")
        terminal.wait(lambda: terminal.visible("Structured ACP"))
        assert terminal.visible("Unknown"), "missing model/cost was fabricated"
        terminal.send("\x1b")

        # A quote is only staged in the recipient draft and can be inspected/removed.
        menu(terminal, "Quote event to another agent")
        terminal.send("\r")
        terminal.send("\x1b[B\r")
        terminal.wait(lambda: terminal.visible("refs"))
        menu(terminal, "Draft references")
        terminal.wait(lambda: terminal.visible("Draft references"))
        terminal.wait(lambda: terminal.visible(" Remove "))
        terminal.click(" Remove ")
        terminal.wait(lambda: terminal.visible("No references attached"))
        terminal.send("\x1b")
        assert not prompts(sock, ids[1]), "quote preview or removal broadcast a message"
        terminal.choose_conversation(f"Source {width}")
        terminal.wait(lambda: terminal.visible(f"EDITOR_SUCCESS_{width}"))
        assert draft not in prompts(sock, ids[0])
        clean(terminal)
        print(f"PASS {width}x{height}: focus keys, Unicode search, exact message/code clipboard, editor success/failure, context save/conflict, unknown metadata, quote review/removal, drafts, raw-mode restoration")
    finally:
        if terminal.proc.poll() is None:
            clean(terminal)
        terminal.close()
        for sid in ids:
            rpc(sock, "session/kill", {"session_id": sid})


def main():
    with tempfile.TemporaryDirectory(prefix="agentmux-features-") as temp:
        base = Path(temp)
        # These are generated test fixtures, not project runtime commands.
        (base / "editor helper.py").write_text("import json,sys,termios\nfrom pathlib import Path\nc=json.loads(Path(sys.argv[1]).read_text())\np=Path(sys.argv[2])\nf=termios.tcgetattr(0)\nassert f[3] & termios.ICANON, 'editor received raw terminal'\np.write_text(p.read_text()+c['suffix'])\nsys.exit(c['exit'])\n")
        (base / "clipboard helper.py").write_text("import json,sys\nfrom pathlib import Path\nif json.loads(Path(sys.argv[2]).read_text())['fail']: sys.exit(1)\nPath(sys.argv[1]).write_bytes(sys.stdin.buffer.read())\n")
        repo = base / "repo"
        repo.mkdir()
        for args in [("init", "-b", "main"), ("config", "user.email", "test@example.com"), ("config", "user.name", "Feature Test")]:
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
                    assert daemon.poll() is None
                    assert time.monotonic() < deadline
                    time.sleep(0.05)
                project = rpc(sock, "project/register", {"root_path": str(repo)})["project"]
                for width, height in [(40, 16), (80, 24), (120, 30), (160, 40)]:
                    check(sock, project["id"], base, width, height)
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
