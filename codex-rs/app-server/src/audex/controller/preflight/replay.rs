//! An identical Start observes its original approval, even if configuration is now unavailable.
use super::*;

pub(in crate::audex::controller) fn request_identity(
    params: &AuditStartParams,
    reviewers: &[ReviewerSelection],
    snapshot: &SnapshotId,
    preflight: Option<&AuditPreflightResponse>,
) -> anyhow::Result<String> {
    let mut identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
        "version":1,"snapshotId":snapshot,"brief":params.brief,
        "provider":reviewers[0].provider,"model":reviewers[0].model,"reasoningEffort":reviewers[0].reasoning_effort,
    }))?);
    if reviewers.len() != 1 {
        identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
            "snapshotId":snapshot,"brief":params.brief,"reviewers":reviewers,
        }))?);
    }
    if params.scheduling != AuditScheduling::Automatic
        || reviewers.iter().any(|r| r.expected_served_model.is_some())
    {
        identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
            "base":identity,"scheduling":params.scheduling,"reviewers":reviewers,
        }))?);
    }
    if reviewers
        .iter()
        .any(|r| r.authentication_route == AuditAuthenticationRoute::ClaudeSubscription)
    {
        identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
            "base":identity,"reviewers":reviewers,"preflight":preflight.map(|p|&p.identity),
        }))?);
    }
    Ok(identity)
}

impl AuditController {
    pub(super) async fn retained_preflight(
        &self,
        params: &AuditStartParams,
        selections: &[ReviewerSelection],
        manifest: &super::super::super::snapshot::SnapshotManifestV1,
    ) -> anyhow::Result<Option<AuditPreflightResponse>> {
        let Some(retained) = AuditStore::read_commission(&self.root, &params.request_key).await?
        else {
            return Ok(None);
        };
        let conflict = || {
            super::super::super::store::AuditStoreError::IdempotencyConflict(
                params.request_key.clone(),
            )
        };
        if retained.inputs.len() != selections.len() {
            return Err(conflict().into());
        }
        let inputs = retained
            .inputs
            .iter()
            .map(|(raw, _)| super::super::super::retained::RetainedInput::decode(raw))
            .collect::<anyhow::Result<Vec<_>>>()?;
        if inputs.iter().all(|input| input.version == 1) {
            // Legacy requests retain their Start identity but no configuration
            // bindings. Prove that identity without consulting today's configuration;
            // disclose only the known request bindings and the existing run.
            if request_identity(params, selections, &manifest.snapshot_id, None)?
                != retained.identity
            {
                return Err(conflict().into());
            }
            let mut response = response(params, manifest, Vec::new())?;
            response.existing_run_id = Some(retained.run_id);
            return Ok(Some(response));
        }
        let mut reviewers = Vec::new();
        for (((_, policy), input), selection) in retained.inputs.iter().zip(&inputs).zip(selections)
        {
            let binding = input
                .bindings
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("retained approval has no preflight bindings"))?;
            let policy: serde_json::Value = serde_json::from_str(policy)?;
            if input.provider != selection.provider
                || input.model != selection.model
                || input.adapter() != selection.execution_adapter
                || input.route() != selection.authentication_route
                || input.expected_served_model != selection.expected_served_model
                || binding.snapshot_id != manifest.snapshot_id.to_string()
                || binding.manifest_digest != sha256_hex(&manifest.to_pretty_bytes()?)
                || binding.brief_digest != sha256_hex(params.brief.as_bytes())
                || binding.context_digest
                    != sha256_hex(&serde_json::to_vec(&manifest.identity.context_documents)?)
                || binding.scheduling != params.scheduling
            {
                return Err(conflict().into());
            }
            let resolved = input
                .resolved_model
                .clone()
                .unwrap_or_else(|| input.model.clone());
            let client = policy
                .get("subscription")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?;
            reviewers.push(AuditReviewerBinding {
                selection: selection.wire(),
                resolved_model: resolved.clone(),
                expected_served_model: input.expected_served_model.clone().unwrap_or(resolved),
                reasoning_effort: input.reasoning_effort.as_ref().map(ToString::to_string),
                backend_configuration_identity: binding.backend_configuration_identity.clone(),
                native_configuration_identity: binding.native_configuration_identity.clone(),
                model_metadata_digest: policy["model_metadata_digest"].as_str().map(str::to_owned),
                client,
            });
        }
        let mut response = response(params, manifest, reviewers)?;
        if request_identity(params, selections, &manifest.snapshot_id, Some(&response))?
            != retained.identity
        {
            return Err(conflict().into());
        }
        response.existing_run_id = Some(retained.run_id);
        Ok(Some(response))
    }
}

pub(super) fn response(
    params: &AuditStartParams,
    manifest: &super::super::super::snapshot::SnapshotManifestV1,
    reviewers: Vec<AuditReviewerBinding>,
) -> anyhow::Result<AuditPreflightResponse> {
    let mut response = AuditPreflightResponse {
        protocol_version: AUDIT_PROTOCOL_VERSION,
        existing_run_id: None,
        identity: String::new(),
        snapshot_id: manifest.snapshot_id.to_string(),
        manifest_digest: sha256_hex(&manifest.to_pretty_bytes()?),
        brief_digest: sha256_hex(params.brief.as_bytes()),
        context_digest: sha256_hex(&serde_json::to_vec(&manifest.identity.context_documents)?),
        reviewers,
    };
    response.identity = sha256_hex(&serde_json::to_vec(&serde_json::json!({
        "requestKey":params.request_key,"scheduling":params.scheduling,"bindings":response,
    }))?);
    Ok(response)
}
