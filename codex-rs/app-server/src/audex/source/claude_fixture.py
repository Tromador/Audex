"""Official pinned client + real Rust bridge, using a scripted loopback API only."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root, claude, audex, binding, snapshot, manifest = sys.argv[1:]
root = Path(root)
for name in ["client-home", "profile", "empty", "bridge-home"]:
    (root / name).mkdir(exist_ok=True)
requests = []
expected_tools = [f"mcp__audex_source__{name}" for name in ["list", "read", "search"]]


def events(number, blocks, stop):
    yield (
        "message_start",
        {
            "type": "message_start",
            "message": {
                "id": f"msg_{number}",
                "type": "message",
                "role": "assistant",
                "model": "claude-haiku-5-5",
                "content": [],
                "stop_reason": None,
                "stop_sequence": None,
                "usage": {"input_tokens": 10, "output_tokens": 1},
            },
        },
    )
    for index, block in enumerate(blocks):
        if block["type"] == "tool_use":
            start = {**block, "input": {}}
            delta = {
                "type": "input_json_delta",
                "partial_json": json.dumps(block["input"]),
            }
        else:
            start = {"type": "text", "text": ""}
            delta = {"type": "text_delta", "text": block["text"]}
        yield (
            "content_block_start",
            {"type": "content_block_start", "index": index, "content_block": start},
        )
        yield (
            "content_block_delta",
            {"type": "content_block_delta", "index": index, "delta": delta},
        )
        yield "content_block_stop", {"type": "content_block_stop", "index": index}
    yield (
        "message_delta",
        {
            "type": "message_delta",
            "delta": {"stop_reason": stop, "stop_sequence": None},
            "usage": {"output_tokens": 5},
        },
    )
    yield "message_stop", {"type": "message_stop"}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))))
        requests.append(body)
        if self.path.split("?")[0] != "/v1/messages":
            self.send_error(400)
            return
        if len(requests) == 1:
            blocks = [
                {
                    "type": "tool_use",
                    "id": "list-call",
                    "name": expected_tools[0],
                    "input": {},
                },
                {
                    "type": "tool_use",
                    "id": "read-call",
                    "name": expected_tools[1],
                    "input": {"path": "example.rs", "offset": 0, "max_bytes": 64},
                },
                {
                    "type": "tool_use",
                    "id": "search-call",
                    "name": expected_tools[2],
                    "input": {"query": "sealed", "path": None, "max_results": 4},
                },
            ]
            stop = "tool_use"
        else:
            blocks, stop = [{"type": "text", "text": "DONE"}], "end_turn"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True
        for kind, event in events(len(requests), blocks, stop):
            self.wfile.write(f"event: {kind}\ndata: {json.dumps(event)}\n\n".encode())
            self.wfile.flush()


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
# The fixture supplies the verified client's PID without adding a production adapter.
# exec retains the bridge process/parent identity; source execution is the Rust binary.
bootstrap = "import os,sys; os.execv(sys.argv[1], sys.argv[1:]+['--parent-pid',str(os.getppid())])"
config = {
    "mcpServers": {
        "audex_source": {
            "type": "stdio",
            "command": sys.executable,
            "args": [
                "-c",
                bootstrap,
                audex,
                "audit-source-mcp",
                "--state-root",
                str(root),
                "--binding",
                binding,
                "--snapshot",
                snapshot,
                "--manifest-digest",
                manifest,
            ],
            "env": {"CODEX_HOME": str(root / "bridge-home")},
        }
    }
}
env = {
    "HOME": str(root / "client-home"),
    "USER": "fixture",
    "PATH": "/usr/bin:/bin",
    "LANG": "C.UTF-8",
    "TERM": "dumb",
    "TMPDIR": os.environ["TMPDIR"],
    "CLAUDE_CONFIG_DIR": str(root / "profile"),
    "ANTHROPIC_API_KEY": "dummy-audex-s1-no-inference",
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
}
for key, value in {
    "CLAUDE_CODE_MAX_RETRIES": "0",
    "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK": "1",
    "CLAUDE_CODE_NONSTREAMING_TIMEOUT_RETRIES": "0",
    "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK": "1",
    "CLAUDE_CODE_DISABLE_REFUSAL_RETRY": "1",
    "DISABLE_COMPACT": "1",
    "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
    "CLAUDE_CODE_DISABLE_CLAUDE_MDS": "1",
    "CLAUDE_CODE_DISABLE_TERMINAL_TITLE": "1",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
}.items():
    env[key] = value
argv = [
    claude,
    "-p",
    "--bare",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--model",
    "claude-haiku-5-5",
    "--tools",
    "",
    "--strict-mcp-config",
    "--mcp-config",
    json.dumps(config),
    "--permission-mode",
    "dontAsk",
    "--allowedTools",
    ",".join(expected_tools),
    "--disable-slash-commands",
    "--no-session-persistence",
    "--system-prompt",
    "Inspect only the assigned sealed source. This is a scripted transport fixture.",
]
process = subprocess.Popen(
    argv,
    cwd=root / "empty",
    env=env,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    start_new_session=True,
)
try:
    prompt = (
        json.dumps(
            {
                "type": "user",
                "message": {
                    "role": "user",
                    "content": "Use the assigned source tools and finish.",
                },
            }
        )
        + "\n"
    ).encode()
    stdout, stderr = process.communicate(prompt, timeout=60)
    assert process.returncode == 0, stderr.decode(errors="replace")
    stream = [json.loads(line) for line in stdout.splitlines()]
    init = next(
        line
        for line in stream
        if line.get("type") == "system" and line.get("subtype") == "init"
    )
    assert sorted(init["tools"]) == sorted(expected_tools), init["tools"]
    assert init["apiKeySource"] == "ANTHROPIC_API_KEY"
    assert len(requests) == 2, len(requests)
    assert sorted(t["name"] for t in requests[0]["tools"]) == sorted(expected_tools)
    results = requests[1]["messages"][-1]["content"]
    assert {r["tool_use_id"] for r in results} == {
        "list-call",
        "read-call",
        "search-call",
    }
    assert all(not r.get("is_error") for r in results), results
    assert "sealed-for-claude" in json.dumps(results)
    terminal = next(line for line in stream if line.get("type") == "result")
    assert not terminal.get("is_error") and terminal["result"] == "DONE", terminal
    print(
        json.dumps(
            {
                "tool_inventory": init["tools"],
                "scripted_requests": len(requests),
                "tool_round_trip_ids": [r["tool_use_id"] for r in results],
                "api_key_source": init["apiKeySource"],
                "successful_terminal_result": True,
                "external_inference": False,
            }
        )
    )
finally:
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
