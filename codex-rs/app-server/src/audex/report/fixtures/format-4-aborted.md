# Audex Audit Report

- Report format: `4`
- Run: `audex-run-01a1a897-e3f9-7a20-812d-1d078c23b551`
- Snapshot: `audex-source-v1:sha256:e525d1acaa156833264a4d2db49ca69014eaa493eed874be0b1123dd20944fe1`
- Selection: `audex-run-01a1a897-e3f9-7a20-812d-1d078c23b551:selection:1`
- Outcome: `aborted_by_operator`

## Reviewer 1: claude-haiku-5-5

- Reviewer: `audex-reviewer-01a1a897-e3fa-7c70-b0f8-38483d2bb40f`
- Assignment: `audex-assignment-01a1a897-e3f9-7a20-812d-1d1009905a9b`
- Operation: `audex-operation-01a1a897-e3f9-7a20-812d-1d251f5abec2`
- Provider: `anthropic`
- Requested model: `claude-haiku-5-5`
- Resolved model: `claude-haiku-5-5`
- Observed served model: `claude-haiku-5-5`
- Disposition: `cancelled`
- Execution state: `failed`
- Attempt: `1`
- Executor: claude\_client
- Route: claude\_subscription
- Admission policy: Bound successful client terminal and verified per-message identity; no Audex per-generation admission. Provider-internal corrective requests may occur.
- Expected served model: claude-haiku-5-5
- Retained input version: 2
- Retained source and policy bindings: {"version":1,"snapshotId":"audex-source-v1:sha256:e525d1acaa156833264a4d2db49ca69014eaa493eed874be0b1123dd20944fe1","manifestDigest":"9e9fbac707ea5022e366925387b9664a59c5ddf4ce1ffcdd1c914bcf3d8163bb","briefDigest":"07195070cd6dcf21f90253c8538408041dd83bc5a5bb37338b748e5bfdfcfd83","contextDigest":"4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945","policyDigest":"3bbfa262a58760039a339ee0f172de06b750358e94d4e495e616e1e36a9e7dc7","backendConfigurationIdentity":"ba7cdd5c737083426672adad5d883b0d6cfabe1e74e92190ac4f36e8a9ad1033","nativeConfigurationIdentity":null,"scheduling":"automatic","toolExposure":\["mcp\_\_audex\_source\_\_list","mcp\_\_audex\_source\_\_read","mcp\_\_audex\_source\_\_search"\],"limits":{"requestRetries":0,"streamRetries":0,"automaticCompaction":false,"toolArgumentBytes":16384,"readBytes":65536,"searchQueryBytes":1024,"searchResults":100}}
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
- Per-generation usage evidence: \[{"request\_id":"0f22db57-f250-45c3-bddf-c7035fe36ff4","evidence\_source":"claude\_client\_terminal\_aggregate\_not\_generation","input\_tokens":{"status":"known","value":6},"cached\_input\_tokens":{"status":"known","value":5487},"output\_tokens":{"status":"known","value":643},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"known","value":{"amount":"0.00101037","currency":"USD","measured":false}}},{"request\_id":"msg\_011CgieRmVQUnWUbM6ZaNp1T","evidence\_source":"claude\_client\_message","input\_tokens":{"status":"known","value":2},"cached\_input\_tokens":{"status":"known","value":0},"output\_tokens":{"status":"unavailable","reason":"client per-message output\_tokens is a placeholder"},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"unavailable","reason":"no cost observation retained"}},{"request\_id":"msg\_011CgieRuFEW24aRNuF8uWbi","evidence\_source":"claude\_client\_message","input\_tokens":{"status":"known","value":2},"cached\_input\_tokens":{"status":"known","value":2577},"output\_tokens":{"status":"unavailable","reason":"client per-message output\_tokens is a placeholder"},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"unavailable","reason":"no cost observation retained"}},{"request\_id":"msg\_011CgieS1zHZcJxdJ7kzj4Fm","evidence\_source":"claude\_client\_message","input\_tokens":{"status":"known","value":2},"cached\_input\_tokens":{"status":"known","value":2910},"output\_tokens":{"status":"unavailable","reason":"client per-message output\_tokens is a placeholder"},"reasoning\_tokens":{"status":"unavailable","reason":"not observed"},"cost":{"status":"unavailable","reason":"no cost observation retained"}}\]
- Client terminal evidence: {"operation\_id":"audex-operation-01a1a897-e3f9-7a20-812d-1d251f5abec2","execution\_id":"01a1a897-ef80-7af0-ae5d-018faff8889b","terminal\_id":"0f22db57-f250-45c3-bddf-c7035fe36ff4","successful":true,"verified":true,"result\_digest":"acdf90e9fe6ba06f06ab7fb6efbd75b948313b09940e5287ed725e20d62a3605"}
- Capture finished at selection: true
- Model identity evidence: {"requestId":"msg\_011CgieRmVQUnWUbM6ZaNp1T","transport":"claude\_client\_message","expectedModel":"claude-haiku-5-5","observedModel":"claude-haiku-5-5","evidenceScope":"message","state":"verified"}
- Model identity evidence: {"requestId":"msg\_011CgieRuFEW24aRNuF8uWbi","transport":"claude\_client\_message","expectedModel":"claude-haiku-5-5","observedModel":"claude-haiku-5-5","evidenceScope":"message","state":"verified"}
- Model identity evidence: {"requestId":"msg\_011CgieS1zHZcJxdJ7kzj4Fm","transport":"claude\_client\_message","expectedModel":"claude-haiku-5-5","observedModel":"claude-haiku-5-5","evidenceScope":"message","state":"verified"}
- Generation receipts: none retained
- Receipt limits: admission is not proof of send or provider receipt; send-start is only a possible wire attempt. Null receipt fields mean no evidence retained at selection. Usage and billing are not inferred from admission or receipt.
- Facts are frozen at report selection. Later operational receipts remain separate and cannot change this contribution or outcome.
- Completeness: incomplete / excluded; no accepted completed opinion

