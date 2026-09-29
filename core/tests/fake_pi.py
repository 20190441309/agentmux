#!/usr/bin/env python3
"""Fake `pi --mode rpc` for agentmux PiConn integration tests.

Deterministic JSONL stub — no real `pi` binary (or API key) required.
Reads command records from stdin (one JSON object per LF-terminated line)
and writes responses/events to stdout, following the documented protocol:

- commands:  {"id": "...", "type": "<command>", ...}
- responses: {"id": "...", "type": "response", "command": "<command>",
              "success": bool, "data"?: ..., "error"?: str}
- events:    {"type": "<event kind>", ...} with no `id`

Contract exercised by `pi_rpc_test.rs`:

- `get_state`   → success response, `data.sessionId == "pi-session-1"`
- `new_session` → success response, `data.cancelled == false`
- `prompt`      → `data.disposition == "started"` response, then a full
                  event run ending in `agent_settled`. The `text_delta`
                  echoes the prompt text verbatim (ensure_ascii=False), so
                  U+2028/U+2029 bytes in the prompt come back raw — the
                  documented framing trap the reader must survive.
- prompt containing `crash`   → process exits 1
- prompt containing `exit42`  → process exits 42
- prompt containing `reject`  → `success:false` error response
- prompt containing `handled` → `disposition:"handled"`, no event run
- prompt containing `slow`    → `disposition:"started"` + `agent_start`,
                                finishing only when `abort` arrives
- prompt containing `burst`   → `disposition:"started"`, then BURST_COUNT
                                `message_update` records in one go, then
                                `agent_settled` — exercises the reader
                                under a >broadcast-capacity event burst
- prompt containing `tool`    → additionally emits a `tool_execution_`
                                lifecycle for an `edit` tool on
                                `src/edited.rs` (start → update → end,
                                `isError:false`), exercising the
                                toolCallId → FileEdited translation
- prompt containing `toolfail`→ same lifecycle but `isError:true` —
                                failed edits must NOT yield FileEdited
- prompt containing `hang`    → never responds (request stays pending)

Trigger words match on WHOLE alphanumeric words (like mock-agent's
`has_trigger`), not substrings — otherwise real prompt text trips them:
the shared-context preamble's "changed" contains "hang", "tools" would
match `tool`, etc.
- `abort`       → ends a `slow` run (`agent_end` + `agent_settled`), then
                  a success response
"""

import json
import os
import re
import sys

sys.stdin.reconfigure(encoding="utf-8")
sys.stdout.reconfigure(encoding="utf-8")

SESSION_ID = "pi-session-1"

STATE = {
    "sessionId": SESSION_ID,
    "sessionName": "fake-pi",
    "sessionFile": "/tmp/fake-pi-session.jsonl",
    "isStreaming": False,
    "isCompacting": False,
    "thinkingLevel": "medium",
    "steeringMode": "all",
    "followUpMode": "one-at-a-time",
    "autoCompactionEnabled": True,
    "messageCount": 0,
    "pendingMessageCount": 0,
}

# True while a `slow` prompt run is waiting for `abort`.
waiting_abort = False

# Event lines emitted by a `burst` prompt — deliberately beyond the 256
# records a 256-cap broadcast ring can retain.
BURST_COUNT = 300


def send(obj):
    """Write one JSONL record. ensure_ascii=False keeps U+2028/U+2029 raw,
    reproducing the real agent's framing edge case."""
    sys.stdout.buffer.write((json.dumps(obj, ensure_ascii=False) + "\n").encode("utf-8"))
    sys.stdout.buffer.flush()


def respond(req, command, success=True, data=None, error=None):
    rec = {"type": "response", "command": command, "success": success}
    if "id" in req:
        rec["id"] = req["id"]
    if data is not None:
        rec["data"] = data
    if error is not None:
        rec["error"] = error
    send(rec)


