//! Provider-neutral extraction of an already authoritative, completed opinion.
//! Only a leading UTF-8 BOM and a whole-response JSON fence are removed. Neither
//! identities nor review content are repaired, and raw bytes are never replaced.
use super::*;
use codex_app_server_protocol::{
    AuditExtractionStatus, AuditResultExtraction, AuditResultTransformation,
};

pub(in crate::audex) struct ExtractedOpinion {
    pub metadata: AuditResultExtraction,
    pub structured: Option<ValidatedAuditResult>,
}

pub(in crate::audex) fn extract(
    raw: &[u8],
    assignment: &str,
    manifest: &SnapshotManifestV1,
) -> Result<ExtractedOpinion, ResultValidationError> {
    if raw.len() > MAX_RAW_RESULT_BYTES {
        return Err(ResultValidationError::TooLarge);
    }
    let text = std::str::from_utf8(raw).map_err(|_| ResultValidationError::InvalidEncoding)?;
    if text.trim().trim_start_matches('\u{feff}').trim().is_empty() {
        return Err(ResultValidationError::Empty);
    }
    let mut metadata = AuditResultExtraction {
        version: 1,
        raw_digest: sha256_hex(raw),
        status: AuditExtractionStatus::ValidatedStructured,
        parse_digest: sha256_hex(raw),
        transformations: Vec::new(),
        validation_error: None,
    };
    if let Ok(structured) = parse_and_validate(raw, assignment, manifest) {
        return Ok(ExtractedOpinion {
            metadata,
            structured: Some(structured),
        });
    }
    let mut input = text;
    if let Some(without_bom) = input.strip_prefix('\u{feff}') {
        input = without_bom;
        metadata
            .transformations
            .push(AuditResultTransformation::Utf8Bom);
    }
    // Optional outer ASCII whitespace belongs to the envelope, not to the opinion.
    // No prose, arbitrary block search, other language tags or multiple blocks.
    let envelope = input.trim_ascii();
    if let Some(body) = envelope
        .strip_prefix("```json\r\n")
        .or_else(|| envelope.strip_prefix("```json\n"))
        && let Some(inner) = body
            .strip_suffix("\r\n```")
            .or_else(|| body.strip_suffix("\n```"))
    {
        input = inner;
        metadata
            .transformations
            .push(AuditResultTransformation::MarkdownJsonFence);
    }
    metadata.parse_digest = sha256_hex(input.as_bytes());
    match parse_and_validate(input.as_bytes(), assignment, manifest) {
        Ok(structured) => {
            metadata.status = AuditExtractionStatus::DeterministicallyNormalised;
            Ok(ExtractedOpinion {
                metadata,
                structured: Some(structured),
            })
        }
        Err(error) => {
            metadata.status = AuditExtractionStatus::UnstructuredOpinion;
            // Diagnostics are bounded independently of the full immutable opinion.
            metadata.validation_error = Some(error.to_string().chars().take(2048).collect());
            Ok(ExtractedOpinion {
                metadata,
                structured: None,
            })
        }
    }
}
