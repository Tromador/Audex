"""Owned subreaper for deterministic daemon crash/reconstruction qualification."""
import ctypes
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

binary, root, stage = sys.argv[1:]
root = Path(root)
assert ctypes.CDLL(None).prctl(36, 1, 0, 0, 0) == 0
args = [binary, "--exact", "audex::controller::tests::execution::external::restart_qualification::isolated_lifecycle_daemon", "--nocapture"]
env = dict(os.environ, AUDEX_C63_RESTART_ROOT=str(root), AUDEX_C63_RESTART_STAGE=stage)
owner = subprocess.Popen(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
recovery = None
home = None
try:
    deadline = time.monotonic() + 20
    while not (root / "checkpoint.json").exists():
        assert owner.poll() is None, owner.stdout.read().decode()
        assert time.monotonic() < deadline, "daemon checkpoint timeout"
        time.sleep(0.01)
    home = Path(json.loads((root / "checkpoint.json").read_text())["home"])
    owner.kill()
    owner.wait(timeout=5)
    if stage == "reused":
        # Deliberately point the persisted numeric PID/PGID at this live foreign
        # supervisor with contradictory start ticks. Recovery must never signal it.
        with sqlite3.connect(home / "audex" / "audit-state.sqlite3") as db:
            db.execute("UPDATE audit_client_launches SET pid=?,process_group=?,start_ticks=0", (os.getpid(),os.getpid()))
    # Surviving group members remain for the production recovery path to stop.
    recovery = subprocess.Popen(args, env=dict(env, AUDEX_C63_RECOVER="1"), stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    out, _ = recovery.communicate(timeout=25)
    assert recovery.returncode == 0, out.decode()
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            pid, status = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            break
        if not pid:
            time.sleep(0.01)
    else:
        raise AssertionError("owned process survived reconstruction")
finally:
    for process in [owner, recovery]:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
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
    if home is not None:
        for directory, dirs, files in os.walk(home):
            os.chmod(directory, 0o700)
        shutil.rmtree(home)