def triggered(msg, word):
    """Whole-word trigger match — same semantics as mock-agent's
    `has_trigger` (split on non-alphanumerics, compare words)."""
    return word in re.findall(r"[a-zA-Z0-9]+", msg)


def emit_tool_run(fail=False):
    """Emit one `edit`-tool execution lifecycle, as pi does between a
    tool call and the assistant's next message. `end` carries no `args`
    (only the correlating `toolCallId`) — the documented pi shape."""
    call = "toolcall-1"
    send({
        "type": "tool_execution_start",
        "toolCallId": call,
        "toolName": "edit",
        "args": {"path": "src/edited.rs", "oldText": "a", "newText": "b"},
    })
    send({
        "type": "tool_execution_update",
        "toolCallId": call,
        "toolName": "edit",
        "args": {"path": "src/edited.rs"},
        "partialResult": {"content": [{"type": "text", "text": "edited"}]},
    })
    send({
        "type": "tool_execution_end",
        "toolCallId": call,
        "toolName": "edit",
        "result": {"content": [{"type": "text", "text": "ok"}]},
        "isError": fail,
    })


def emit_run(message):
    """Emit a complete agent run for `message`, in the documented order."""
    reply = "fake pi reply: " + message
    send({"type": "agent_start"})
    send({"type": "turn_start"})
    send({"type": "message_start", "message": {"role": "assistant", "content": []}})
    if triggered(message, "toolfail"):
        emit_tool_run(fail=True)
    elif triggered(message, "tool"):
        emit_tool_run()
    send({
        "type": "message_update",
        "usage": {},
        "assistantMessageEvent": {
            "type": "text_delta",
            "contentIndex": 0,
            "delta": reply,
        },
    })
    send({
        "type": "message_end",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": reply}],
        },
    })
    send({"type": "turn_end"})
    send({"type": "agent_end", "messages": [], "willRetry": False})
    send({"type": "agent_settled"})


def main():
    global waiting_abort
    for raw in sys.stdin:
        try:
            req = json.loads(raw)
        except ValueError:
            continue  # tolerate torn/garbage lines like a real agent would
        ctype = req.get("type")

        if ctype == "get_state":
            respond(req, "get_state", data=dict(STATE))
        elif ctype == "new_session":
            respond(req, "new_session", data={"cancelled": False})
        elif ctype == "abort":
            if waiting_abort:
                waiting_abort = False
                send({"type": "agent_end", "messages": [], "willRetry": False})
                send({"type": "agent_settled"})
            respond(req, "abort")
        elif ctype == "prompt":
            msg = req.get("message", "")
            if triggered(msg, "crash"):
                os._exit(1)
            if triggered(msg, "exit42"):
                os._exit(42)
            if triggered(msg, "hang"):
                continue  # blackhole: no response, the request stays pending
            if triggered(msg, "reject"):
                respond(req, "prompt", success=False,
                        error="prompt rejected: agent is streaming")
                continue
            if triggered(msg, "handled"):
                respond(req, "prompt", data={"disposition": "handled"})
                continue
            if triggered(msg, "slow"):
                respond(req, "prompt", data={"disposition": "started"})
                send({"type": "agent_start"})
                waiting_abort = True
                continue
            if triggered(msg, "burst"):
                respond(req, "prompt", data={"disposition": "started"})
                send({"type": "agent_start"})
                for i in range(BURST_COUNT):
                    send({
                        "type": "message_update",
                        "usage": {},
                        "assistantMessageEvent": {
                            "type": "text_delta",
                            "contentIndex": 0,
                            "delta": f"burst chunk {i}",
                        },
                    })
                send({"type": "agent_end", "messages": [], "willRetry": False})
                send({"type": "agent_settled"})
                continue
            respond(req, "prompt", data={"disposition": "started"})
            emit_run(msg)
        # Unknown commands are ignored, like a protocol extension the stub
        # doesn't implement — the client's request simply stays pending.


main()
