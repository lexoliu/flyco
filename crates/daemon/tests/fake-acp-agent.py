#!/usr/bin/env python3
"""Stand-in for an ACP agent, used by tests/acp.rs.

It speaks the newline-delimited ACP subset flycod uses and nothing else:
`initialize`, `session/new`, `session/set_mode`, `session/set_config_option`,
`session/prompt`, `session/cancel`, `session/close`, plus the three extension
methods the test configuration names (`mcpServerStatus/list`,
`account/rateLimits/read`, `thread/compact/start`). The point is the driver,
not a vendor: handshake, session open, a turn with a permission request,
usage reads, interrupt, and manual compaction all run in CI without any
agent binary.

The one thing a test can vary is the directory the driver runs it in, which
is the scratch directory the test named: a scratch called `unmounted` gets
a mount report whose flyco server is missing `machine_status`, and one
called `skillsreload` pushes an `available_commands_update` when a turn
opens, so the driver's palette re-announcement can be watched. For the
continuation fallback, `refuseload` offers `session/load` as the only
resume path and then rejects it, while `continuless` speaks neither
`session/resume` nor `session/load` at all.
"""

from __future__ import annotations

import json
import os
import sys

SESSION = "fake-session"

# The model/effort options the session reports, and the answer
# `session/set_config_option` echoes back with `currentValue` moved.
CONFIG_OPTIONS = [
    {
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": "gpt-5.6-terra",
        "options": [
            {
                "value": "gpt-5.6-terra",
                "name": "GPT-5.6-Terra",
                "description": "Balanced agentic coding model for everyday work.",
            },
            {
                "value": "gpt-5.6-luna",
                "name": "GPT-5.6-Luna",
                "description": "Fast and affordable agentic coding model.",
            },
        ],
    },
    {
        "id": "reasoning_effort",
        "name": "Reasoning effort",
        "category": "thought_level",
        "type": "select",
        "currentValue": "medium",
        "options": [
            {"value": "low", "name": "Low"},
            {"value": "medium", "name": "Medium"},
            {"value": "high", "name": "High"},
        ],
    },
]

MODES = {
    "currentModeId": "agent",
    "availableModes": [
        {"id": "read-only", "name": "Read only"},
        {"id": "agent", "name": "Agent"},
        {"id": "agent-full-access", "name": "Agent (full access)"},
    ],
}

# Readings of the plan's meters; each `account/rateLimits/read` consumes
# one. A turn is the only thing that moves the five-hour window, so the
# second answer — the one the driver asks for after turn one — has spent
# it: the reading itself goes nowhere (the control plane reads the plan
# from the vendor), and what the driver must still do with it is announce
# the limit that stops the session.
USAGE_READINGS = [
    {
        "rateLimits": {
            "primary": {
                "usedPercent": 12,
                "windowDurationMins": 300,
                "resetsAt": 1789002000,
            },
            "secondary": {
                "usedPercent": 40,
                "windowDurationMins": 10080,
                "resetsAt": 1789570800,
            },
        }
    },
    {
        "rateLimits": {
            "primary": {
                "usedPercent": 100,
                "windowDurationMins": 300,
                "resetsAt": 1789002000,
            },
            "secondary": {
                "usedPercent": 40,
                "windowDurationMins": 10080,
                "resetsAt": 1789570800,
            },
        }
    },
]


def send(obj: dict) -> None:
    obj.setdefault("jsonrpc", "2.0")
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def recv() -> dict | None:
    line = sys.stdin.readline()
    if line == "":
        return None
    return json.loads(line)


def update(payload: dict) -> None:
    send(
        {
            "method": "session/update",
            "params": {"sessionId": SESSION, "update": payload},
        }
    )


