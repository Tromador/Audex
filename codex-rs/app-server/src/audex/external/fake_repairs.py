#!/usr/bin/python3
"""Bounded corrective-stream fixture; no network, credentials or inference."""
import json
import os
from pathlib import Path
import sys
import time

if sys.argv[1:] == ["--version"]:
    print("2.1.314 (Claude Code)")
    sys.exit(0)
from fake_policy import handshake

root = Path(sys.argv[0]).parent
scenario = json.loads((root / "stream-scenario.json").read_text())
prompt = handshake()
assert prompt and sys.stdin.buffer.read() == b""
assignment = json.loads(json.loads(prompt)["message"]["content"])
config = json.loads(Path(sys.argv[sys.argv.index("--mcp-config") + 1]).read_text())
args = config["mcpServers"]["audex_source"]["args"]
binding = json.loads(args[args.index("--binding") + 1])
with (root / "stream-launches").open("a") as log:
    log.write(binding["execution_id"] + "\n")
(root / "stream-prompt-receipt.json").write_text(json.dumps({"count": 1, "pid": os.getpid(), "max_budget_usd": sys.argv[sys.argv.index("--max-budget-usd")+1] if "--max-budget-usd" in sys.argv else None}))
seq = 0
records = []
session = "repair-session-" + binding["execution_id"]
model = "claude-haiku-5-5"
secret = "PRIVATE_SENTINEL_MUST_NOT_PERSIST"
terminal_usage = {"input_tokens": 55, "cache_read_input_tokens": 13,
                  "cache_creation_input_tokens": 7, "output_tokens": 35}
prompts = {
    "max_tokens": "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.",
    "end_turn": "[Your previous response had no visible output. Please continue and produce a user-visible response.]",
    "tool_use": "The previous response failed to produce a valid tool call. Please retry the tool call now.",
}


def emit(kind, **fields):
    global seq
    seq += 1
    event = {"type": kind, "session_id": session, "uuid": f"repair-event-{seq}", "credentials": secret, **fields}
    records.append(event)
    return event


def assistant(ident, stop, text):
    return emit("assistant", message={"id": ident, "model": model, "role": "assistant",
                "stop_reason": stop, "content": [{"type": "thinking", "thinking": secret}, {"type": "text", "text": text}]})


def send():
    data = "".join(json.dumps(event) + "\n" for event in records).encode()
    records.clear()
    # Long client-managed continuation chains need several pipe writes.
    while data:
        written = os.write(1, data[:8192])
        assert written > 0
        data = data[written:]


emit("system", subtype="init", apiKeySource="none")
send()
if scenario.get("gate_start"):
    (root / "before-public.ready").write_text("ready")
    deadline = time.monotonic() + 30
    while not (root / "before-public.release").exists():
        assert time.monotonic() < deadline
        time.sleep(0.005)
if scenario.get("error_notice"):
    emit("assistant", error="unknown", message={"id": "synthetic-error", "model": "<synthetic>",
         "content": [{"type": "text", "text": secret}]})
    if scenario.get("error_terminal"):
        emit("result", subtype="error_max_budget_usd", is_error=True, result="",
             usage=terminal_usage, total_cost_usd=0.0123)
        send()
        time.sleep(30)
        sys.exit(0)
if "single_stop" in scenario:
    # 2.1.314 wire shape: a successful result can still carry pause_turn.
    # Full assistant records may omit stop_reason; message_delta supplies it.
    text = "pausing"
    if scenario.get("single_structured"):
        text = json.dumps({"schema_version": 1, "assignment_id": assignment["assignment_id"],
                           "snapshot_id": assignment["snapshot_id"], "summary": "Still paused", "findings": []})
    assistant("single", None, text)
    if scenario["single_stop"] is not None:
        emit("stream_event", event={"type": "message_delta", "delta": {"stop_reason": scenario["single_stop"]}})
    emit("result", subtype="success", is_error=False, result=text,
         stop_reason=scenario.get("terminal_stop", scenario["single_stop"]),
         usage=terminal_usage, total_cost_usd=None if scenario.get("omit_cost") else 0.0123)
    send()
    time.sleep(30)
    sys.exit(0)
