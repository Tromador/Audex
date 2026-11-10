#!/usr/bin/python3
"""Controlled Claude conversation client. No network or real credentials."""
import json
import os
from pathlib import Path
import sys
import time

root = Path(sys.argv[0]).parent
if sys.argv[1:] == ["--version"]:
    print("2.1.319 (Claude Code)")
    sys.exit(0)
args = sys.argv[1:]
model = args[args.index("--model") + 1]
assert args[args.index("--tools") + 1] == ""
assert json.loads(Path(args[args.index("--mcp-config") + 1]).read_text()) == {"mcpServers": {}}
assert args[args.index("--permission-mode") + 1] == "dontAsk"
assert "--no-session-persistence" in args and "--disable-slash-commands" in args
assert "--effort" not in args and "--fallback-model" not in args
assert "DISABLE_COMPACT" not in os.environ
assert not any(key in os.environ for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "OPENAI_API_KEY"])
assert list(Path.cwd().iterdir()) == []
settings = json.loads(Path(os.environ["CLAUDE_CONFIG_DIR"], "settings.json").read_text())
for subtype in ["initialize", "get_usage", "get_settings", "get_hooks_listing", "get_status"]:
    request = json.loads(sys.stdin.buffer.readline())
    assert request["request"]["subtype"] == subtype
    if subtype == "initialize":
        result = {"pid":os.getpid(), "claude_code_version":"2.1.319", "current_permission_mode":"dontAsk", "commands":[], "output_style":"default", "account":{"apiProvider":"firstParty", "subscriptionType":"Claude Max"}}
    elif subtype == "get_usage":
        result = {"subscription_type":"max"}
    elif subtype == "get_settings":
        result = {"effective":settings, "sources":[{"source":"userSettings", "settings":settings}], "applied":{"model":model, "advisor":None, "ultracode":False, "ultracodeRequested":False}}
    elif subtype == "get_hooks_listing":
        result = {"events":[], "hooks":[], "policy":{"allDisabled":True,"policyHookCount":0}}
    else:
        result = {"sections":[{"title":"Environment","rows":[{"label":"Setting sources","value":"User settings"}]}]}
    print(json.dumps({"type":"control_response", "response":{"subtype":"success", "request_id":request["request_id"], "response":result}}), flush=True)
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
