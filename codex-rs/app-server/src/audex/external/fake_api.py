#!/usr/bin/python3
"""Pinned-client offline interoperability fixture; never retain raw request/stream bodies."""
import json
from pathlib import Path
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = Path(sys.argv[1])
fixture = json.loads((root / "api-fixture.json").read_text())
requests = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        assert int(self.headers["content-length"]) <= 1024 * 1024
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        assert self.path.split("?")[0] == "/v1/messages"
        assert self.headers.get("x-api-key") == "dummy-c63-offline"
        assert not self.headers.get("authorization")
        assert body["model"] == "claude-haiku-5-5"
        tools = [t["name"] for t in body.get("tools", [])]
        # The official client may include its documented deferred-tool protocol
        # placeholder even when ToolSearch is excluded. It is never invoked here.
        source_tools = set(tools) - {"DeferredToolPlaceholder"}
        assert source_tools == {"mcp__audex_source__" + name for name in ["list", "read", "search"]}, tools
        assert len(tools) == len(source_tools) + int("DeferredToolPlaceholder" in tools)
        results = []
        for message in body["messages"]:
            content = message.get("content", [])
            if isinstance(content, list):
                for block in content:
                    if block.get("type") == "tool_result":
                        assert not block.get("is_error"), block.get("tool_use_id")
                        text = block["content"]
                        if isinstance(text, list):
                            text = "".join(b.get("text", "") for b in text)
                        assert "example.rs" in text or "fn main" in text, "source response missing fixture"
                        results.append(block["tool_use_id"])
        requests.append({"path": self.path, "model": body["model"], "tools": tools,
                         "tool_results": results, "dummy_credential": True})
        (root / "api-receipts.json").write_text(json.dumps(requests))
        step = len(requests)
        assert step <= 2, "unexpected fallback or repeated inference"
        if step == 2:
            assert set(results) == {"fixture-list", "fixture-read", "fixture-search"}, results
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()

        def emit(kind, **value):
            value = {"type": kind, **value}
            self.wfile.write(f"event: {kind}\ndata: {json.dumps(value)}\n\n".encode())
            self.wfile.flush()

        emit("message_start", message={"id": f"official-msg-{step}", "type": "message", "role": "assistant",
             "model": body["model"], "content": [], "stop_reason": None, "stop_sequence": None,
             "usage": {"input_tokens": 10, "output_tokens": 1}})
        if step == 1:
            blocks = [("thinking", "PRIVATE_SENTINEL_MUST_NOT_PERSIST"), ("text", "Official progress one."),
                      ("text", "Official progress two.")]
            for index, (kind, text) in enumerate(blocks):
                emit("content_block_start", index=index, content_block={"type": kind, kind: ""})
                emit("content_block_delta", index=index, delta={"type": kind + "_delta", kind: text})
                if kind == "thinking":
                    emit("content_block_delta", index=index, delta={"type": "signature_delta", "signature": "PRIVATE_SENTINEL_MUST_NOT_PERSIST"})
                emit("content_block_stop", index=index)
            for index, (name, arguments) in enumerate([
                ("list", {}), ("read", {"path": "example.rs", "offset": 0, "max_bytes": 64}),
                ("search", {"query": "main", "path": None, "max_results": 2})], 3):
                emit("content_block_start", index=index, content_block={"type": "tool_use", "id": "fixture-" + name,
                     "name": "mcp__audex_source__" + name, "input": {}})
                emit("content_block_delta", index=index, delta={"type": "input_json_delta", "partial_json": json.dumps(arguments)})
                emit("content_block_stop", index=index)
        else:
            emit("content_block_start", index=0, content_block={"type": "text", "text": ""})
            emit("content_block_delta", index=0, delta={"type": "text_delta", "text": json.dumps(fixture["result"])})
            emit("content_block_stop", index=0)
        # Terminal completion waits for durable observation of the real client socket.
        if step == 2:
            deadline = time.monotonic() + 30
            while not (root / "socket-observed").exists():
                assert time.monotonic() < deadline, "socket observation barrier timed out"
                time.sleep(0.01)
        emit("message_delta", delta={"stop_reason": "tool_use" if step == 1 else "end_turn", "stop_sequence": None},
             usage={"output_tokens": 37 if step == 1 else 41})
        emit("message_stop")
        self.close_connection = True


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
(root / "api-ready.json").write_text(json.dumps({"url": f"http://127.0.0.1:{server.server_port}"}))
server.serve_forever()
