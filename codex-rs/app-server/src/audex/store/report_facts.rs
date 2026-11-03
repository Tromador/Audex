//! Read only durable facts under the same transaction that freezes contributions.
use super::*;
use crate::audex::report::facts::*;
use crate::audex::retained::RetainedInput;
use codex_app_server_protocol::{AuditAuthenticationRoute, AuditGenerationIdentity};

pub(super) async fn collect(
    connection: &mut sqlx::SqliteConnection,
    assignment: &str,
    retained: Option<&RetainedInput>,
) -> Result<ProviderFacts, AuditStoreError> {
    let route = retained.map(RetainedInput::route);
    let subscription = route == Some(AuditAuthenticationRoute::ClaudeSubscription);
    let client = matches!(
        route,
        Some(AuditAuthenticationRoute::ClaudeSubscription | AuditAuthenticationRoute::ClaudeApi)
    );
    let local = route == Some(AuditAuthenticationRoute::OllamaLocal);
    let attempt = sqlx::query(
        "SELECT attribution_failure,capture_finished FROM audit_attempts WHERE assignment_id=?",
    )
    .bind(assignment)
    .fetch_one(&mut *connection)
    .await?;
    let policy: Option<(String, String)> = sqlx::query_as(
        "SELECT policy_json,policy_digest FROM audit_attempt_inputs WHERE assignment_id=?",
    )
    .bind(assignment)
    .fetch_optional(&mut *connection)
    .await?;
    let metadata_digest = if let Some((raw, digest)) = policy {
        if sha256_hex(raw.as_bytes()) != digest {
            return Err(AuditStoreError::InvalidPersistedState(
                "retained report policy digest mismatch".to_owned(),
            ));
        }
        // Legacy inputs can be opaque; do not invent metadata for those inputs.
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|value| value["model_metadata_digest"].as_str().map(str::to_owned))
    } else {
        None
    };
    let usage: Option<String> =
        sqlx::query_scalar("SELECT usage_json FROM reviewer_assignments WHERE id=?")
            .bind(assignment)
            .fetch_one(&mut *connection)
            .await?;
    let native_usage = if client {
        Fact::NotApplicable
    } else {
        Fact::from_option(
            usage
                .map(|raw| serde_json::from_str::<NativeUsage>(&raw))
                .transpose()?,
            "no native usage counters retained at selection",
        )
    };
    let identity_evidence = sqlx::query(if subscription {
        "SELECT message_id AS request_id,'claude_client_message' AS transport,expected_model,observed_model,identity_state,'message' AS model_scope FROM audit_client_model_observations WHERE assignment_id=? ORDER BY execution_id,message_id"
    } else {
        "SELECT request_id,transport,expected_model,observed_model,identity_state,model_scope FROM audit_request_admissions WHERE assignment_id=? ORDER BY admitted_at_ms,request_id"
    }).bind(assignment).fetch_all(&mut *connection).await?.into_iter().map(|row| Ok(AuditGenerationIdentity {
        request_id: row.try_get("request_id")?, transport: row.try_get("transport")?,
        expected_model: row.try_get("expected_model")?, observed_model: row.try_get("observed_model")?,
        state: row.try_get("identity_state")?, evidence_scope: row.try_get("model_scope")?,
    })).collect::<Result<Vec<_>, AuditStoreError>>()?;
    let receipts = sqlx::query("SELECT request_id,operation_id,execution_id,transport,admitted_at_ms,send_started_at_ms,confirmed_at_ms,provider_response_id FROM audit_request_admissions WHERE assignment_id=? ORDER BY admitted_at_ms,request_id")
        .bind(assignment).fetch_all(&mut *connection).await?.into_iter().map(|row| Ok(GenerationReceipt {
            request_id: row.try_get("request_id")?, operation_id: row.try_get("operation_id")?, execution_id: row.try_get("execution_id")?,
            transport: row.try_get("transport")?, admitted_at_ms: row.try_get("admitted_at_ms")?, send_started_at_ms: row.try_get("send_started_at_ms")?,
            confirmed_at_ms: row.try_get("confirmed_at_ms")?, provider_response_id: row.try_get("provider_response_id")?,
        })).collect::<Result<Vec<_>, AuditStoreError>>()?;
    let client_terminal = if subscription {
        let row = sqlx::query("SELECT operation_id,execution_id,terminal_id,successful,verified,result_digest FROM audit_client_terminal_evidence WHERE assignment_id=?")
            .bind(assignment).fetch_optional(&mut *connection).await?;
        Fact::from_option(
            row.map(|row| {
                Ok::<_, AuditStoreError>(ClientTerminal {
                    operation_id: row.try_get("operation_id")?,
                    execution_id: row.try_get("execution_id")?,
                    terminal_id: row.try_get("terminal_id")?,
                    successful: row.try_get("successful")?,
                    verified: row.try_get("verified")?,
                    result_digest: row.try_get("result_digest")?,
                })
            })
            .transpose()?,
            "no retained client terminal evidence",
        )
    } else {
        Fact::NotApplicable
    };
    let admission = retained.map(|input| if subscription {
        "Bound successful client terminal and verified per-message identity; no Audex per-generation admission. Provider-internal corrective requests may occur."
    } else if input.version == 2 || input.require_served_identity {
        "Strict per-generation Audex admission and verified served-model identity; no automatic inference replay."
    } else {
        "Legacy native admission policy; served-model identity was not required."
    }.to_owned());
    Ok(ProviderFacts {
        executor: Fact::from_option(
            retained.map(RetainedInput::adapter),
            "no retained executor binding",
        ),
        route: Fact::from_option(route, "no retained route binding"),
        admission_policy: Fact::from_option(
            admission,
            "legacy input has no typed admission policy",
        ),
        expected_served_model: Fact::from_option(
            retained.map(|i| i.expected_model().to_owned()),
            "no retained expected model",
        ),
        identity_evidence,
        attribution_failure: attempt.try_get("attribution_failure")?,
        retained_input_version: Fact::from_option(
            retained.map(|i| i.version),
            "no versioned retained input",
        ),
        retained_bindings: Fact::from_option(
            retained.and_then(|i| i.bindings.clone()),
            "legacy input has no versioned policy bindings",
        ),
        versions: VersionBindings {
            client_version: applicable(client),
            client_executable_digest: applicable(client),
            model_version: Fact::unavailable(
                "no independent model version retained; see requested, resolved and observed identities",
            ),
            model_metadata_digest: Fact::from_option(
                metadata_digest,
                "no model metadata digest retained",
            ),
            backend_version: Fact::unavailable("backend version not retained"),
        },
        subscription: SubscriptionFacts {
            repair_disclosure: if subscription {
                Fact::known("Subscription permits provider-internal corrective requests without Audex generation admission; occurrence requires separate evidence.".to_owned())
            } else {
                Fact::NotApplicable
            },
            repair_occurrences: applicable(subscription),
        },
        ollama: OllamaFacts {
            model_digest: applicable(local),
            context_tokens: applicable(local),
            output_tokens: applicable(local),
            truncation_detection: applicable(local),
            truncation_observed: applicable(local),
            resource_tag: applicable(local),
            resource_ownership: applicable(local),
            resource_release: applicable(local),
        },
        native_usage,
        usage: Fact::unavailable(
            "per-generation token and cost evidence is not retained by current native capture",
        ),
        receipts,
        client_terminal,
        capture_finished: attempt.try_get("capture_finished")?,
    })
}

fn applicable<T>(applies: bool) -> Fact<T> {
    if applies {
        Fact::unavailable("no retained evidence at selection")
    } else {
        Fact::NotApplicable
    }
}
