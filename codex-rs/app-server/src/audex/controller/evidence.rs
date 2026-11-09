//! Conversation projections of retained evidence. No execution or live-source access.
use super::*;
use base64::Engine;
use serde_json::{Value, json};

impl AuditController {
    pub(super) async fn evidence(&self, params: AuditEvidenceParams) -> anyhow::Result<Value> {
        let store = self.store().await?;
        let run = store.load_run_metadata(&params.run_id).await?;
        let (_, _, _, manifest_digest) = store.attempt_input(&run.operation_id).await?;
        let identity = json!({"runId":run.run_id,"snapshotId":run.snapshot_id,"manifestDigest":manifest_digest});
        if let AuditEvidenceQuery::Source {
            snapshot,
            path,
            offset,
            max_bytes,
        } = params.query
        {
            anyhow::ensure!(
                snapshot == run.snapshot_id,
                "snapshot does not belong to this run"
            );
            anyhow::ensure!(max_bytes != Some(0), "maxBytes must be positive");
            let snapshot = self.snapshots().open(&SnapshotId::parse(snapshot)?).await?;
            anyhow::ensure!(
                sha256_hex(&snapshot.manifest().to_pretty_bytes()?) == manifest_digest,
                "snapshot manifest binding mismatch"
            );
            let path = SnapshotPath::new(path)?;
            let access = snapshot.access(super::super::snapshot::AccessLimits::default());
            // Reuse the existing whole-file hash verification, including manifest-only paths.
            let bytes = access.verified_content(&path).await?;
            let start = usize::try_from(offset)?;
            anyhow::ensure!(start <= bytes.len(), "source offset exceeds file size");
            let end = max_bytes
                .map(|size| start.saturating_add(size).min(bytes.len()))
                .unwrap_or(bytes.len());
            let part = &bytes[start..end];
            let (encoding, content) = match std::str::from_utf8(part) {
                Ok(text) => ("utf8", text.to_owned()),
                Err(_) => (
                    "base64",
                    base64::engine::general_purpose::STANDARD.encode(part),
                ),
            };
            let line = |offset: usize| 1 + bytes[..offset].iter().filter(|b| **b == b'\n').count();
            return Ok(
                json!({"identity":identity,"path":path,"sha256":sha256_hex(&bytes),
                "offset":start,"endOffset":end,"totalBytes":bytes.len(),"eof":end==bytes.len(),"encoding":encoding,"content":content,
                "startLine":line(start),"endLine":if end>start {Some(line(end-1))} else {None},"linesVerified":true}),
            );
        }
        // Preserve conversational progress reads while the immutable collection is pending.
        if matches!(params.query, AuditEvidenceQuery::Reviewers { .. })
            && !matches!(
                run.phase,
                RunPhase::Completed
                    | RunPhase::CompletedWithFailures
                    | RunPhase::Failed
                    | RunPhase::Cancelled
            )
        {
            return Ok(
                json!({"identity":identity,"phase":phase(run.phase),"findingsAvailable":false,
                "status":"Audit still in progress; retained findings become available after publication. This read starts no inference."}),
            );
        }
        anyhow::ensure!(
            matches!(
                run.phase,
                RunPhase::Completed
                    | RunPhase::CompletedWithFailures
                    | RunPhase::Failed
                    | RunPhase::Cancelled
            ),
            "retained findings are not yet published; use audit_list to check progress"
        );
        let mut reviewers = Vec::new();
        if let Some(selection) = store.conversation_selection(&run.run_id).await? {
            anyhow::ensure!(
                selection.run_id == run.run_id
                    && selection.snapshot_id == run.snapshot_id
                    && selection.manifest_digest == manifest_digest,
                "retained selection identity mismatch"
            );
            for reviewer in selection.reviewers {
                let findings = reviewer
                    .completed
                    .as_ref()
                    .map(|c| serde_json::to_value(&c.findings))
                    .transpose()?
                    .unwrap_or(json!([]));
                reviewers.push((json!({"reviewerId":reviewer.reviewer_id,"assignmentId":reviewer.assignment_id,
                    "number":reviewer.reviewer_ordinal+1,"provider":reviewer.provider,"model":reviewer.resolved_model,
                    "requestedModel":reviewer.requested_model,"observedModel":reviewer.observed_model,"modelIdentityVerified":reviewer.observation_verified,
                    "state":reviewer.assignment_state,"disposition":reviewer.disposition,
                    "findingCount":findings.as_array().unwrap().len(),
                    "structuredFindingsAvailable":reviewer.completed.is_some(),
                    "opinionAvailable":reviewer.opinion.is_some()}), findings));
            }
        } else {
            // Older single-review reports predate frozen collection selections.
            let retained = store.load_run(&run.run_id).await?;
            reviewers.push((json!({"reviewerId":retained.reviewer_id,"assignmentId":retained.assignment_id,
                "number":1,"provider":retained.provider,"model":retained.model,"state":format!("{:?}",retained.assignment_state),
                "findingCount":retained.findings.len(),"structuredFindingsAvailable":retained.assignment_state == AssignmentState::Succeeded}),serde_json::to_value(retained.findings)?));
        }
        match params.query {
            AuditEvidenceQuery::Reviewers { offset, limit } => page(
                identity,
                reviewers.into_iter().map(|(r, _)| r).collect(),
                offset,
                limit,
            ),
            AuditEvidenceQuery::Findings {
                reviewer,
                offset,
                limit,
            } => {
                let selected = match reviewer.as_deref() {
                    Some(id) => vec![select(&reviewers, id)?],
                    None => reviewers.iter().collect(),
                };
                let items = selected
                    .iter()
                    .flat_map(|(attribution, findings)| {
                        findings.as_array().unwrap().iter().map(move |finding| {
                            let mut item = finding.clone();
                            item["number"] = json!(finding["ordinal"].as_i64().unwrap() + 1);
                            item["reviewerId"] = attribution["reviewerId"].clone();
                            item["assignmentId"] = attribution["assignmentId"].clone();
                            item
                        })
                    })
                    .collect();
                let mut result = page(identity, items, offset, limit)?;
                if reviewer.is_some() {
                    result["reviewer"] = selected[0].0.clone();
                } else {
                    // Include empty/failed reviewers as well as those contributing findings.
                    result["reviewers"] =
                        json!(selected.iter().map(|(r, _)| r).collect::<Vec<_>>());
                }
                Ok(result)
            }
            AuditEvidenceQuery::Finding {
                reviewer,
                number,
                offset,
                max_bytes,
            } => {
                let (attribution, findings) = select(&reviewers, &reviewer)?;
                let finding = number
                    .checked_sub(1)
                    .and_then(|i| findings.as_array().unwrap().get(i))
                    .ok_or_else(|| anyhow::anyhow!("finding number is out of range (one based)"))?;
                let text = serde_json::to_string(finding)?;
                anyhow::ensure!(
                    offset <= text.len() && text.is_char_boundary(offset),
                    "invalid finding continuation offset"
                );
                anyhow::ensure!(max_bytes != Some(0), "maxBytes must be positive");
                let mut end = max_bytes
                    .map(|size| offset.saturating_add(size).min(text.len()))
                    .unwrap_or(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                anyhow::ensure!(
                    end > offset || offset == text.len(),
                    "maxBytes does not fit the next UTF-8 character"
                );
                Ok(
                    json!({"identity":identity,"reviewer":attribution,"number":number,"findingId":finding["id"],
                    "finding":if offset==0 && end==text.len() {Some(finding)} else {None},
                    "encoding":"json-utf8","content":&text[offset..end],"offset":offset,"nextOffset":if end<text.len() {Some(end)} else {None},
                    "totalBytes":text.len(),"eof":end==text.len()}),
                )
            }
            AuditEvidenceQuery::Source { .. } => unreachable!(),
        }
    }
}

fn select<'a>(reviewers: &'a [(Value, Value)], id: &str) -> anyhow::Result<&'a (Value, Value)> {
    reviewers
        .iter()
        .find(|(r, _)| r["reviewerId"] == id)
        .ok_or_else(|| {
            anyhow::anyhow!("reviewer not in this run; call audit_read with view=reviewers")
        })
}

