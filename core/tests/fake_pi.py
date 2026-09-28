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
- prompt containing `hang`    → never responds (request stays pending)
- `abort`       → ends a `slow` run (`agent_end` + `agent_settled`), then
                  a success response
"""

import json
import os
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


def emit_run(message):
    """Emit a complete agent run for `message`, in the documented order."""
    reply = "fake pi reply: " + message
    send({"type": "agent_start"})
    send({"type": "turn_start"})
    send({"type": "message_start", "message": {"role": "assistant", "content": []}})
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
            if "crash" in msg:
                os._exit(1)
            if "exit42" in msg:
                os._exit(42)
            if "hang" in msg:
                continue  # blackhole: no response, the request stays pending
            if "reject" in msg:
                respond(req, "prompt", success=False,
                        error="prompt rejected: agent is streaming")
                continue
            if "handled" in msg:
                respond(req, "prompt", data={"disposition": "handled"})
                continue
            if "slow" in msg:
                respond(req, "prompt", data={"disposition": "started"})
                send({"type": "agent_start"})
                waiting_abort = True
                continue
            if "burst" in msg:
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
