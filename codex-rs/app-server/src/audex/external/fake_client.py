#!/usr/bin/python3
"""Deterministic owned child: no endpoint, credentials, model or replacement source service."""
import ctypes
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import time

if sys.argv[1:] == ["--version"]:
    print("2.1.314 (Claude Code)")
    sys.exit(0)

config_path = Path(sys.argv[sys.argv.index("--mcp-config") + 1])
directory = config_path.parent
sys.stderr = (directory / "fixture-error.log").open("w")
config = json.loads(config_path.read_text())["mcpServers"]["audex_source"]
args = config["args"]
binding = json.loads(args[args.index("--binding") + 1])
root = Path(args[args.index("--state-root") + 1])
with (directory / "spawn-count").open("a") as file:
    file.write("spawn\n")
bridge = None
if (Path(sys.argv[0]).parent / "early-bridge").exists():
    bridge = subprocess.Popen([config["command"], *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, env=dict(os.environ, **config["env"]))
    pending = bridge.stderr.readline().decode().strip()
    assert pending == "Audex source bridge waiting for committed launch identity", pending
    (directory / "early-bridge.json").write_text(json.dumps({"bridge_pid": bridge.pid, "identity_pending": True}))
prompt = sys.stdin.buffer.readline()
if not prompt:
    sys.exit(0)
assert sys.stdin.buffer.read() == b"", "duplicate prompt"
assert len(prompt.splitlines()) == 1
message = json.loads(prompt)
assert message["type"] == "user"
assert message["message"]["role"] == "user"
assert b"HOST_DEVELOPER_DO_NOT_INHERIT" not in prompt
with sqlite3.connect(f"file:{root / 'audit-state.sqlite3'}?mode=ro", uri=True) as db:
    launch = db.execute("SELECT pid,start_ticks,boot_id,process_group,parent_pid,identity_at_ms,prompt_write_started_at_ms,execution_id FROM audit_client_launches WHERE operation_id=?", (binding["operation_id"],)).fetchone()
    boundary = db.execute("SELECT submission_boundary FROM operations WHERE id=?", (binding["operation_id"],)).fetchone()[0]
assert launch[0] == os.getpid() == os.getpgrp()
assert launch[4] == os.getppid()
assert launch[5] is not None and launch[6] is not None
assert launch[7] == binding["execution_id"] and binding["execution_id"] in str(config_path)
assert boundary == "submission_started"
signal = ctypes.c_int()
assert ctypes.CDLL(None).prctl(2, ctypes.byref(signal), 0, 0, 0) == 0
assert signal.value == 9
assert set(os.environ) <= {
    "PATH", "USER", "LANG", "TERM", "HOME", "TMPDIR", "CLAUDE_CONFIG_DIR", "LC_CTYPE",
    "CLAUDE_CODE_MAX_RETRIES", "CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK",
    "CLAUDE_CODE_NONSTREAMING_TIMEOUT_RETRIES", "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK",
    "CLAUDE_CODE_DISABLE_REFUSAL_RETRY", "DISABLE_COMPACT", "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
    "CLAUDE_CODE_DISABLE_CLAUDE_MDS", "CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
}
assert Path.cwd() == directory / "cwd" and not list(Path.cwd().iterdir())
assert Path(os.environ["HOME"]) == directory / "home"
assert not list(Path(os.environ["CLAUDE_CONFIG_DIR"]).iterdir())
assert sys.argv[sys.argv.index("--tools") + 1] == ""
assert sys.argv[sys.argv.index("--allowedTools") + 1].split(",") == [f"mcp__audex_source__{n}" for n in ["list", "read", "search"]]
assert not {"--resume", "--continue", "--fallback-model", "--json-schema"}.intersection(sys.argv)
bridge_env = dict(os.environ, **config["env"])
if bridge is None:
    bridge = subprocess.Popen([config["command"], *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=bridge_env)
transcript = []


def send(payload):
    bridge.stdin.write((json.dumps(payload) + "\n").encode())
    bridge.stdin.flush()


def call(number, method, params):
    send({"jsonrpc": "2.0", "id": number, "method": method, "params": params})
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        assert select.select([bridge.stdout], [], [], 10)[0], "bridge response timeout"
        line = bridge.stdout.readline()
        assert line, bridge.stderr.read().decode()
        response = json.loads(line)
        if response.get("id") == number:
            transcript.append(response)
            return response
    raise AssertionError("missing response")


try:
    call(1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "c61-fixture", "version": "1"}})
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    inventory = call(2, "tools/list", {})["result"]["tools"]
    assert [tool["name"] for tool in inventory] == ["list", "read", "search"]
    for number, name, arguments in [
        (3, "list", {}),
        (4, "read", {"path": "example.rs", "offset": 0, "max_bytes": 64}),
        (5, "search", {"query": "main", "path": None, "max_results": 2}),
    ]:
        result = call(number, "tools/call", {"name": name, "arguments": arguments})["result"]
        assert not result.get("isError"), result
    refused = call(6, "tools/call", {"name": "read", "arguments": {"path": "/etc/passwd", "offset": 0, "max_bytes": 64}})["result"]
    assert refused["isError"]
    receipt = {"pid": os.getpid(), "bridge_pid": bridge.pid, "nonce": binding["execution_id"], "prompt_count": 1,
               "boundary_at_read": boundary, "parent_death_signal": signal.value, "identity": launch,
               "environment": dict(os.environ), "argv": sys.argv[1:], "transcript": transcript}
    (directory / "receipt.json").write_text(json.dumps(receipt))
    deadline = time.monotonic() + 20
    while not (directory / "stop").exists() and time.monotonic() < deadline:
        if (directory / "read-again").exists() and not (directory / "revoked.json").exists():
            refused = call(7, "tools/call", {"name": "read", "arguments": {"path": "example.rs", "offset": 0, "max_bytes": 64}})
            assert refused["result"]["isError"], refused
            (directory / "revoked.json").write_text(json.dumps(refused))
        time.sleep(0.01)
finally:
    bridge.stdin.close()
    bridge.wait(timeout=5)
