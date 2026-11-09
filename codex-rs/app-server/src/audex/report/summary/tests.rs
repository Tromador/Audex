use super::*;
use crate::audex::report::collection::{self, ReviewerInput};
use crate::audex::snapshot::manifest::{
    SnapshotEntryKindV1, SnapshotEntryV1, SourceFormV1, sha256_hex,
};
use crate::audex::snapshot::{CaptureLimits, SnapshotPath};
use serde_json::json;

fn fixture() -> (CollectionInput, SnapshotManifestV1) {
    let mut input =
        collection::decode(include_str!("../fixtures/format-3-completed.json")).unwrap();
    let template = serde_json::to_value(&input.reviewers[0]).unwrap();
    input.reviewers.clear();
    // Same defect, similar wording/different defect, contradiction, and a minority
    // claim are all retained separately for the subsequent Codex assessment.
    let claims = [
        (
            "Cache stays stale",
            "clear() fails to invalidate cached entries",
            "Old value is returned",
        ),
        (
            "Cache stays stale",
            "clear() fails to invalidate cached entries",
            "Old value is returned",
        ),
        (
            "Cache stays stale",
            "replace() retains the key for a different user",
            "Another user's value is returned",
        ),
        (
            "Cache is invalidated",
            "clear() always invalidates cached entries",
            "The stale-cache claim is disputed",
        ),
        (
            "Minority overflow",
            "increment() overflows at the integer maximum",
            "Unverified; no execution was performed",
        ),
    ];
    for (index, (title, mechanism, impact)) in claims.into_iter().enumerate() {
        let mut reviewer = template.clone();
        reviewer["reviewer_id"] = json!(format!("reviewer-{index}"));
        reviewer["reviewer_ordinal"] = json!(index);
        reviewer["resolved_model"] = json!(format!("model-{index}"));
        reviewer["completed"]["findings"] = json!([{
            "id":format!("finding-{index}"),"ordinal":0,"severity":"high","confidence":"low",
            "title":title,"mechanism":mechanism,"impact":impact,"remediation":null,
            "verification_state":"unverified","evidence":[{"ordinal":0,"lines_verified":index%2==0,
                "manifest_path":"example.rs","start_byte":0,"end_byte":2,"start_line":1,"end_line":1,
                "description":format!("Original evidence {index}")}]}]);
        input
            .reviewers
            .push(serde_json::from_value::<ReviewerInput>(reviewer).unwrap());
    }
    let manifest = SnapshotManifestV1::build(
        SourceFormV1::Snippet,
        CaptureLimits::default(),
        vec![SnapshotEntryV1 {
            path: SnapshotPath::new("example.rs").unwrap(),
            kind: SnapshotEntryKindV1::RegularFile,
            executable: false,
            size: 13,
            sha256: sha256_hex(b"fn main() {}\n"),
        }],
        vec![],
    )
    .unwrap();
    (input, manifest)
}

#[test]
fn every_claim_and_evidence_is_attributed_without_inventing_consensus_or_verification() {
    let (input, manifest) = fixture();
    let before = serde_json::to_vec(&input).unwrap();
    let summary = render(&input, &manifest);
    for index in 0..5 {
        assert!(summary.contains(&format!("reviewer=reviewer-{index}, number=1")));
        assert!(summary.contains(&format!("Original evidence {index}")));
    }
    assert_eq!(summary.matches("### 1.").count(), 5);
    assert_eq!(summary.matches("verification: unverified").count(), 5);
    assert!(summary.contains("Relationships between claims have not yet been assessed"));
    assert!(summary.contains("different user"));
    assert!(summary.contains("claim is disputed"));
    assert!(summary.contains("Minority overflow"));
    assert!(summary.contains("unverified reviewer lines"));
    assert!(!summary.contains("Policy digest"));
    assert!(!summary.contains("Input digest"));
    assert_eq!(serde_json::to_vec(&input).unwrap(), before);
}

#[test]
fn failed_empty_and_unstructured_reviewers_are_not_reported_as_clean_reviews() {
    let (mut input, manifest) = fixture();
    input.reviewers[0].completed = None;
    input.reviewers[0].assignment_state = "failed".into();
    input.reviewers[0].failure_code = Some("provider_unavailable".into());
    input.reviewers[1]
        .completed
        .as_mut()
        .unwrap()
        .findings
        .clear();
    input.reviewers[2].completed = None;
    input.reviewers[2].opinion = Some(crate::audex::report::collection::OpinionInput {
        extraction: serde_json::from_value(json!({"version":1,"rawDigest":"fixture","parseDigest":"fixture",
            "status":"unstructured_opinion","transformations":[],"validationError":"not structured"})).unwrap(),
        text: Some("Unstructured claim remains retrievable".into()),
    });
    let summary = render(&input, &manifest);
    assert!(summary.contains(r"provider\_unavailable"));
    assert!(summary.contains("No findings reported. This is not proof"));
    assert!(summary.contains("Unstructured claim remains retrievable"));
    assert!(summary.contains("incomplete output remains available"));
    assert!(summary.contains("/audit diagnostics"));
}

#[test]
fn presentation_excerpts_are_explicit_and_cannot_inject_terminal_or_markdown_actions() {
    let (mut input, manifest) = fixture();
    let finding = &mut input.reviewers[0].completed.as_mut().unwrap().findings[0];
    finding.mechanism = format!(
        "\x1b[2J[link](https://example.invalid){}END",
        "é".repeat(500)
    );
    let summary = render(&input, &manifest);
    assert!(!summary.contains('\x1b'));
    assert!(summary.contains("[excerpt; full text retained]"));
    assert!(summary.contains("\\[link\\]"));
    assert!(
        input.reviewers[0].completed.as_ref().unwrap().findings[0]
            .mechanism
            .ends_with("END")
    );
}
