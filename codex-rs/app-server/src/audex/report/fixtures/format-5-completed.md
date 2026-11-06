# Audex Audit Report

- Report format: `5`
- Run: `audex-run-01a1ac80-a8de-7f53-b899-0d5d38b20c26`
- Snapshot: `audex-source-v1:sha256:c65da42aece9bbb42ff8482ef0fe823d3e2956a8e4edefc72e575239ca46df40`
- Selection: `audex-run-01a1ac80-a8de-7f53-b899-0d5d38b20c26:selection:1`
- Outcome: `success`

## Reviewer 1: gpt-5.6-luna

- Reviewer: `audex-reviewer-01a1ac80-a8de-7f53-b899-0d8e1ca0d308`
- Assignment: `audex-assignment-01a1ac80-a8de-7f53-b899-0d6ab9032c88`
- Operation: `audex-operation-01a1ac80-a8de-7f53-b899-0d71f129bac5`
- Provider: `openai`
- Requested model: `gpt-5.6-luna`
- Resolved model: `gpt-5.6-luna`
- Observed served model: `gpt-5.6-luna`
- Disposition: `active`
- Execution state: `succeeded`
- Attempt: `1`
- Executor: native
- Route: daemon\_configured\_native
- Admission policy: Strict per-generation Audex admission and verified served-model identity; no automatic inference replay.
- Expected served model: gpt-5.6-luna
- Retained input version: 2
- Retained source and policy bindings: {"version":1,"snapshotId":"audex-source-v1:sha256:c65da42aece9bbb42ff8482ef0fe823d3e2956a8e4edefc72e575239ca46df40","manifestDigest":"716da5f0a15c21f6edbb19fd9eac5181219c812ec51db5d2575223ae883cf727","briefDigest":"a4b732b20085113cae8ebdad5451b819939dad4c9eaeb2230d6822de8c71eece","contextDigest":"4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945","policyDigest":"4b99561f3a9dc6906e70472fcbc27461f0015f2f17314092f110144c673a092d","backendConfigurationIdentity":"2430f1a2ad2982d0067885488a4c89e21ad1d7c83b115ba8f1b20acc88dfaea8","nativeConfigurationIdentity":"16ca9a4865c1ac98e84fea70e3950e900b6f6d28acf4ca293c1ac3b447a27307","scheduling":"automatic","toolExposure":\["audex\_source.list","audex\_source.read","audex\_source.search"\],"limits":{"requestRetries":0,"streamRetries":0,"automaticCompaction":false,"toolArgumentBytes":16384,"readBytes":65536,"searchQueryBytes":1024,"searchResults":100}}
- Client version binding: not applicable
- Client executable digest: not applicable
- Model version binding: unavailable (no independent model version retained; see requested, resolved and observed identities)
- Model metadata digest: 8ee75a369bf0e0085b522aabfc44001b54bec978bc1f2502cc6e02eb20dbb000
- Backend version binding: unavailable (backend version not retained)
- Subscription repair disclosure: not applicable
- Subscription repair occurrences: not applicable
- Ollama model digest: not applicable
- Ollama context token limit: not applicable
- Ollama output token limit: not applicable
- Ollama truncation detection: not applicable
- Ollama truncation observed: not applicable
- Local resource tag: not applicable
- Local resource ownership: not applicable
- Local resource release: not applicable
- Native aggregate and last usage counters: {"total\_token\_usage":{"input\_tokens":12880,"cached\_input\_tokens":8298,"cache\_write\_input\_tokens":4573,"output\_tokens":1078,"reasoning\_output\_tokens":541,"total\_tokens":13958},"last\_token\_usage":{"input\_tokens":4576,"cached\_input\_tokens":4281,"cache\_write\_input\_tokens":292,"output\_tokens":736,"reasoning\_output\_tokens":478,"total\_tokens":5312},"last\_usage\_includes\_reasoning":true,"model\_context\_window":258400}
- Native usage scope: retained runtime counters, which can include estimates; not per-generation attribution or billing evidence.
- Per-generation usage evidence: unavailable (per-generation token and cost evidence is not retained by current native capture)
- Client terminal evidence: not applicable
- Capture finished at selection: true
- Model identity evidence: {"requestId":"a09ca603-e58f-5e78-8bca-839be64693c3","transport":"native\_websocket","expectedModel":"gpt-5.6-luna","observedModel":"gpt-5.6-luna","evidenceScope":"generation","state":"verified"}
- Model identity evidence: {"requestId":"fa78c48c-6a4a-53f1-8015-6b1be4c7fbaa","transport":"native\_websocket","expectedModel":"gpt-5.6-luna","observedModel":"gpt-5.6-luna","evidenceScope":"generation","state":"verified"}
- Model identity evidence: {"requestId":"143e5860-932e-5bba-add9-2599d519e088","transport":"native\_websocket","expectedModel":"gpt-5.6-luna","observedModel":"gpt-5.6-luna","evidenceScope":"generation","state":"verified"}
- Generation receipt: {"request\_id":"a09ca603-e58f-5e78-8bca-839be64693c3","operation\_id":"audex-operation-01a1ac80-a8de-7f53-b899-0d71f129bac5","execution\_id":"01a1ac80-a927-7a42-bfed-cb8fc64dc2af","transport":"native\_websocket","admitted\_at\_ms":1793895476540,"send\_started\_at\_ms":1793895476544,"confirmed\_at\_ms":1793895476655,"provider\_response\_id":"resp\_0052b1f25d7a58d1016aecac3826d087d2b9a7d30cf2094c24"}
- Generation receipt: {"request\_id":"fa78c48c-6a4a-53f1-8015-6b1be4c7fbaa","operation\_id":"audex-operation-01a1ac80-a8de-7f53-b899-0d71f129bac5","execution\_id":"01a1ac80-a927-7a42-bfed-cb8fc64dc2af","transport":"native\_websocket","admitted\_at\_ms":1793895483612,"send\_started\_at\_ms":1793895483616,"confirmed\_at\_ms":1793895483722,"provider\_response\_id":"resp\_0052b1f25d7a58d1016aecac3c039487d2be53b91af1bacb77"}
- Generation receipt: {"request\_id":"143e5860-932e-5bba-add9-2599d519e088","operation\_id":"audex-operation-01a1ac80-a8de-7f53-b899-0d71f129bac5","execution\_id":"01a1ac80-a927-7a42-bfed-cb8fc64dc2af","transport":"native\_websocket","admitted\_at\_ms":1793895485189,"send\_started\_at\_ms":1793895485194,"confirmed\_at\_ms":1793895485295,"provider\_response\_id":"resp\_0052b1f25d7a58d1016aecac3dc5a487d28c3e718a679d6696"}
- Receipt limits: admission is not proof of send or provider receipt; send-start is only a possible wire attempt. Null receipt fields mean no evidence retained at selection. Usage and billing are not inferred from admission or receipt.
- Facts are frozen at report selection. Later operational receipts remain separate and cannot change this contribution or outcome.
- Extraction: validated_structured
- Raw output SHA-256: `18e9f19d11a07ab9d79d6f275d9f7d470f6862d93ecc4cd1512d54fdd7efc475`
- Parse input SHA-256: `18e9f19d11a07ab9d79d6f275d9f7d470f6862d93ecc4cd1512d54fdd7efc475`
- Deterministic transformations: []
- Completion and validated structure do not establish opinion correctness; assessment belongs to the operator.
- Completeness: completed opinion; validated structured extraction
- Raw result digest: `18e9f19d11a07ab9d79d6f275d9f7d470f6862d93ecc4cd1512d54fdd7efc475`

