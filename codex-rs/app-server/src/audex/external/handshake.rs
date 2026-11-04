//! No audit content crosses this bounded control exchange. The owned child guard
//! remains live throughout; cancellation, timeout or refusal drops it normally.
use super::super::store::{AuditStore, admission::AdmissionIdentity};
use super::owner::Spawned;
use super::policy::{Policy, validate_document, validate_effective};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::time::Duration;
use tokio::io::unix::AsyncFd;

const MAX_RESPONSE: usize = 256 * 1024;

pub(super) async fn qualify(
    child: Spawned,
    policy: &mut Policy,
    model: &str,
    billing_source: &str,
    store: &AuditStore,
    identity: &AdmissionIdentity,
) -> anyhow::Result<Spawned> {
    let Spawned {
        owned,
        stdin,
        stdout,
        stderr,
    } = child;
    let mut stdin = AsyncFd::new(stdin)?;
    let mut stdout = AsyncFd::new(stdout)?;
    let mut stderr = AsyncFd::new(stderr)?;
    let exchange = async {
        for (number, subtype) in [
            "initialize",
            "get_usage",
            "get_settings",
            "get_hooks_listing",
            "get_status",
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = format!("audex-policy-{}-{number}", identity.execution_id);
            let mut control = json!({"subtype":subtype});
            if subtype == "get_usage" {
                // Read only the client's tier/session usage; do not scan transcripts.
                control["skip_behaviors"] = true.into();
            }
            let mut request = serde_json::to_vec(
                &json!({"type":"control_request","request_id":request_id,"request":control}),
            )?;
            request.push(b'\n');
            let mut offset = 0;
            while offset < request.len() {
                let mut ready = stdin.writable_mut().await?;
                match ready.try_io(|pipe| pipe.get_mut().write(&request[offset..])) {
                    Ok(Ok(0)) => anyhow::bail!("policy input closed"),
                    Ok(Ok(n)) => offset += n,
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => {}
                }
            }
            // Read exactly one record so no buffered byte is lost when the normal
            // collector takes ownership of these same pipes after qualification.
            let mut line = Vec::new();
            loop {
                let mut byte = [0u8];
                let mut ready = stdout.readable_mut().await?;
                match ready.try_io(|pipe| pipe.get_mut().read(&mut byte)) {
                    Ok(Ok(0)) => anyhow::bail!("policy response missing"),
                    Ok(Ok(_)) if byte[0] == b'\n' => break,
                    Ok(Ok(_)) => {
                        line.push(byte[0]);
                        anyhow::ensure!(
                            line.len() <= MAX_RESPONSE,
                            "policy response exceeds bound"
                        );
                    }
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => {}
                }
            }
            let value: Value = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("malformed policy response"))?;
            anyhow::ensure!(
                value["type"] == "control_response"
                    && value["response"]["subtype"] == "success"
                    && value["response"]["request_id"] == request_id,
                "unexpected pre-prompt client record"
            );
            let result = &value["response"]["response"];
            match subtype {
                "initialize" => validate_initialise(result, owned.identity.pid, billing_source)?,
                "get_usage" => {
                    validate_personal_tier(result, billing_source)?;
                    // get_usage can refresh credentials. Bind only its completed
                    // observation, then reject subsequent changes through submission.
                    policy.bind_credentials()?;
                }
                "get_settings" => validate_settings(result, model, policy)?,
                "get_hooks_listing" => validate_hooks(result)?,
                "get_status" => validate_status(result, billing_source, policy)?,
                _ => unreachable!(),
            }
        }
        policy.verify_before_prompt()?;
        anyhow::Ok(())
    };
    let monitor = async {
        let mut diagnostics = [0u8; 4096];
        let mut total = 0;
        let mut stderr_open = true;
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    anyhow::ensure!(store.renew_execution_lease(identity, chrono::Utc::now().timestamp_millis()).await?, "policy qualification authority stopped");
                }
                ready = stderr.readable_mut(), if stderr_open => {
                    let mut ready = ready?;
                    if let Ok(result) = ready.try_io(|pipe| pipe.get_mut().read(&mut diagnostics)) {
                        let n = result?;
                        stderr_open = n != 0;
                        total += n;
                        anyhow::ensure!(total <= 64 * 1024, "policy diagnostic bound exceeded");
                    }
                }
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::select! {
            result = exchange => result,
            result = monitor => result,
        }
    })
    .await??;
    Ok(Spawned {
        owned,
        stdin: stdin.into_inner(),
        stdout: stdout.into_inner(),
        stderr: stderr.into_inner(),
    })
}

fn validate_initialise(value: &Value, pid: u32, billing_source: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value["pid"] == pid
            && value["claude_code_version"] == super::config::CLIENT_VERSION
            && value["current_permission_mode"] == "dontAsk"
            && value["commands"] == json!([])
            && value["output_style"] == "default"
            && value["account"]["apiProvider"] == "firstParty",
        "client identity or subscription provider mismatch"
    );
    // The display name is only a consistency check. get_usage below returns the
    // raw tier, including when planDisplayName disguises a Team/Enterprise account.
    anyhow::ensure!(
        billing_source != "none"
            || matches!(
                value["account"]["subscriptionType"].as_str(),
                Some("Claude Pro" | "Claude Max")
            ),
        "subscription identity is indeterminate"
    );
    anyhow::ensure!(
        value["account"]
            .get("apiKeySource")
            .is_none_or(|v| v == billing_source),
        "API billing source in subscription handshake"
    );
    Ok(())
}