fn page(
    identity: Value,
    items: Vec<Value>,
    offset: usize,
    limit: Option<usize>,
) -> anyhow::Result<Value> {
    anyhow::ensure!(offset <= items.len(), "page offset exceeds item count");
    anyhow::ensure!(limit != Some(0), "limit must be positive");
    let next = limit
        .map(|size| offset.saturating_add(size).min(items.len()))
        .unwrap_or(items.len());
    let selected = &items[offset..next];
    Ok(
        json!({"identity":identity,"items":selected,"total":items.len(),"offset":offset,
        "nextOffset":if next<items.len() {Some(next)} else {None}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_controls_paging_without_a_content_size_ceiling() {
        for title in ["\"".repeat(512), "\0".repeat(512)] {
            let items: Vec<_> = (0..8).map(|number| json!({"id":"f".repeat(100),"number":number,
                "title":title,"severity":"high","confidence":"medium","verification_state":"unverified"})).collect();
            let identity = json!({"runId":"r".repeat(50),"snapshotId":"s".repeat(90),"manifestDigest":"d".repeat(64)});
            let all = page(identity.clone(), items.clone(), 0, None).unwrap();
            assert_eq!(all["items"], json!(items));
            assert!(all["nextOffset"].is_null());
            let mut offset = 0;
            loop {
                let value = page(identity.clone(), items.clone(), offset, Some(5)).unwrap();
                assert_eq!(
                    value["items"].as_array().unwrap().len(),
                    (8 - offset).min(5)
                );
                offset += value["items"].as_array().unwrap().len();
                if value["nextOffset"].is_null() {
                    break;
                }
                assert_eq!(value["nextOffset"], offset);
            }
            assert_eq!(offset, 8);
        }
    }
}
