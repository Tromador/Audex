"""Exercise the production stdio bridge; only local deterministic evidence is used."""
import subprocess


def exercise_evidence():
    config = mcp["mcpServers"]["audex_source"]
    child = subprocess.Popen([config["command"], *config["args"]],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             env={**os.environ, **config["env"]}, text=True)
    serial = 0

    def rpc(method, params):
        nonlocal serial
        serial += 1
        child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": serial,
                                     "method": method, "params": params}) + "\n")
        child.stdin.flush()
        while True:
            line = child.stdout.readline()
            assert line, "bridge exited before responding"
            value = json.loads(line)
            if value.get("id") == serial:
                return value

    rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "claude-fixture", "version": "1"}})
    child.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    child.stdin.flush()
    inventory = rpc("tools/list", {})["result"]["tools"]
    assert {tool["name"] for tool in inventory} == {"list", "read", "search", "evidence"}
    assert "runId" not in inventory[-1]["inputSchema"].get("properties", {})
    assert "diagnostics" in inventory[-1]["inputSchema"]["properties"]["view"]["enum"]
    assert "guidance" in inventory[-1]["inputSchema"]["properties"]["view"]["enum"]
    calls = []

    def call(name, arguments, denied=False):
        result = rpc("tools/call", {"name": name, "arguments": arguments})
        calls.append({"name": name, "arguments": arguments, "response": result})
        if denied:
            assert "error" in result or result["result"].get("isError"), result
            return
        assert "error" not in result and not result["result"].get("isError"), result
        call_id = "evidence-" + str(serial)
        # Exercise the actual client event sequence around a source-tool exchange.
        emit("stream_event", event={"type": "message_start", "message": {"id": call_id, "model": model}})
        emit("stream_event", event={"type": "content_block_start", "index": 0,
                                    "content_block": {"type": "tool_use", "id": call_id, "name": "mcp__audex_source__" + name}})
        emit("assistant", message={"id": call_id, "model": model, "content": [
            {"type": "tool_use", "id": call_id, "name": "mcp__audex_source__" + name, "input": arguments}], "stop_reason": "tool_use"})
        emit("user", message={"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": call_id, "content": result["result"]["content"]}]})
        value = result["result"]["structuredContent"]
        assert value["identity"] == prompt["retained_audit"]
        return value

    # Retrieve the original context documents independently of findings/reviewer prose.
    import hashlib
    guidance_query = {"view": "guidance", "snapshot": prompt["retained_audit"]["snapshotId"]}
    guidance_inventory = call("evidence", guidance_query)
    guidance = []
    for document in guidance_inventory["documents"]:
        query = {**guidance_query, "path": document["path"]}
        full = call("evidence", query)
        assert full["document"] == document
        assert hashlib.sha256(full["content"].encode()).hexdigest() == document["sha256"]
        assert len(full["content"].encode()) == document["size"]
        offset = 0
        restored_guidance = ""
        while True:
            page = call("evidence", {**query, "offset": offset, "maxBytes": 1021})
            assert page["document"] == document
            restored_guidance += page["content"]
            if page["eof"]:
                break
            offset = page["nextOffset"]
        assert restored_guidance == full["content"]
        assert "LIVE_GUIDANCE_REPLACED" not in full["content"]
        guidance.append(full)
    for extra in [{"snapshot": "foreign-snapshot"}, {"path": "../AGENTS.md"},
                  {"path": "/etc/passwd"}, {"runId": "foreign-run"},
                  {"path": "example.rs"}, {"reviewer": "foreign-reviewer"}]:
        call("evidence", {**guidance_query, **extra}, denied=True)
    if mode == "guidance_only":
        expected = json.loads((root / "expected-guidance.json").read_text())
        assert guidance == expected
        child.stdin.close()
        child.wait(timeout=5)
        assert child.returncode == 0
        (root / "guidance-parity.json").write_text(json.dumps(guidance))
        return " | Original captured guidance retrieved"

    diagnostics = call("evidence", {"view": "diagnostics"})
    assert diagnostics["runId"] == prompt["retained_audit"]["runId"]
    assert diagnostics["snapshotId"] == prompt["retained_audit"]["snapshotId"]
    restored = ""
    offset = 0
    while True:
        part = call("evidence", {"view": "diagnostics", "offset": offset, "maxBytes": 4093})
        restored += part["content"]
        if part["eof"]:
            break
        offset = part["nextOffset"]
    assert restored == diagnostics["content"]
    for extra in [{"runId": "foreign-run"}, {"reviewer": "foreign-reviewer"},
                  {"snapshot": "foreign-snapshot"}, {"path": "/etc/passwd"}]:
        call("evidence", {"view": "diagnostics", **extra}, denied=True)
    if mode == "diagnostics_only":
        expected = json.loads((root / "expected-diagnostics.json").read_text())
        assert diagnostics == expected
        child.stdin.close()
        child.wait(timeout=5)
        assert child.returncode == 0
        (root / "diagnostics-parity.json").write_text(json.dumps(diagnostics))
        return " | Complete reviewer diagnostics retrieved"

    reviewers = call("evidence", {"view": "reviewers"})
    reviewer = reviewers["items"][0]["reviewerId"]
    page = call("evidence", {"view": "findings", "reviewer": reviewer, "limit": 1})
    assert page["nextOffset"] == 1
    tail = call("evidence", {"view": "findings", "reviewer": reviewer, "offset": 1})
    finding = call("evidence", {"view": "finding", "reviewer": reviewer, "number": 2})
    assert finding["finding"]["id"] == tail["items"][0]["id"]
    assert finding["reviewer"]["reviewerId"] == reviewer
    call("list", {})
    source = call("evidence", {"view": "source", "snapshot": prompt["retained_audit"]["snapshotId"], "path": "example.rs"})
    assert source["linesVerified"] is True
    call("read", {"path": "example.rs", "offset": 0, "max_bytes": 100})
    call("search", {"query": "fn", "path": None, "max_results": 10})
    for name, arguments in [
        ("write", {"path": "example.rs", "content": "changed"}),
        ("audit_start", {}),
        ("read", {"path": str(root / "mode"), "offset": 0, "max_bytes": 20}),
        ("read", {"path": "../outside.rs", "offset": 0, "max_bytes": 20}),
        ("evidence", {"view": "findings", "runId": "foreign-run"}),
        ("evidence", {"view": "source", "snapshot": "foreign-snapshot", "path": "example.rs"}),
    ]:
        call(name, arguments, denied=True)
    foreign = root / "foreign-audit.json"
    if foreign.exists():
        other = json.loads(foreign.read_text())
        assert other["runId"] != prompt["retained_audit"]["runId"]
        assert other["runId"] not in restored
        assert other["reviewerId"] not in restored
        call("evidence", {"view": "guidance", "snapshot": other["snapshotId"], "path": "AGENTS.md"}, denied=True)
        call("evidence", {"view": "source", "snapshot": other["snapshotId"], "path": "example.rs"}, denied=True)
        call("evidence", {"view": "finding", "reviewer": other["reviewerId"], "number": 2}, denied=True)
        # A forged launch binding cannot turn this source identity into another run.
        forged = list(config["args"])
        position = forged.index("--binding") + 1
        binding = json.loads(forged[position])
        binding["identity"]["runId"] = other["runId"]
        forged[position] = json.dumps(binding)
        refused = subprocess.run([config["command"], *forged], input="", capture_output=True,
                                 env={**os.environ, **config["env"]}, text=True, timeout=5)
        assert refused.returncode != 0 and "binding mismatch" in refused.stderr
    forged = list(config["args"])
    position = forged.index("--binding") + 1
    binding = json.loads(forged[position])
    binding["mcp_config_path"] = "/unrelated-parent-marker"
    forged[position] = json.dumps(binding)
    refused = subprocess.run([config["command"], *forged], input="", capture_output=True,
                             env={**os.environ, **config["env"]}, text=True, timeout=5)
    assert refused.returncode != 0
    with (root / "bridge.jsonl").open("a") as log:
        log.write(json.dumps({"identity": prompt["retained_audit"], "calls": calls, "pid": child.pid}) + "\n")
    if mode == "evidence_held":
        while not (root / "release").exists():
            time.sleep(0.01)
    child.stdin.close()
    child.wait(timeout=5)
    assert child.returncode == 0
    return " | " + finding["finding"]["title"] + " | sealed source: " + source["content"]
