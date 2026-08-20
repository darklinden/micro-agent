#!/usr/bin/env python3
"""A minimal mock OpenAI-Chat SSE upstream used to exercise `ma` end-to-end.

Behavior (driven by inspecting the request messages):
  * If the last user message mentions "safety judge"  -> gate prompt -> {"allow":true}
  * Else if the history already contains a tool result / assistant tool_calls
      -> the follow-up turn -> emit a plain-text final answer.
  * Else (first turn) -> emit one tool_call for `bash` ("echo hi from mock").
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
    # Gate prompt?
    for m in reversed(messages):
        if m.get("role") == "user":
            c = m.get("content")
            if isinstance(c, str) and "safety judge" in c:
                return [sse(text_chunk('{"allow": true, "reason": "test ok"}', "stop"))]
            break
    # Follow-up with tool history?
    has_tool = any(m.get("role") == "tool" for m in messages) or any(
        m.get("tool_calls") for m in messages if m.get("role") == "assistant"
    )
    if has_tool:
        return [
            sse(text_chunk("Final answer: the tool ran successfully.", None)),
            sse(text_chunk("", "stop")),
        ]
    # First turn -> tool call.
    return [
        sse(tool_call_chunk("bash", '{"command": "echo hi from mock"}')),
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
