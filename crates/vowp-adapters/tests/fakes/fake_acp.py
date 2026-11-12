#!/usr/bin/env python3
"""Fake Agent Client Protocol server for the vowp-adapters tests.

Speaks the pinned first-slice subset (JSON-RPC 2.0 over stdin/stdout) and never calls a
model or the network. See docs/design/adapters.md §10.

Environment:
  FAKE_ACP_MODE   ok | empty | no-result | malformed | unknown-method | permission | quota | hang
  FAKE_ACP_LOG    file to which every client message (and argv) is appended as JSONL
  FAKE_ACP_CHILD_PID  file to which a spawned child pid is written (hang mode)
"""

import json
import os
import subprocess
import sys
import time

MODE = os.environ.get("FAKE_ACP_MODE", "ok")
LOG = os.environ.get("FAKE_ACP_LOG")
CHILD_PID = os.environ.get("FAKE_ACP_CHILD_PID")
SESSION_ID = "sess_fake_acp_1"
PROMPT_ID = None


def log(entry):
    if LOG:
        with open(LOG, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def respond(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def rpc_error(request_id, code, message):
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def update(payload):
    notify("session/update", {"sessionId": SESSION_ID, "update": payload})


def read_message():
    line = sys.stdin.readline()
    if not line:
        return None
    try:
        return json.loads(line)
    except json.JSONDecodeError:
        log({"unparsed_client_line": line.rstrip("\n")})
        return {}


def status_turn():
    """The clean turn: message chunks, two tool calls, a plan and token usage."""
    update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello "}})
    update(
        {
            "sessionUpdate": "tool_call",
            "toolCallId": "tool-read-1",
            "title": "Read README.md",
            "kind": "read",
            "status": "pending",
            "rawInput": {"path": "README.md"},
        }
    )
    update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "from ACP."}})
    update(
        {
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-read-1",
            "status": "in_progress",
        }
    )
    update(
        {
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tool-read-1",
            "status": "completed",
            "rawOutput": "readme contents",
        }
    )
    update(
        {
            "sessionUpdate": "tool_call",
            "toolCallId": "tool-edit-2",
            "title": "Edit src/lib.rs",
            "kind": "edit",
            "status": "completed",
            "rawInput": {"path": "src/lib.rs"},
            "rawOutput": "edited",
            "locations": [{"path": "src/lib.rs", "line": 1}],
        }
    )
    update(
        {
            "sessionUpdate": "plan",
            "entries": [
                {"content": "read the file", "priority": "high", "status": "completed"},
                {"content": "edit the file", "priority": "medium", "status": "in_progress"},
            ],
        }
    )
    respond(
        PROMPT_ID,
        {
            "stopReason": "end_turn",
            "usage": {
                "totalTokens": 150,
                "inputTokens": 120,
                "outputTokens": 30,
                "cachedReadTokens": 4,
            },
        },
    )
    return 0


def permission_turn():
    update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "before permission"}})
    send(
        {
            "jsonrpc": "2.0",
            "id": 77,
            "method": "session/request_permission",
            "params": {
                "sessionId": SESSION_ID,
                "toolCall": {
                    "toolCallId": "tool-edit-1",
                    "title": "Write a file",
                    "kind": "edit",
                    "status": "pending",
                    "rawInput": {"path": "src/lib.rs"},
                },
                "options": [
                    {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"},
                ],
            },
        }
    )
    response = read_message()
    log({"response": response})
    outcome = "cancelled"
    if isinstance(response, dict):
        result = response.get("result")
        if isinstance(result, dict):
            outcome = result.get("outcome", {}).get("outcome", "cancelled")
    respond(PROMPT_ID, {"stopReason": "cancelled" if outcome == "cancelled" else "end_turn"})
    return 0


def hang_turn():
    update(
        {
            "sessionUpdate": "tool_call",
            "toolCallId": "tool-hang-1",
            "title": "Hang",
            "kind": "read",
            "status": "pending",
        }
    )
    child = subprocess.Popen(["sleep", "30"])
    if CHILD_PID:
        with open(CHILD_PID, "w", encoding="utf-8") as handle:
            handle.write(str(child.pid))
    while True:
        time.sleep(1)


def main():
    global PROMPT_ID
    log({"argv": sys.argv})
    if MODE == "empty":
        return 0
    while True:
        message = read_message()
        if message is None:
            return 0
        log(message)
        method = message.get("method")
        if method == "initialize":
            respond(
                message.get("id"),
                {
                    "protocolVersion": 1,
                    "agentCapabilities": {},
                    "authMethods": [],
                    "agentInfo": {"name": "fake-acp", "version": "0.0.0"},
                },
            )
        elif method == "session/new":
            respond(message.get("id"), {"sessionId": SESSION_ID})
        elif method == "session/prompt":
            PROMPT_ID = message.get("id")
            if MODE == "no-result":
                update(
                    {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "partial"},
                    }
                )
                return 0
            if MODE == "malformed":
                sys.stdout.write("this is not json\n")
                sys.stdout.flush()
                return 0
            if MODE == "unknown-method":
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": 900,
                        "method": "session/unsupported_thing",
                        "params": {"sessionId": SESSION_ID},
                    }
                )
                log({"response": read_message()})
                return 0
            if MODE == "permission":
                return permission_turn()
            if MODE == "quota":
                rpc_error(
                    message.get("id"),
                    -32000,
                    "usage limit reached|1794164400",
                )
                return 1
            if MODE == "hang":
                return hang_turn()
            return status_turn()


if __name__ == "__main__":
    sys.exit(main())
