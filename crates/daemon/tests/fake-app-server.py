#!/usr/bin/env python3
"""Stand-in for `codex app-server`, used by tests/app_server.rs.

It speaks the newline-delimited JSON-RPC subset flycod uses and nothing
else: no Codex binary, so the driver's handshake, turns, approvals,
interrupt, and manual-compaction paths can run in CI. Extra argv
(`app-server --strict-config`) is
ignored the way a real binary's subcommand would consume it.

The one thing a test can vary is the directory the driver runs it in, which
is the scratch directory the test named: a scratch called `unmounted` gets
an app-server whose flyco MCP server is missing `machine_status`. The driver
gives the app-server no other channel, and a second copy of this file would
be a second copy of the whole protocol.
"""

from __future__ import annotations

import json
import os
import sys


def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def recv() -> dict | None:
    line = sys.stdin.readline()
    if line == "":
        return None
    return json.loads(line)


def main() -> None:
    init = recv()
    if init is None or init.get("method") != "initialize":
        sys.exit(1)
    send({"id": init["id"], "result": {"codexHome": "/tmp", "platform": "linux"}})

    initialized = recv()
    if initialized is None or initialized.get("method") != "initialized":
        sys.exit(1)

    thread = recv()
    if thread is None or thread.get("method") not in ("thread/start", "thread/resume"):
        sys.exit(1)
    send(
        {
            "id": thread["id"],
            "result": {"thread": {"id": "fake-thread", "status": {"type": "idle"}}},
        }
    )

    while True:
        msg = recv()
        if msg is None:
            return
        method = msg.get("method")
        if method == "model/list":
            # The real app-server's own three rows, trimmed to the fields
            # flycod reads, plus a hidden one — the driver has to drop it,
            # and a fixture that never carried one could not show that.
            send(
                {
                    "id": msg["id"],
                    "result": {
                        "data": [
                            {
                                "id": "gpt-5.6-terra",
                                "displayName": "GPT-5.6-Terra",
                                "description": "Balanced agentic coding model for everyday work.",
                                "isDefault": True,
                                "hidden": False,
                                "defaultReasoningEffort": "medium",
                                "supportedReasoningEfforts": [
                                    {"reasoningEffort": "low", "description": "Fast"},
                                    {"reasoningEffort": "medium", "description": "Balanced"},
                                    {"reasoningEffort": "high", "description": "Deeper"},
                                ],
                            },
                            {
                                "id": "gpt-5.6-luna",
                                "displayName": "GPT-5.6-Luna",
                                "description": "Fast and affordable agentic coding model.",
                                "isDefault": False,
                                "hidden": False,
                                "defaultReasoningEffort": "medium",
                                "supportedReasoningEfforts": [
                                    {"reasoningEffort": "low", "description": "Fast"},
                                    {"reasoningEffort": "medium", "description": "Balanced"},
                                ],
                            },
                            {
                                "id": "gpt-5.6-internal",
                                "displayName": "Internal",
                                "description": "Not a model a user is meant to choose.",
                                "isDefault": False,
                                "hidden": True,
                                "defaultReasoningEffort": None,
                                "supportedReasoningEfforts": [],
                            },
                        ],
                        "nextCursor": None,
                    },
                }
            )
        elif method == "account/rateLimits/read":
            # Recorded from codex-cli 0.153.4 on 2026-09-09, plus a
            # `secondary` window the recorded account did not have: the
            # driver reports both, and a fixture with one could not show it.
            send(
                {
                    "id": msg["id"],
                    "result": {
                        "rateLimits": {
                            "limitId": "codex",
                            "limitName": None,
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
                            "credits": {
                                "hasCredits": False,
                                "unlimited": False,
                                "balance": "0",
                            },
                            "individualLimit": None,
                            "spendControlReached": False,
                            "planType": "plus",
                            "rateLimitReachedType": None,
                        }
                    },
                }
            )
        elif method == "mcpServerStatus/list":
            # What the app-server mounted. The driver refuses the session
            # unless flyco's own server is here, connected, with its tools.
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
                                "authStatus": "unsupported",
                                "resources": [],
                                "resourceTemplates": [],
                                "tools": {name: {} for name in tools},
                            }
                        ]
                    },
                }
            )
        elif method == "turn/start":
            send({"id": msg["id"], "result": {}})
            send(
                {
                    "method": "turn/started",
                    "params": {
                        "threadId": "fake-thread",
                        "turn": {"id": "fake-turn"},
                    },
                }
            )
            send(
                {
                    "method": "item/agentMessage/delta",
                    "params": {
                        "delta": "hello",
                        "threadId": "fake-thread",
                        "turnId": "fake-turn",
                    },
                }
            )
            send(
                {
                    "id": 99,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": "fake-thread",
                        "turnId": "fake-turn",
                        "item": {
                            "id": "item-1",
                            "type": "commandExecution",
                            "command": "echo hi",
                        },
                    },
                }
            )
        elif method == "turn/interrupt":
            send({"id": msg["id"], "result": {}})
            send(
                {
                    "method": "turn/completed",
                    "params": {
                        "threadId": "fake-thread",
                        "turn": {"id": "fake-turn", "status": "interrupted"},
                    },
                }
            )
        elif method == "thread/compact/start":
            if msg.get("params") != {"threadId": "fake-thread"}:
                sys.exit(1)
            send({"id": msg["id"], "result": {}})
            send(
                {
                    "method": "item/completed",
                    "params": {
                        "threadId": "fake-thread",
                        "item": {"id": "compact-1", "type": "contextCompaction"},
                    },
                }
            )
        elif "result" in msg and msg.get("id") == 99:
            send(
                {
                    "method": "thread/tokenUsage/updated",
                    "params": {
                        "threadId": "fake-thread",
                        "turnId": "fake-turn",
                        "tokenUsage": {
                            "total": {
                                "inputTokens": 7,
                                "outputTokens": 11,
                                "totalTokens": 18,
                            },
                            "modelContextWindow": 200000,
                        },
                    },
                }
            )
            send(
                {
                    "method": "turn/completed",
                    "params": {
                        "threadId": "fake-thread",
                        "turn": {"id": "fake-turn", "status": "completed"},
                    },
                }
            )
            # Sparse, exactly as the app-server documents it: the window
            # the turn moved, and nothing about the weekly one. The driver
            # has to merge it into the snapshot it already read.
            send(
                {
                    "method": "account/rateLimits/updated",
                    "params": {
                        "rateLimits": {
                            "primary": {
                                "usedPercent": 13,
                                "windowDurationMins": 300,
                                "resetsAt": 1789002000,
                            }
                        }
                    },
                }
            )
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
