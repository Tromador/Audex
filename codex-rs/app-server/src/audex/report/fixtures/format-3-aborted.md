# Audex Audit Report

- Report format: `3`
- Run: `audex-run-01a1a133-f63b-7732-ac8a-2aeac57b67d4`
- Snapshot: `audex-source-v1:sha256:c54bb7fd42bbc19f0031913461ff7293a981dc311ec630e7301f87860c5ea6c0`
- Selection: `audex-run-01a1a133-f63b-7732-ac8a-2aeac57b67d4:selection:1`
- Outcome: `aborted_by_operator`

## Reviewer 1: audit-test

- Reviewer: `audex-reviewer-01a1a133-f63c-7e92-ae8b-12409c7ee871`
- Assignment: `audex-assignment-01a1a133-f63b-7732-ac8a-2af056e0feab`
- Operation: `audex-operation-01a1a133-f63b-7732-ac8a-2b02fa7b3358`
- Provider: `audit_fixture`
- Requested model: `audit-test`
- Resolved model: `audit-test`
- Observed served model: `audit-test`
- Disposition: `active`
- Execution state: `succeeded`
- Attempt: `1`
- Completeness: completed
- Canonical result digest: `69a388653f1bffb2546e7591981780ddeadd7167b965e19f02397424d8ecfeb1`

### Summary

> CLI fixture audit

### Findings

No findings were reported.

## Reviewer 2: audit-test

- Reviewer: `s1-private-reviewer`
- Assignment: `s1-private-assignment`
- Operation: `s1-private-operation`
- Provider: `audit_fixture`
- Requested model: `audit-test`
- Resolved model: `audit-test`
- Observed served model: `unavailable; unverified`
- Disposition: `cancelled`
- Execution state: `failed`
- Attempt: `1`

**Provider caveat**

> Served-model identity was not verified for this contribution.

**Provider caveat**

> Attribution failed; received material is unvalidated history, not accepted findings.
- Completeness: incomplete / excluded; no accepted completed opinion

**Exclusion / stop reason**

> whole-audit Abort

**Execution failure reason**

> {"version":1,"stage":"reviewer_turn","cause":"other"}
- Received prefix cutoff: `3`
- Prefix digest: `4cf1f02f6e596f893c4a9d3417c96761d945c3ac4e65c42ab12dae583401f67d`

**Incomplete assistant output (unvalidated; not findings)**

> PRIVATE_PEER_PRE_ABORT_PARTIAL {

**Source/tool activity and status (not findings)**


**status**

> execution_error

**status**

> turn_completed

