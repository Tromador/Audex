#!/usr/bin/python3
"""C6.2 deterministic stream producer. Only owned fixtures; no provider or network."""
import json
import os
from pathlib import Path
import signal
import sys
import time

if sys.argv[1:] == ["--version"]:
    print("2.1.314 (Claude Code)")
    sys.exit(0)
root = Path(sys.argv[0]).parent
scenario = json.loads((root / "stream-scenario.json").read_text())
mode = scenario["mode"]
config_path = Path(sys.argv[sys.argv.index("--mcp-config") + 1])
config = json.loads(config_path.read_text())["mcpServers"]["audex_source"]
args = config["args"]
binding = json.loads(args[args.index("--binding") + 1])
assert not any(k in os.environ for k in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY", "ANTHROPIC_BASE_URL"])
from fake_policy import handshake

prompt = sys.stdin.buffer.readline() if "--bare" in sys.argv else handshake()
assert prompt and sys.stdin.buffer.read() == b""
assignment = json.loads(json.loads(prompt)["message"]["content"])
with (root / "stream-launches").open("a") as launches:
    launches.write(binding["execution_id"] + "\n")
(root / "stream-prompt-receipt.json").write_text(json.dumps({"count": 1, "nonce": binding["execution_id"], "pid": os.getpid()}))
session = scenario.get("session", "session-" + binding["execution_id"])
model = "claude-haiku-5-5"
seq = 0
if scenario.get("lifecycle"):
    import socket
    import subprocess
    local_socket = socket.socket(socket.AF_UNIX)
    socket_path = Path(scenario.get("socket_path", str(Path(os.environ["TMPDIR"]) / "s")))
    local_socket.bind(str(socket_path))
    local_socket.listen()
    bridge = subprocess.Popen([config["command"], *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, env=dict(os.environ, **config["env"]))
    bridge.stdin.write((json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"c63-lifecycle","version":"1"}}}) + "\n").encode())
    bridge.stdin.flush()
    assert json.loads(bridge.stdout.readline())["id"] == 1
    member = subprocess.Popen(["/usr/bin/sleep", "60"])
    (root / "lifecycle-ready.json").write_text(json.dumps({"client":os.getpid(), "bridge":bridge.pid,
        "member":member.pid, "socket":str(socket_path)}))
secret = "PRIVATE_SENTINEL_MUST_NOT_PERSIST"


def emit(kind, **fields):
    global seq
    seq += 1
    value = {"type": kind, "session_id": session, "uuid": f"event-{seq}", "api_key": secret, **fields}
    data = (json.dumps(value) + "\n").encode()
    # Exercise arbitrary byte fragmentation independently of JSONL record boundaries.
    sys.stdout.buffer.write(data[:7])
    sys.stdout.buffer.flush()
    sys.stdout.buffer.write(data[7:-1] if mode == "terminal_no_newline" and kind == "result" else data[7:])
    sys.stdout.buffer.flush()
    return value


def partial(kind, **fields):
    return emit("stream_event", event={"type": kind, **fields})


def assistant(ident, blocks, served=model):
    return emit("assistant", message={"id": ident, "role": "assistant", "model": served,
        "content": blocks, "stop_reason": "tool_use" if any(b["type"] == "tool_use" for b in blocks) else "end_turn", "usage": {"input_tokens": 11, "output_tokens": 7, "credentials": secret}, "thinking": secret})


def gate(name):
    (root / (name + ".ready")).write_text("ready")
    deadline = time.monotonic() + 30
    while not (root / (name + ".release")).exists():
        if time.monotonic() > deadline:
            raise RuntimeError("fixture gate timeout")
        time.sleep(0.005)


if scenario.get("gate_start"):
    gate("before-public")
billing = {} if mode == "missing_billing" else {"apiKeySource": "ANTHROPIC_API_KEY" if mode == "wrong_billing" else "none"}
emit("system", subtype="init", model="init-is-not-served-model", credentials={"token": secret}, **billing)
sys.stderr.write(secret + "\n")
sys.stderr.flush()
if mode == "wrong_execution":
    emit("assistant", execution_id="foreign-execution", message={})
elif mode == "wrong_session":
    session = "foreign-session"
elif mode == "missing_model":
    model = None
elif mode == "wrong_model":
    model = "wrong-served-model"
if mode == "multi_record":
    partial("message_start", message={"id": "multi-text", "model": model})
    for index, kind in enumerate(["thinking", "text", "text"]):
        partial("content_block_start", index=index, content_block={"type": kind, kind: ""})
        partial("content_block_delta", index=index, delta={"type": kind + "_delta", kind: secret if kind == "thinking" else "Distinct equal block."})
        assistant("multi-text", [{"type": kind, kind: secret if kind == "thinking" else "Distinct equal block."}], model)
        partial("content_block_stop", index=index)
    for index, call in enumerate(["multi-call-1", "multi-call-2"], start=3):
        tool = {"type": "tool_use", "id": call, "name": "mcp__audex_source__list", "input": {}}
        partial("content_block_start", index=index, content_block=tool)
        assistant("multi-text", [tool], model)
        partial("content_block_stop", index=index)
        assistant("multi-text", [tool], model)
    partial("message_stop")
    for call in ["multi-call-1", "multi-call-2", "multi-call-1"]:
        emit("user", message={"role": "user", "content": [{"type": "tool_result", "tool_use_id": call, "content": [{"type": "text", "text": '{"entries":[]}'}]}]})
