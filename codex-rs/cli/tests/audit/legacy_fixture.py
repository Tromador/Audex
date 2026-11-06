"""Owned test databases only: emulate retained v1 bytes or fingerprint audit state."""
import hashlib
import json
import sqlite3
import sys
from pathlib import Path

mode, home, *args = sys.argv[1:]
database = Path(home) / "audex/audit-state.sqlite3"
assert database.is_file(), database
connection = sqlite3.connect(database.as_uri() + ("?mode=rw" if mode == "v1" else "?mode=ro"), uri=True)
if mode == "v1":
    run, = args
    with connection:
        rows = connection.execute(
            "SELECT i.assignment_id,i.request_json FROM audit_attempt_inputs i "
            "JOIN audit_attempts a ON a.assignment_id=i.assignment_id "
            "JOIN audit_reviewers v ON v.id=a.reviewer_id "
            "WHERE a.run_id=? AND a.ordinal=1 ORDER BY v.ordinal", (run,)
        ).fetchall()
        assert rows
        for index, (assignment, raw) in enumerate(rows):
            value = json.loads(raw)
            assert value["version"] == 2
            assert value["authenticationRoute"] == "daemon_configured_native"
            # This is the shape emitted immediately before RetainedInput v2.
            value["version"] = 1
            del value["bindings"]
            raw = json.dumps(value, separators=(",", ":"), sort_keys=True)
            digest = hashlib.sha256(raw.encode()).hexdigest()
            connection.execute("UPDATE audit_attempt_inputs SET request_json=?,input_digest=? WHERE assignment_id=?", (raw, digest, assignment))
            connection.execute("UPDATE operations SET input_digest=? WHERE assignment_id=?", (digest, assignment))
            connection.execute("UPDATE reviewer_assignments SET reviewer_input_digest=? WHERE id=?", (digest, assignment))
            if index == 0:
                connection.execute("UPDATE audit_inputs SET request_json=? WHERE run_id=?", (raw, run))
                connection.execute("UPDATE audit_runs SET input_digest=? WHERE id=?", (digest, run))
        # The original request identity, policy and frozen report are untouched.
        assert connection.execute("SELECT request_identity_digest FROM audit_inputs WHERE run_id=?", (run,)).fetchone()[0]
else:
    assert mode == "fingerprint"
    tables = connection.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").fetchall()
    rows = {name: sorted(connection.execute('SELECT * FROM "' + name.replace('"', '""') + '"').fetchall(), key=repr) for name, in tables}
    print(hashlib.sha256(json.dumps(rows, sort_keys=True, default=repr).encode()).hexdigest())
connection.close()