mode = scenario.get("mode", "max_tokens")
trigger = "max_tokens" if mode == "conflicting_prefix" else mode if mode in prompts else "end_turn"
text = "" if trigger == "end_turn" and mode not in ["unexpected", "contradictory"] else "Incomplete finding draft."
if mode == "refusal":
    trigger = "refusal"
if scenario.get("streamed_trigger"):
    emit("stream_event", event={"type": "message_start", "message": {"id": "origin", "model": model}})
    emit("stream_event", event={"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
    emit("stream_event", event={"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})
    emit("assistant", message={"id": "origin", "model": model, "content": [{"type": "text", "text": text}]})
    emit("stream_event", event={"type": "content_block_stop", "index": 0})
    emit("stream_event", event={"type": "message_delta", "delta": {"stop_reason": trigger}})
    emit("stream_event", event={"type": "message_stop"})
else:
    assistant("origin", trigger, text)
if mode == "conflicting_prefix":
    assistant("origin", trigger, "Contradictory public prefix.")
for index in range(scenario.get("count", 1)):
    if mode == "resend":
        emit("system", subtype="api_retry", error=secret)
        break
    if mode == "restarted_message":
        emit("stream_event", event={"type": "message_start", "message": {"id": "origin", "model": model}})
        break
    if scenario.get("prompts", True):
        wording = prompts.get(trigger, "Your response above was stopped by a safety classifier. " + secret)
        if mode == "unknown_prompt":
            wording = "Unexpected correction " + secret
        event = emit("user", message={"role": "user", "content": [{"type": "text", "text": wording}]})
        if scenario.get("duplicates"):
            records.append(event)
            emit("user", message=event["message"])
    next_id = f"generation-{index}"
    if scenario.get("subsequent", True):
        if scenario.get("partial_start"):
            emit("stream_event", event={"type": "message_start", "message": {"id": next_id, "model": model}})
        elif scenario.get("reset_after") == index + 1 and index + 1 < scenario.get("count", 1):
            emit("assistant", message={"id": next_id, "model": model, "stop_reason": "tool_use", "content": [
                {"type": "tool_use", "id": "between-occurrences", "name": "mcp__audex_source__list", "input": {}}]})
            emit("user", message={"role": "user", "content": [{"type": "tool_result", "tool_use_id": "between-occurrences", "content": '{"entries":[]}'}]})
            assistant(f"after-tool-{index}", trigger, text)
        else:
            assistant(next_id, trigger, text)
    if not scenario.get("subsequent", True):
        break
if scenario.get("complete"):
    result = json.dumps({"schema_version": 1, "assignment_id": assignment["assignment_id"],
                        "snapshot_id": assignment["snapshot_id"], "summary": "Completed repaired review", "findings": []})
    if scenario.get("final_form") == "fragment":
        result = "OK-final"
    elif scenario.get("final_form") == "wrong_identity":
        result = json.dumps({"schema_version": 1, "assignment_id": "foreign-assignment",
                             "snapshot_id": assignment["snapshot_id"], "summary": "Unbound tail", "findings": []})
    elif scenario.get("final_form") == "fenced":
        result = "```json\n" + result + "\n```"
    # Finish the already observed corrective generation, not another request.
    # Use partial start above so its full text has not already been captured.
    emit("stream_event", event={"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
    emit("stream_event", event={"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": result}})
    emit("stream_event", event={"type": "content_block_stop", "index": 0})
    emit("assistant", message={"id": next_id, "model": model, "role": "assistant", "stop_reason": "end_turn",
                               "content": [{"type": "text", "text": result}]})
    emit("stream_event", event={"type": "message_delta", "delta": {"stop_reason": "end_turn"}})
    emit("stream_event", event={"type": "message_stop"})
    emit("result", subtype="success", is_error=False, result=result, stop_reason="end_turn",
         usage=terminal_usage, total_cost_usd=None if scenario.get("omit_cost") else 0.0123)
send()
time.sleep(30)
