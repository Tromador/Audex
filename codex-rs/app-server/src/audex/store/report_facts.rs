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
    let client_terminal = if client {
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
    let launch: Option<(String, Option<String>)> = sqlx::query_as("SELECT client_version,executable_digest FROM audit_client_launches l JOIN operations o ON o.id=l.operation_id WHERE o.assignment_id=?")
        .bind(assignment).fetch_optional(&mut *connection).await?;
    let mut client_usage = Vec::new();
    if client {
        let mut rows: Vec<(String, String, String)> = sqlx::query_as("SELECT message_id,usage_json,'claude_client_message' FROM audit_client_model_observations WHERE assignment_id=? AND usage_json IS NOT NULL
            UNION ALL SELECT terminal_id,usage_json,'claude_client_terminal_aggregate_not_generation' FROM audit_client_terminal_evidence WHERE assignment_id=? AND usage_json IS NOT NULL ORDER BY 1,3")
            .bind(assignment).bind(assignment).fetch_all(&mut *connection).await?;
        let failures: Vec<String> = sqlx::query_scalar("SELECT a.payload FROM audit_activity a JOIN audit_attempts t ON t.assignment_id=a.assignment_id
            WHERE a.assignment_id=? AND a.kind='status' AND a.producer_key LIKE 'client-terminal-failed:%'
            AND a.sequence<=COALESCE(t.output_cutoff,t.output_sequence) ORDER BY a.sequence")
            .bind(assignment).fetch_all(&mut *connection).await?;
        for raw in failures {
            let receipt: serde_json::Value = serde_json::from_str(&raw)?;
            if let Some(id) = receipt["terminal_id"].as_str() {
                rows.push((
                    id.to_owned(),
                    receipt["usage"].to_string(),
                    "claude_client_failed_terminal_aggregate_not_generation".into(),
                ));
            }
        }
        for (request_id, raw, evidence_source) in rows {
            let usage: super::client_stream::ClientUsage = serde_json::from_str(&raw)?;
            let output_tokens = if evidence_source == "claude_client_message" {
                // Also protects unfrozen observations retained before this qualification.
                Fact::unavailable("client per-message output_tokens is a placeholder")
            } else {
                Fact::from_option(usage.output_tokens, "not observed")
            };
            client_usage.push(GenerationUsage {
                request_id,
                evidence_source,
                input_tokens: Fact::from_option(usage.input_tokens, "not observed"),
                cached_input_tokens: Fact::from_option(
                    usage.cache_read_input_tokens,
                    "not observed",
                ),
                output_tokens,
                reasoning_tokens: Fact::unavailable("not observed"),
                cost: Fact::from_option(
                    usage.estimated_cost_usd.map(|amount| Cost {
                        amount,
                        currency: "USD".into(),
                        measured: false,
                    }),
                    "no cost observation retained",
                ),
            });
        }
    }
    let policy_notices: Vec<String> = if subscription {
        sqlx::query_scalar("SELECT a.payload FROM audit_activity a JOIN audit_attempts t ON t.assignment_id=a.assignment_id WHERE a.assignment_id=? AND a.kind='status' AND a.producer_key LIKE 'client-policy:%' AND a.sequence<=COALESCE(t.output_cutoff,t.output_sequence) ORDER BY a.sequence")
            .bind(assignment).fetch_all(&mut *connection).await?
    } else {
        Vec::new()
    };
    let repair_count = policy_notices
        .iter()
        .filter(|raw| {
            serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .is_some_and(|v| v["label"] == "client-initiated repair occurred")
        })
        .count() as u64;
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
            client_version: if client {
                Fact::from_option(
                    launch.as_ref().map(|l| l.0.clone()),
                    "no launch version retained",
                )
            } else {
                Fact::NotApplicable
            },
            client_executable_digest: if client {
                Fact::from_option(launch.and_then(|l| l.1), "no executable digest retained")
            } else {
                Fact::NotApplicable
            },
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
                let mut disclosure = "Subscription permits the qualified client corrective requests without Audex generation admission. Interruption is best effort; initiated sends and remote charges are unconfirmed.".to_owned();
                for notice in &policy_notices {
                    disclosure.push('\n');
                    disclosure.push_str(notice);
                }
                Fact::known(disclosure)
            } else {
                Fact::NotApplicable
            },
            repair_occurrences: if subscription && !policy_notices.is_empty() {
                if repair_count == policy_notices.len() as u64 {
                    Fact::known(repair_count)
                } else {
                    Fact::unavailable(
                        "client-policy anomalies were observed; the repair-occurrence count cannot be established",
                    )
                }
            } else {
                applicable(subscription)
            },
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
        usage: if client && !client_usage.is_empty() {
            Fact::known(client_usage)
        } else {
            Fact::unavailable(
                "per-generation token and cost evidence is not retained by current native capture",
            )
        },
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
