#!/usr/bin/env python3
"""Stand-in for `codex app-server`, used by tests/app_server.rs.

It speaks the newline-delimited JSON-RPC subset flycod uses and nothing
else: no Codex binary, so the driver's handshake, turns, approvals,
interrupt, and manual-compaction paths can run in CI. Extra argv
(`app-server --strict-config`) is
ignored the way a real binary's subcommand would consume it.
"""

from __future__ import annotations

import json
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
        if method == "turn/start":
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
