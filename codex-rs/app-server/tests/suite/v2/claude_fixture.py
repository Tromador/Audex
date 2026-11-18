#!/usr/bin/python3
"""Controlled Claude conversation client. No network or real credentials."""
import json
import os
from pathlib import Path
import sys
import time

root = Path(sys.argv[0]).parent
args = sys.argv[1:]
model = args[args.index("--model") + 1]
assert args[args.index("--tools") + 1] == ""
assert "--restricted" in args and "--strict-mcp-config" in args
assert "--mcp-config" not in args and "--json-schema" not in args
assert "--add-dir" not in args and "--include-partial-messages" in args
assert "--allowedTools" not in args
assert args[args.index("--permission-mode") + 1] == "dontAsk"
assert "--no-session-persistence" in args and "--disable-slash-commands" in args
assert "--effort" not in args and "--fallback-model" not in args
assert "DISABLE_COMPACT" not in os.environ
assert not any(key in os.environ for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "OPENAI_API_KEY"])
assert list(Path.cwd().iterdir()) == []
prompt = json.loads(sys.stdin.buffer.readline())
assert sys.stdin.buffer.read() == b""
prompt = json.loads(prompt["message"]["content"])
with (root / "prompts.jsonl").open("a") as log:
    log.write(json.dumps({"pid":os.getpid(), "model":model, "prompt":prompt, "args":args}) + "\n")
mode = (root / "mode").read_text().strip()
if mode == "held":
    while not (root / "release").exists():
        time.sleep(0.01)
if mode == "eof":
    sys.exit(0)
session = "fixture-session"
def emit(kind, **fields):
    print(json.dumps({"type":kind,"session_id":session,"uuid":kind,**fields}), flush=True)
served = "claude-wrong-model" if mode == "wrong_model" else model
emit("system", subtype="init", model=served, tools=["Bash"] if mode == "tools" else [], mcp_servers=[], permissionMode="dontAsk", apiKeySource="none")
if mode == "rate_limit":
    emit("rate_limit_event", rate_limit_info={"status":"allowed"})
text = "Claude fixture answer: " + prompt["operator_message"]
emit("stream_event", event={"type":"message_start","message":{"id":"answer","model":served,"content":[]}})
if mode in ["text_blocks", "text_mismatch"]:
    # Official client sequence retained in cycle-6-3/pinned-stream-probe.json:
    # thinking and each completed text block are separate records for one message.
    blocks = [{"type":"thinking", "thinking":"PRIVATE_THINKING_SENTINEL", "signature":"fixture"},
              {"type":"text", "text":"First public block. "},
              {"type":"text", "text":"Second public block."}]
    for index, block in enumerate(blocks):
        emit("stream_event", event={"type":"content_block_start","index":index,"content_block":{"type":block["type"]}})
        emit("assistant", message={"id":"answer", "model":served, "content":[block], "stop_reason":None})
        emit("stream_event", event={"type":"content_block_stop","index":index})
    text = blocks[-1]["text"] if mode == "text_blocks" else "Mismatched terminal text."
else:
    emit("assistant", message={"id":"answer", "model":served, "content":[{"type":"text","text":text}], "stop_reason":None})
emit("stream_event", event={"type":"message_delta","delta":{"stop_reason":"max_tokens" if mode == "incomplete" else "end_turn"}})
emit("result", subtype="error_during_execution" if mode == "error" else "success", is_error=mode == "error", result=text)
time.sleep(30)  # The normal owner must stop the client after terminal capture.