partial("message_start", message={"id": "progress", "model": model})
partial("content_block_start", index=0, content_block={"type": "thinking", "thinking": secret, "signature": secret})
partial("content_block_delta", index=0, delta={"type": "thinking_delta", "thinking": secret})
partial("content_block_delta", index=0, delta={"type": "signature_delta", "signature": secret})
partial("content_block_stop", index=0)
partial("content_block_start", index=1, content_block={"type": "text", "text": ""})
delta = partial("content_block_delta", index=1, delta={"type": "text_delta", "text": "Public progress."})
sys.stdout.write(json.dumps(delta) + "\n")  # same stable event identity: no second delta
sys.stdout.flush()
partial("content_block_stop", index=1)
tool1 = {"type": "tool_use", "id": "call-1", "name": "mcp__audex_source__list", "input": {}}
partial("content_block_start", index=2, content_block=tool1)
assistant("progress", [{"type": "thinking", "thinking": secret, "signature": secret}, {"type": "text", "text": "Public progress."}, tool1],
          "contradictory-model" if mode == "model_conflict" else model)
partial("content_block_stop", index=2)
partial("message_delta", delta={"stop_reason": "tool_use"})
partial("message_stop")


def tool_result(call):
    result = {"type": "tool_result", "tool_use_id": call, "content": [{"type": "text", "text": json.dumps({"entries": [], "api_key": secret})}, {"type": "thinking", "thinking": secret}]}
    emit("user", message={"role": "user", "content": [result]}, tool_use_result={"secret": secret})
    emit("user", message={"role": "user", "content": [result]})


tool_result("call-1")
tool2 = {"type": "tool_use", "id": "call-2", "name": "mcp__audex_source__list", "input": {"credentials": secret}}
assistant("identical-but-distinct", [{"type": "text", "text": "Public progress."}, tool2], model)
assistant("identical-but-distinct", [{"type": "text", "text": "Public progress."}, tool2], model)
tool_result("call-2")
if scenario.get("gate"):
    gate("before-terminal")
if mode == "malformed":
    sys.stdout.write('{"broken":\n')
    sys.stdout.flush()
    time.sleep(20)
    sys.exit(0)
if mode == "oversized":
    sys.stdout.buffer.write(b'x' * (2 * 1024 * 1024 + 1))
    sys.stdout.buffer.flush()
    time.sleep(20)
    sys.exit(0)
if mode == "truncated":
    sys.stdout.write('{"type":"result"')
    sys.exit(0)
if mode == "eof":
    sys.exit(0)
if mode == "interrupted":
    os.kill(os.getpid(), signal.SIGKILL)
if mode == "stderr_bound":
    sys.stderr.write(secret * 4096)
    sys.stderr.flush()
    time.sleep(20)
if mode == "stream_bound":
    same = {"type": "system", "subtype": "status", "session_id": session, "uuid": "large-private", "thinking": "X" * (1024 * 1024)}
    for _ in range(65):
        sys.stdout.write(json.dumps(same) + "\n")
    sys.stdout.flush()
    time.sleep(20)
if mode == "record_bound":
    same = {"type": "system", "subtype": "status", "session_id": session, "uuid": "repeat"}
    for _ in range(65537):
        sys.stdout.write(json.dumps(same) + "\n")
    sys.stdout.flush()
    time.sleep(20)
result = {"schema_version": 1, "assignment_id": assignment["assignment_id"], "snapshot_id": assignment["snapshot_id"], "summary": "Valid external review", "findings": []}
if mode == "invalid_result":
    result = {"not_an_audit": True}
if mode == "invalid_source":
    result["findings"] = [{"severity": "high", "confidence": "high", "title": "Fixture", "mechanism": "Fixture", "impact": "Fixture", "remediation": None,
        "evidence": [{"description": "Outside snapshot", "path": "missing.rs", "start_byte": 0, "end_byte": 1, "start_line": None, "end_line": None}]}]
text = json.dumps(result)
partial("message_start", message={"id": "final-message", "model": model})
partial("content_block_start", index=0, content_block={"type": "text", "text": ""})
partial("content_block_delta", index=0, delta={"type": "text_delta", "text": text[:len(text)//2]})
partial("content_block_delta", index=0, delta={"type": "text_delta", "text": text[len(text)//2:]})
partial("content_block_stop", index=0)
partial("message_stop")
if mode != "no_final_message":
    assistant("final-message", [{"type": "text", "text": text}], model)
if mode == "wrong_terminal_session":
    session = "foreign-session"
emit("result", subtype="error_during_execution" if mode == "error_terminal" else "success", is_error=mode == "error_terminal", result="different" if mode == "wrong_terminal_text" else text,
    usage={"input_tokens": 55, "output_tokens": 35, "api_key": secret}, num_turns=5, duration_ms=7, total_cost_usd=0.0123, credentials=secret, signature=secret)
if mode == "terminal_no_newline":
    sys.exit(0)
# Exit code is not completion evidence, and the collector need not wait for it.
time.sleep(20)