fn validate_personal_tier(value: &Value, billing_source: &str) -> anyhow::Result<()> {
    // 2.1.314 Szn -> mr -> Dn exposes the raw stored-login tier. The qualified
    // env_clear launch excludes mr's host/background-token fallbacks. With
    // CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 and no GB disk-cache override,
    // Wm/B$i/getFeatureValueWithSource disable live AND cached feature flags;
    // TC/mpr therefore cannot turn a personal tier into an unknown organisation.
    // Local preflight excludes forceLoginOrgUUID and unreadable authorities.
    // The tier is observed after get_usage's normal credential refresh; metadata
    // is bound immediately afterwards and must remain stable until submission.
    // Together these establish M/dE: unsupported_subscription and i4: prosumer_oauth
    // before any prompt. Status-row absence or get_settings success alone cannot.
    anyhow::ensure!(
        billing_source != "none"
            || matches!(value["subscription_type"].as_str(), Some("pro" | "max")),
        "personal subscription policy eligibility is indeterminate or unsupported"
    );
    Ok(())
}

fn validate_settings(value: &Value, model: &str, policy: &Policy) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.get("errors").is_none_or(|v| v == &json!([])),
        "effective policy has load errors"
    );
    validate_effective(&value["effective"])?;
    anyhow::ensure!(
        value["applied"]["model"] == model
            && value["applied"]["advisor"].is_null()
            && value["applied"]["ultracode"] == false
            && value["applied"]["ultracodeRequested"] == false,
        "unqualified applied client configuration"
    );
    let sources = value["sources"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("policy sources missing"))?;
    let mut user = false;
    let mut managed = false;
    for source in sources {
        match source["source"].as_str() {
            Some("userSettings") if !user && !managed => user = true,
            Some("policySettings") if user && !managed => managed = true,
            _ => anyhow::bail!("unexpected or duplicate settings source"),
        }
        validate_document(&source["settings"])?;
    }
    anyhow::ensure!(
        user && managed == policy.has_managed,
        "effective policy source inventory incomplete"
    );
    Ok(())
}

fn validate_hooks(value: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(
        value["hooks"] == json!([])
            && value["events"] == json!([])
            && value["policy"]["allDisabled"] == true
            && value["policy"]["policyHookCount"] == 0
            && value["policy"]
                .get("policyUnreadable")
                .is_none_or(|v| v == false)
            && value.get("errors").is_none_or(|v| v == &json!([])),
        "configured hooks or unverifiable hook policy"
    );
    Ok(())
}

fn validate_status(value: &Value, _billing_source: &str, policy: &Policy) -> anyhow::Result<()> {
    let sections = value["sections"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("policy status missing"))?;
    let mut source_seen = false;
    for section in sections {
        let rows = section["rows"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("policy status rows missing"))?;
        for row in rows {
            match row["label"].as_str() {
                Some("Setting sources") => {
                    anyhow::ensure!(!source_seen, "duplicate managed source authority");
                    source_seen = true;
                    // Personal eligibility excludes remote policy. Match the
                    // remaining tier to the read-only local/Windows preflight.
                    let source = row["value"].as_str();
                    anyhow::ensure!(
                        if !policy.has_managed {
                            source == Some("User settings")
                        } else if policy.windows_registry {
                            source == Some("User settings, Enterprise managed settings (HKLM)")
                        } else if policy.windows_user_registry {
                            source == Some("User settings, Enterprise managed settings (HKCU)")
                        } else {
                            matches!(
                                source,
                                Some(
                                    "User settings, Enterprise managed settings (file)"
                                        | "User settings, Enterprise managed settings (drop-ins)"
                                        | "User settings, Enterprise managed settings (file + drop-ins)"
                                )
                            )
                        },
                        "unqualified managed source authority"
                    );
                }
                // C6's existing compiled-test-only, network-isolated fake API
                // has no OAuth profile and explicitly cannot fetch remote policy.
                // Permit only its two pinned diagnostics. This is not compiled
                // into production and is never subscription authority evidence.
                #[cfg(test)]
                Some("Managed settings (remote)")
                    if _billing_source == "ANTHROPIC_API_KEY"
                        && row["value"]
                            == "not fetched — not available with a custom ANTHROPIC_BASE_URL" => {
                }
                #[cfg(test)]
                Some("Organization policy")
                    if _billing_source == "ANTHROPIC_API_KEY"
                        && row["value"] == "not fetched with a custom ANTHROPIC_BASE_URL" => {}
                Some("Managed settings (remote)" | "Skipped sources" | "Organization policy") => {
                    anyhow::bail!("remote or organisation policy is indeterminate");
                }
                _ => {}
            }
        }
    }
    anyhow::ensure!(source_seen, "managed source authority missing");
    Ok(())
}
