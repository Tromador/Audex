#!/usr/bin/python3
"""Public commissioning fixture: real source bridge, dummy policy replies, no network."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root = Path(sys.argv[0]).parent
with (root / "launches").open("a") as log:
    log.write("launch\n")
if sys.argv[1:] == ["--version"]:
    print("2.1.314 (Claude Code)")
    sys.exit(0)
assert not any(k in os.environ for k in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "OPENAI_API_KEY"])
from fake_policy import handshake

assert sys.argv[sys.argv.index("--model") + 1] == "claude-haiku-5-5"
prompt = handshake()
assert prompt and sys.stdin.buffer.read() == b""
assignment = json.loads(json.loads(prompt)["message"]["content"])
mcp = json.loads(Path(sys.argv[sys.argv.index("--mcp-config") + 1]).read_text())["mcpServers"]["audex_source"]
args = mcp["args"]
binding = json.loads(args[args.index("--binding") + 1])
mode = json.loads((root / "scenario.json").read_text())["mode"]
with (root / "prompts").open("a") as log:
    log.write(json.dumps({"assignment":assignment["assignment_id"], "execution":binding["execution_id"], "count":1}) + "\n")
session = "public-" + binding["execution_id"]
model = "claude-haiku-5-5"
if mode == "alias":
    model = "claude-qualified-alias"
if mode == "wrong_model":
    model = "claude-wrong-model"
if mode == "missing_model":
    model = None
sequence = 0


def emit(kind, **fields):
    global sequence
    sequence += 1
    print(json.dumps({"type":kind, "uuid":f"public-{sequence}", "session_id":session, **fields}), flush=True)


def assistant(ident, blocks, stop):
    emit("stream_event", event={"type":"message_start", "message":{"id":ident,"model":model}})
    for index, block in enumerate(blocks):
        emit("stream_event", event={"type":"content_block_start","index":index,"content_block":block})
        emit("stream_event", event={"type":"content_block_stop","index":index})
    emit("assistant", message={"id":ident,"role":"assistant","model":model,"content":blocks,"stop_reason":stop})
    emit("stream_event", event={"type":"message_delta","delta":{"stop_reason":stop}})
    emit("stream_event", event={"type":"message_stop"})


emit("system", subtype="init", model="requested-is-not-served", apiKeySource="ANTHROPIC_API_KEY" if mode == "wrong_billing" else "none")
bridge = subprocess.Popen([mcp["command"], *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE, env=dict(os.environ, **mcp["env"]))
number = 0


def call(method, params):
    global number
    number += 1
    bridge.stdin.write((json.dumps({"jsonrpc":"2.0","id":number,"method":method,"params":params}) + "\n").encode())
    bridge.stdin.flush()
    while True:
        line = bridge.stdout.readline()
        assert line, "source bridge closed"
        reply = json.loads(line)
        if reply.get("id") == number:
            return reply["result"]


try:
    call("initialize", {"protocolVersion":"2025-03-26", "capabilities":{}, "clientInfo":{"name":"public-fixture","version":"1"}})
    bridge.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
    bridge.stdin.flush()
    tools = call("tools/list", {})["tools"]
    assert [t["name"] for t in tools] == ["list", "read", "search"]
    calls = [("list", {}), ("read", {"path":"example.rs","offset":0,"max_bytes":1024}),
             ("search", {"query":"fn", "path":None, "max_results":2})]
    blocks = [{"type":"text","text":"Independent subscription review progress."}]
    blocks += [{"type":"tool_use","id":"call-"+name,"name":"mcp__audex_source__"+name,"input":arguments} for name, arguments in calls]
    assistant("source-tools", blocks, "tool_use")
    results = []
    for name, arguments in calls:
        result = call("tools/call", {"name":name,"arguments":arguments})
        assert not result.get("isError"), result
        if name == "read":
            source = json.loads(result["content"][0]["text"])
            assert source["content"] == "fn main() {}\n" and source["eof"] is True, source
        results.append({"type":"tool_result","tool_use_id":"call-"+name,"content":result["content"]})
    emit("user", message={"role":"user","content":results})
    (root / ("ready-" + binding["execution_id"])).write_text(json.dumps({"pid":os.getpid(), "bridge":bridge.pid,
        "assignment":assignment["assignment_id"], "tools":[t["name"] for t in tools], "results":results}))
    if mode == "held":
        deadline = time.monotonic() + 60
        while not (root / ("release-" + binding["execution_id"])).exists():
            assert time.monotonic() < deadline, "fixture release timeout"
            time.sleep(0.01)
    if mode == "repair":
        assistant("truncated", [{"type":"text","text":"Incomplete subscription material."}], "max_tokens")
        emit("user", message={"role":"user","content":"Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces."})
        time.sleep(30)
        sys.exit(0)
    if mode == "interrupted":
        sys.stdout.write('{"type":"result"')
        sys.stdout.flush()
        sys.exit(0)
    result = {"schema_version":1, "snapshot_id":assignment["snapshot_id"], "assignment_id":assignment["assignment_id"],
              "summary":"Independent public subscription review", "findings":[]}
    if mode == "wrong_claim":
        result["assignment_id"] = "untrusted-claimed-assignment"
    text = json.dumps(result)
    if mode == "fenced":
        text = "```json\n" + text + "\n```"
    elif mode == "plain":
        text = "Independent public subscription review in plain English. No concrete defect was observed."
    elif mode == "diagnostic_controls":
        # An actual escaped-control unknown key, decoded by the result validator.
        text = '{"\\u001b]0;MODEL_TITLE\\u0007\\u001b[31m": true}'
    assistant("final", [{"type":"text","text":text}], "end_turn")
    emit("result", subtype="success", is_error=False, result=text, usage={"input_tokens":100,"output_tokens":40}, num_turns=2)
    time.sleep(30)
finally:
    bridge.stdin.close()
    bridge.wait(timeout=5)
