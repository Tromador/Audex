"""Own an isolated test daemon and reap its descendants after a deliberate crash."""
import ctypes
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

binary, root, stage = sys.argv[1:]
root = Path(root)
assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0  # PR_SET_CHILD_SUBREAPER
process = subprocess.Popen(
    [binary, "--exact", "audex::controller::tests::execution::external::isolated_owner_process", "--nocapture"],
    env=dict(os.environ, AUDEX_C61_OWNER_ROOT=str(root), AUDEX_C61_OWNER_STAGE=stage),
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
)
try:
    deadline = time.monotonic() + 20
    checkpoint = root / "owner-checkpoint.json"
    while not checkpoint.exists() and time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(process.stdout.read().decode())
        time.sleep(0.01)
    assert checkpoint.exists(), "owner checkpoint timeout"
    record = json.loads(checkpoint.read_text())
    process.kill()
    process.wait(timeout=5)
    reaped = []
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            pid, status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            break
        if pid:
            reaped.append([pid, status])
        else:
            time.sleep(0.01)
    else:
        raise AssertionError("owned descendant survived daemon death")
    assert record["pid"] is None or any(pid == record["pid"] for pid, _ in reaped)
    print(json.dumps({"stage": stage, "daemon_killed": True, "reaped": reaped, "checkpoint": record}))
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    # On fixture failure, only this supervisor's adopted descendants may be killed.
    children = Path(f"/proc/{os.getpid()}/task/{os.getpid()}/children").read_text().split()
    for child in children:
        try:
            os.kill(int(child), signal.SIGKILL)
        except ProcessLookupError:
            pass
    for child in children:
        try:
            os.waitpid(int(child), 0)
        except ChildProcessError:
            pass
