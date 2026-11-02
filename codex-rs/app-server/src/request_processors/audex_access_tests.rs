use super::*;

#[test]
fn only_the_requested_threads_demonstrated_absence_cases_allow_dispatch() {
    let id = ThreadId::new();
    let other = ThreadId::new();
    for error in [
        ThreadStoreError::ThreadNotFound { thread_id: id },
        ThreadStoreError::InvalidRequest {
            message: format!("no rollout found for thread id {id}"),
        },
    ] {
        assert!(ownership_lookup_error(id, error).is_ok());
    }
    for error in [
        ThreadStoreError::ThreadNotFound { thread_id: other },
        ThreadStoreError::InvalidRequest {
            message: format!("no rollout found for thread id {other}"),
        },
        ThreadStoreError::InvalidRequest {
            message: format!("no rollout found for thread id {id}: lookup failed"),
        },
        ThreadStoreError::Internal {
            message: format!("no rollout found for thread id {id}"),
        },
    ] {
        assert!(ownership_lookup_error(id, error).is_err());
    }
}

#[test]
fn corrupt_or_mismatched_metadata_keeps_the_stale_read_error_contract() {
    let id = ThreadId::new();
    let path = "sessions/stale.jsonl";
    for message in [
        format!(
            "session metadata {path} belongs to thread {}, expected {id}",
            ThreadId::new()
        ),
        format!("failed to read session metadata {path}: invalid session identity"),
    ] {
        let error = ThreadStoreError::Internal { message };
        let expected = format!("failed to read thread: {error}");
        let rejected = ownership_lookup_error(id, error).expect_err("invalid identity must reject");
        assert_eq!(rejected.code, -32603);
        assert_eq!(rejected.message, expected);
    }
}

#[test]
fn lookup_failures_never_grant_access() {
    let id = ThreadId::new();
    for error in [
        ThreadStoreError::Internal {
            message: "Permission denied (os error 13)".into(),
        },
        ThreadStoreError::Internal {
            message: "database disk image is malformed".into(),
        },
        ThreadStoreError::Retryable {
            message: "storage unavailable".into(),
        },
        ThreadStoreError::OutcomeUnknown {
            message: "connection lost".into(),
        },
        ThreadStoreError::Conflict {
            message: "writer conflict".into(),
        },
    ] {
        let expected = format!("failed to read thread: {error}");
        let rejected = ownership_lookup_error(id, error).expect_err("failed lookup must reject");
        assert_eq!(rejected.code, -32603);
        assert_eq!(rejected.message, expected);
    }
    for error in [
        ThreadStoreError::InvalidRequest {
            message: "unrecognised invalid request".into(),
        },
        ThreadStoreError::Unsupported {
            operation: "read_thread",
        },
    ] {
        assert!(ownership_lookup_error(id, error).is_err());
    }
}