**Exclusion / stop reason**

> whole-audit Abort

**Execution failure reason**

> invalid_result
- Received prefix cutoff: `66`
- Prefix digest: `9c5df03545d6eb8f7506806a4e0043bfdbe16e594b01f687eba58395de91d7bf`

**Incomplete assistant output (unvalidated; not findings)**

> ```json
> {
>   "assignment_id": "audex-assignment-01a1a897-e3f9-7a20-812d-1d1009905a9b",
>   "findings": [],
>   "schema_version": 1,
>   "snapshot_id": "audex-source-v1:sha256:e525d1acaa156833264a4d2db49ca69014eaa493eed874be0b1123dd20944fe1",
>   "summary": "Read example.rs (bytes 0-43, sha256 eaf5e128c0b96e7012edbf4a7c51aeecf11cc26e62d592232fc7a00c0a7f7532). The file contains one function, `pub fn identity(value: u8) -> u8 { value }`, which returns its argument unchanged. No concrete defects are supported by this source, so the findings list is empty. I made one extra one-byte read of the same file while checking the first byte; it did not change the result."
> }
> ```

**Source/tool activity and status (not findings)**


**status**

> client_session_observed

**status**

> client_status_observed

**status**

> client_system_event_redacted

**status**

> client_system_event_redacted

**tool_start**

> {"tool":"mcp__audex_source__read","arguments":{"path":"example.rs","offset":0,"max_bytes":65536}}

**status**

> client_rate_limit_status_observed

**tool_result**

> {"path":"example.rs","sha256":"eaf5e128c0b96e7012edbf4a7c51aeecf11cc26e62d592232fc7a00c0a7f7532","offset":0,"end_offset":43,"eof":true,"encoding":"utf8","content":"pub fn identity(value: u8) -> u8 { value }\n"}

**status**

> client_status_observed

**tool_start**

> {"tool":"mcp__audex_source__read","arguments":{"path":"example.rs","offset":0,"max_bytes":1}}

**tool_result**

> {"path":"example.rs","sha256":"eaf5e128c0b96e7012edbf4a7c51aeecf11cc26e62d592232fc7a00c0a7f7532","offset":0,"end_offset":1,"eof":false,"encoding":"utf8","content":"p"}

**status**

> client_status_observed

**status**

> client_terminal_success_observed