### Summary

    The implementation fails the empty-slice case because it unconditionally indexes the first element.

### Findings

#### 1. Empty slices panic instead of returning zero

- Finding ID: `audex-finding-v1:sha256:36f6f53b764e5f4b9bd085a07bd2537b9bd5c861dd3efd77022b17f9df541588`
- Severity: `medium`
- Confidence: `high`
- Verification: `unverified_model_claim`

**Mechanism**

    Unconditional indexing with values[0] is performed without checking whether the slice is empty.

**Impact**

    An empty input causes a runtime panic, violating the stated empty-slice contract.
**Evidence**

- `first_or_zero.rs` bytes `102..111`, verified lines `3..3`: The function indexes element zero unconditionally; calling first\_or\_zero(&\[\]) triggers an out-of-bounds panic instead of returning 0.

**Remediation**

    Return 0 when the slice is empty, otherwise return its first byte.


## Reviewer 2: claude-haiku-5-5

- Reviewer: `audex-reviewer-01a1ac80-a8e1-7700-90fa-cc248094e7c5`
- Assignment: `audex-assignment-01a1ac80-a8e1-7700-90fa-cc01343b0d7c`
- Operation: `audex-operation-01a1ac80-a8e1-7700-90fa-cc13931ef921`
- Provider: `anthropic`
- Requested model: `claude-haiku-5-5`
- Resolved model: `claude-haiku-5-5`
- Observed served model: `claude-haiku-5-5`
- Disposition: `active`
- Execution state: `succeeded`
- Attempt: `1`
- Executor: claude\_client
- Route: claude\_subscription
- Admission policy: Bound successful client terminal and verified per-message identity; no Audex per-generation admission. Provider-internal corrective requests may occur.
- Expected served model: claude-haiku-5-5
- Retained input version: 2
- Retained source and policy bindings: {"version":1,"snapshotId":"audex-source-v1:sha256:c65da42aece9bbb42ff8482ef0fe823d3e2956a8e4edefc72e575239ca46df40","manifestDigest":"716da5f0a15c21f6edbb19fd9eac5181219c812ec51db5d2575223ae883cf727","briefDigest":"a4b732b20085113cae8ebdad5451b819939dad4c9eaeb2230d6822de8c71eece","contextDigest":"4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945","policyDigest":"bb7cf0d090b7c6bb0ced1add1c8e65c9c50f3d5aaf76754f9e96fc50ac7a9a52","backendConfigurationIdentity":"bc81b0064a60138c7377c4d2f4d81ce90d3c1a90ec8e776317ab0dfd6c610b85","nativeConfigurationIdentity":null,"scheduling":"automatic","toolExposure":\["mcp\_\_audex\_source\_\_list","mcp\_\_audex\_source\_\_read","mcp\_\_audex\_source\_\_search"\],"limits":{"requestRetries":0,"streamRetries":0,"automaticCompaction":false,"toolArgumentBytes":16384,"readBytes":65536,"searchQueryBytes":1024,"searchResults":100}}
- Client version binding: 2.1.314
- Client executable digest: 4487042b4c4f5d88d4568f68ae98aae0b1ffae58864cf57fb868641bc5f84594
- Model version binding: unavailable (no independent model version retained; see requested, resolved and observed identities)
- Model metadata digest: unavailable (no model metadata digest retained)
- Backend version binding: unavailable (backend version not retained)
- Subscription repair disclosure: Subscription permits the qualified client corrective requests without Audex generation admission. Interruption is best effort; initiated sends and remote charges are unconfirmed.
- Subscription repair occurrences: unavailable (no retained evidence at selection)
- Ollama model digest: not applicable
- Ollama context token limit: not applicable
- Ollama output token limit: not applicable
- Ollama truncation detection: not applicable
- Ollama truncation observed: not applicable
- Local resource tag: not applicable
- Local resource ownership: not applicable
- Local resource release: not applicable
- Native aggregate and last usage counters: not applicable
- Native usage scope: retained runtime counters, which can include estimates; not per-generation attribution or billing evidence.
- Per-generation usage evidence: \[{"request\_id":"f694af92-202e-4f7a-a351-b7262ea18664","evidence\_source":"claude\_client\_terminal\_aggregate\_not\_generation","input\_tokens":{"status":"known","value":6},"cached\_input\_tokens":{"status":"known","value":5503},"output\_tokens":{"status":"known","value":823},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"known","value":{"amount":"0.00110513","currency":"USD","measured":false}}},{"request\_id":"msg\_011Cgk5nfh9GJrVds14yCCGx","evidence\_source":"claude\_client\_message","input\_tokens":{"status":"known","value":2},"cached\_input\_tokens":{"status":"known","value":0},"output\_tokens":{"status":"unavailable","reason":"client per-message output\_tokens is a placeholder"},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"unavailable","reason":"no cost observation retained"}},{"request\_id":"msg\_011Cgk5noiLz7rMvTVdYMBFk","evidence\_source":"claude\_client\_message","input\_tokens":{"status":"known","value":2},"cached\_input\_tokens":{"status":"known","value":2610},"output\_tokens":{"status":"unavailable","reason":"client per-message output\_tokens is a placeholder"},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"unavailable","reason":"no cost observation retained"}},{"request\_id":"msg\_011Cgk5nxmpMdAZxxC56nsom","evidence\_source":"claude\_client\_message","input\_tokens":{"status":"known","value":2},"cached\_input\_tokens":{"status":"known","value":2893},"output\_tokens":{"status":"unavailable","reason":"client per-message output\_tokens is a placeholder"},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"unavailable","reason":"no cost observation retained"}}\]
- Client terminal evidence: {"operation\_id":"audex-operation-01a1ac80-a8e1-7700-90fa-cc13931ef921","execution\_id":"01a1ac80-c274-7aa0-bb13-94b9ccaf9218","terminal\_id":"f694af92-202e-4f7a-a351-b7262ea18664","successful":true,"verified":true,"result\_digest":"16103c72c51b320fcbd09afaf0fd596976dd3f870c692a870b9f5d8082422711"}
- Capture finished at selection: true
- Model identity evidence: {"requestId":"msg\_011Cgk5nfh9GJrVds14yCCGx","transport":"claude\_client\_message","expectedModel":"claude-haiku-5-5","observedModel":"claude-haiku-5-5","evidenceScope":"message","state":"verified"}
- Model identity evidence: {"requestId":"msg\_011Cgk5noiLz7rMvTVdYMBFk","transport":"claude\_client\_message","expectedModel":"claude-haiku-5-5","observedModel":"claude-haiku-5-5","evidenceScope":"message","state":"verified"}
- Model identity evidence: {"requestId":"msg\_011Cgk5nxmpMdAZxxC56nsom","transport":"claude\_client\_message","expectedModel":"claude-haiku-5-5","observedModel":"claude-haiku-5-5","evidenceScope":"message","state":"verified"}
- Generation receipts: none retained
- Receipt limits: admission is not proof of send or provider receipt; send-start is only a possible wire attempt. Null receipt fields mean no evidence retained at selection. Usage and billing are not inferred from admission or receipt.
- Facts are frozen at report selection. Later operational receipts remain separate and cannot change this contribution or outcome.
- Extraction: deterministically_normalised
- Raw output SHA-256: `16103c72c51b320fcbd09afaf0fd596976dd3f870c692a870b9f5d8082422711`
- Parse input SHA-256: `979b0a6bdfa6adf42c26e29b9cf610b2a7f98b7139fea23db2692d415af9b798`
- Deterministic transformations: ["markdown_json_fence"]
- Completion and validated structure do not establish opinion correctness; assessment belongs to the operator.
- Completeness: completed opinion; validated structured extraction
- Raw result digest: `16103c72c51b320fcbd09afaf0fd596976dd3f870c692a870b9f5d8082422711`

### Summary

    Not assessed. The single assigned read of first_or_zero.rs through the source read tool returned 'source access limit exceeded'. A second attempt returned the same error, and the brief limits the review to one read, so I stopped. No source content was inspected, so the implementation was not checked against its contract, including the empty-slice case. No findings are reported because none could be supported by evidence. This is not evidence that the file is free of defects. A new assignment with source access restored would be needed to complete the review.

### Findings

No findings were reported.

