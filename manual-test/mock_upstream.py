#!/usr/bin/env python3
"""A minimal mock OpenAI-Chat SSE upstream used to exercise `ma` end-to-end.

Behavior (driven by inspecting the request messages):
  * If the last user message mentions "safety judge"  -> gate prompt -> {"allow":true}
  * If there is no tool history yet and the first user message starts with a
    mode marker, emit that mode's tool_call:
      - "mock:plan" / "mock:edit" -> the `plan` tool (writes a plan file)
      - "mock:run"               -> the `task` tool (dispatches a sub-agent)
    Otherwise (e.g. a sub-agent's own first turn) -> `bash`.
  * Once any tool history exists (a tool result or an assistant tool_call) ->
    a plain-text final answer (ends the loop).

So `ma -p "mock:plan ..."` writes a plan and prints its path, while
`ma -r <plan> "mock:run ..."` dispatches a sub-agent whose first turn falls
through to the `bash` default.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def sse(payload_obj):
    data = json.dumps(payload_obj)
    return f"data: {data}\n\n".encode()


def tool_call_chunk(name, args_txt):
    return {
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_demo_1",
                    "type": "function",
                    "function": {"name": name, "arguments": args_txt},
                }],
            },
            "finish_reason": "tool_calls",
        }]
    }


def text_chunk(t, finish=None):
    return {"choices": [{"index": 0, "delta": {"content": t}, "finish_reason": finish}]}


def decide(messages):
    # Gate prompt -> allow.
    for m in reversed(messages):
        if m.get("role") == "user":
            c = m.get("content")
            if isinstance(c, str) and "safety judge" in c:
                return [sse(text_chunk('{"allow": true, "reason": "test ok"}', "stop"))]
            break

    first_user = next(
        (m.get("content") for m in messages
         if m.get("role") == "user" and isinstance(m.get("content"), str)),
        "",
    )
    has_tool = any(m.get("role") == "tool" for m in messages) or any(
        m.get("tool_calls") for m in messages if m.get("role") == "assistant"
    )

    # First turn, no tool history yet -> the tool_call requested by the marker.
    # (Run mode wraps the plan in a code block, so match anywhere, not just a LEAD.)
    if not has_tool:
        if "mock:plan" in first_user or "mock:edit" in first_user:
            return [
                sse(tool_call_chunk("plan", json.dumps({"plan": "1. mock step\n2. done"}))),
                sse(text_chunk("", "stop")),
            ]
        if "mock:run" in first_user:
            return [
                sse(tool_call_chunk("task", json.dumps({"task": "run the mock sub-step and report"}))),
                sse(text_chunk("", "stop")),
            ]
        # Default (including a sub-agent's own first turn) -> bash.
        return [
            sse(tool_call_chunk("bash", '{"command": "echo hi from mock"}')),
            sse(text_chunk("", "stop")),
        ]

    # Follow-up with tool history -> plain-text final answer ends the loop.
    return [
        sse(text_chunk("Final answer: the tool ran successfully.", None)),
        sse(text_chunk("", "stop")),
    ]


class H(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        body_txt = json.dumps(body)[:200]
        sys.stderr.write(f"MOCK req: {body_txt}\n")
        for chunk in decide(body.get("messages", [])):
            self.wfile.write(chunk)
            self.wfile.flush()

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    print(f"mock upstream on :{port}", flush=True)
    HTTPServer(("127.0.0.1", port), H).serve_forever()
