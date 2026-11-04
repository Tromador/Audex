-- One owned launch per existing operation/execution. Submission is never replayable.
CREATE TABLE audit_client_launches (
    operation_id TEXT PRIMARY KEY REFERENCES operations(id),
    execution_id TEXT NOT NULL UNIQUE,
    route TEXT NOT NULL CHECK(route IN ('claude_subscription','claude_api')),
    configuration_identity TEXT NOT NULL,
    launch_configuration_digest TEXT NOT NULL,
    executable TEXT NOT NULL,
    client_version TEXT NOT NULL,
    mcp_config_path TEXT NOT NULL UNIQUE,
    intent_at_ms INTEGER NOT NULL,
    pid INTEGER,
    start_ticks INTEGER,
    boot_id TEXT,
    process_group INTEGER,
    parent_pid INTEGER,
    identity_at_ms INTEGER,
    prompt_write_started_at_ms INTEGER,
    CHECK((pid IS NULL AND start_ticks IS NULL AND boot_id IS NULL AND process_group IS NULL
           AND parent_pid IS NULL AND identity_at_ms IS NULL)
       OR (pid > 0 AND start_ticks >= 0 AND length(boot_id) > 0 AND process_group = pid
           AND parent_pid > 0 AND identity_at_ms IS NOT NULL)),
    CHECK(prompt_write_started_at_ms IS NULL OR identity_at_ms IS NOT NULL)
);