def main() -> None:
    readings = list(USAGE_READINGS)
    # The `session/prompt` request id while a turn is in flight; its answer
    # is sent when the turn ends, not when the request arrives.
    prompt_id = None

    while True:
        msg = recv()
        if msg is None:
            return
        method = msg.get("method")

        if method == "initialize":
            capabilities = {
                "loadSession": True,
                "mcpCapabilities": {"http": True},
                "sessionCapabilities": {"resume": {}, "close": {}},
            }
            if "refuseload" in os.getcwd():
                # Load is the only continuation offered, and it fails.
                del capabilities["sessionCapabilities"]["resume"]
            elif "continuless" in os.getcwd():
                capabilities["loadSession"] = False
                del capabilities["sessionCapabilities"]["resume"]
            send(
                {
                    "id": msg["id"],
                    "result": {
                        "protocolVersion": 1,
                        "agentInfo": {"name": "fake-acp", "version": "0.1.0"},
                        "agentCapabilities": capabilities,
                    },
                }
            )
        elif method == "session/new":
            send(
                {
                    "id": msg["id"],
                    "result": {
                        "sessionId": SESSION,
                        "modes": MODES,
                        "configOptions": CONFIG_OPTIONS,
                        # Extension field, flattened into the result: the
                        # palette the session opens with.
                        "availableCommands": [
                            {
                                "name": "cloudflare",
                                "description": (
                                    "Comprehensive Cloudflare platform skill "
                                    "covering Workers, Pages, storage, and AI."
                                ),
                            }
                        ],
                    },
                }
            )
        elif method == "session/resume":
            send(
                {
                    "id": msg["id"],
                    "result": {"modes": MODES, "configOptions": CONFIG_OPTIONS},
                }
            )
        elif method == "session/load":
            if "refuseload" in os.getcwd():
                send(
                    {
                        "id": msg["id"],
                        "error": {"code": -32602, "message": "session is gone"},
                    }
                )
            else:
                send(
                    {
                        "id": msg["id"],
                        "result": {"modes": MODES, "configOptions": CONFIG_OPTIONS},
                    }
                )
        elif method == "session/set_mode":
            send({"id": msg["id"], "result": {}})
        elif method == "session/set_config_option":
            params = msg.get("params", {})
            options = json.loads(json.dumps(CONFIG_OPTIONS))
            for option in options:
                if option["id"] == params.get("configId"):
                    option["currentValue"] = params.get("value")
            send({"id": msg["id"], "result": {"configOptions": options}})
        elif method == "mcpServerStatus/list":
            # What the agent mounted. The driver refuses the session unless
            # flyco's own server is here, connected, with its tools.
            tools = ["budget_status", "machine_resize"]
            if "unmounted" not in os.getcwd():
                tools.append("machine_status")
            send(
                {
                    "id": msg["id"],
                    "result": {
                        "data": [
                            {
                                "name": "flyco",
                                "runtimeStatus": "connected",
                                "tools": {name: {} for name in tools},
                            }
                        ],
                        "nextCursor": None,
                    },
                }
            )
        elif method == "account/rateLimits/read":
            send({"id": msg["id"], "result": readings.pop(0) if readings else USAGE_READINGS[-1]})
        elif method == "thread/compact/start":
            if msg.get("params") != {"threadId": SESSION}:
                sys.exit(1)
            send({"id": msg["id"], "result": {}})
        elif method == "session/prompt":
            prompt_id = msg["id"]
            if "skillsreload" in os.getcwd():
                # A skill appeared while the session was open; the palette
                # update is how the driver learns the set moved.
                update(
                    {
                        "sessionUpdate": "available_commands_update",
                        "availableCommands": [
                            {
                                "name": "cloudflare",
                                "description": (
                                    "Comprehensive Cloudflare platform skill "
                                    "covering Workers, Pages, storage, and AI."
                                ),
                            },
                            {
                                "name": "release",
                                "description": "Cut a release of this crate.",
                                "input": {"hint": "version"},
                            },
                        ],
                    }
                )
            update(
                {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hello"},
                }
            )
            # The tool call the turn is blocked on: a permission request is
            # an agent-to-client request, so the turn stays open until the
            # answer comes back.
            send(
                {
                    "id": 99,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": SESSION,
                        "toolCall": {
                            "toolCallId": "call-1",
                            "title": "",
                            "kind": "execute",
                            "status": "pending",
                            "rawInput": {"command": "echo hi"},
                        },
                        "options": [
                            {
                                "optionId": "allow-1",
                                "name": "Allow once",
                                "kind": "allow_once",
                            },
                            {
                                "optionId": "deny-1",
                                "name": "Reject",
                                "kind": "reject_once",
                            },
                        ],
                    },
                }
            )
        elif method == "session/cancel":
            if prompt_id is not None:
                send({"id": prompt_id, "result": {"stopReason": "cancelled"}})
                prompt_id = None
        elif method == "session/close":
            send({"id": msg["id"], "result": {}})
            return
        elif "result" in msg and msg.get("id") == 99:
            # The permission answer: allow or deny, either way the turn
            # completes. The window fill lands first so the completion
            # carries it.
            update(
                {
                    "sessionUpdate": "usage_update",
                    "used": 18,
                    "size": 200000,
                }
            )
            send({"id": prompt_id, "result": {"stopReason": "end_turn"}})
            prompt_id = None
        elif method is None:
            continue
        else:
            send(
                {
                    "id": msg.get("id", 0),
                    "error": {"code": -32601, "message": f"unknown {method}"},
                }
            )


if __name__ == "__main__":
    main()
