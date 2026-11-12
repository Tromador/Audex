# Audex User Guide

Audex adds **durable, isolated source auditing** to the Codex command-line application. An audit captures an immutable copy of one explicitly selected source file or snippet, runs a restricted reviewer against that snapshot, and retains a report that can be retrieved after the terminal disconnects or the application restarts.

Audex currently provides two operator interfaces:

- **Interactive TUI:** start and monitor audits within a normal Codex conversation, then discuss the report with the operator model.
- **CLI:** submit audits from the shell, retrieve Markdown reports, and integrate results into scripts.

Both interfaces use the **same local managed app-server and audit store**. The application builds as a genuine `audex` executable; the compatibility `codex` target remains available. See [Build, install and update Audex](audex-build-install.md) for complete-package installation and local updates.

## 1. Quick start

### 1.1 Set up a shell command

Install the complete local package and add its `bin` directory to your normal PATH using the [installation procedure](audex-build-install.md). Verify command resolution before starting:

```bash
type -a audex
audex --version
```

The Audex build defaults to `~/.chatgpt/Audex/codex-home`. An explicit `CODEX_HOME` overrides this default. Use the same home for the CLI, TUI and managed daemon. Existing audit data stays in that runtime home.

### 1.2 Start and inspect the managed daemon

```bash
audex app-server daemon start
audex app-server daemon version
```

The version response identifies the running backend, package path, socket and versions. The daemon stays running when the CLI or TUI exits.

**Important:** use the Audex binary and its dedicated `CODEX_HOME` for both daemon and client operations. An upstream Codex daemon does not implement Audex's audit protocol.

### 1.3 Open the interactive TUI

From the directory containing the code you want to inspect:

```bash
cd ~/projects/Audex
audex --remote unix://
```

`--remote unix://` explicitly connects to the managed socket inside Audex's `CODEX_HOME`. This is the recommended launch mode for local Audex use; it prevents an implicit connection failure from silently switching to an embedded app-server without audit support.

Sign in when prompted if this runtime directory has not been authenticated.

Inside the TUI, enter `/audit` to open the audit picker. Choose **Start a source audit**, select the source, review the confirmation screen and explicitly approve submission. The default confirmation choice is **No**.

### 1.4 Run an audit directly from the shell

From the repository root, for example:

```bash
audex audit --file codex-rs/tui/src/audit.rs \
    --model gpt-5.6-luna --provider openai
```

The CLI prints progress to **stderr** and the completed Markdown report to **stdout**. The example model/provider pair is one tested configuration, not a requirement; use a model and provider supported by your installation.

## 2. How an audit works

1. **Select a source.** Supply one regular file or named snippet, plus an optional audit brief and explicit context documents.
2. **Confirm disclosure (TUI).** The operator sees the chosen source, provider and model before any reviewer request is submitted. The CLI submission is itself the explicit action.
3. **Capture a snapshot.** The server retains the source and manifest with content hashes. Subsequent edits to the working file do not change that audit's evidence.
4. **Run the reviewer.** An independent Codex reviewer receives the selected snapshot and only the restricted snapshot inspection tools. It does not inherit the operator conversation, tools, instructions or history.
5. **Persist progress and results.** The managed controller records state changes, reviewer results and a Markdown report. Closing a frontend does not cancel a submitted audit.
6. **Retrieve and inspect.** Read the report in the CLI or TUI. Reviewer findings are **unverified model claims** until independently examined.

An audit records a **run ID** (such as `audex-run-...`), along with snapshot and assignment identifiers. Preserve the run ID when you want to return to that audit.

A currently completed audit is not a whole-repository review: it examines the selected source and any context explicitly supplied. Multiple files require separate submissions.

## 3. Interactive TUI

Launch the TUI using `audex --remote unix://`. Slash commands act directly through the audit client; they do not require a conversational model turn just to open a picker or read a report.

| Command | Purpose |
| --- | --- |
| `/audit` | Open the durable audit picker, including existing runs and the option to start one. |
| `/audit --file ./src/example.rs` | Audit the indicated file. |
| `/audit --snippet 'fn main() {}' --name example.rs` | Audit a named source snippet. |
| `/audit --file ./src/example.rs --model MODEL --provider PROVIDER` | Override the reviewer selection. |
| `/audit --file ./src/example.rs --brief 'Check error handling and state transitions'` | Supply specific review priorities. |
| `/audit list` | List existing durable audits. |
| `/audit read RUN` | Reopen a saved run, keep observing through pending states and display its findings. |
| `/audit diagnostics RUN` | Display the complete retained report, including execution diagnostics. |
| `/audit cancel RUN` | Request cancellation of an existing run. |
| `/audit release-slot RUN` | Explicitly confirm releasing an unresolved run's capacity reservation; this does not establish its outcome. |

For source submissions, paths are resolved relative to the **conversation's working directory**, not necessarily the directory of a later shell invocation. An absolute path is an option when there is any doubt.

### Model and provider selection

By default, TUI audits use the **current operator conversation's model and provider**. Change the operator model using the normal TUI controls before proposing an audit, or override it explicitly with `--model` and `--provider`. Always inspect the values on the confirmation screen before approving, especially when different models have different costs.

For singular native selections, the reviewer's reasoning effort is taken from the conversation's current selection. Repeated `--reviewer` selections preserve their explicit order and keep duplicate reviewers independent. Native shorthand remains `PROVIDER:MODEL`; JSON provides the typed route and optional served-model binding.

### Conversation with Claude

Use `/claude <message>` in the ordinary conversation to address Claude explicitly. Plain messages continue to the Codex model selected in the TUI.

```text
/claude Challenge the assumptions in Codex's answer.
/claude --model claude-sonnet-5-5 Respond to Codex's latest argument.
```

The default is `claude-haiku-5-5`. An optional top-level `claudeConversationModel` in `$CODEX_HOME/audex/reviewer-backends.json` changes that default. A per-turn `--model MODEL` takes precedence. Use the client's full model identifier: Audex checks the requested identity against the applied client configuration and served response, and fails visibly if they disagree. It does not substitute a fallback model.

This uses the existing `claudeSubscription` executable, version, profile and runtime configuration described below. Replies are labelled **Claude (MODEL)** in the same conversation. Subsequent Codex turns can read them; further Claude turns receive the preceding visible operator and assistant discussion from the same persisted thread, including after resume. Hidden instructions, reasoning, tool records and workspace files are excluded. Text pasted into the conversation is included. Image attachments are rejected without consuming the draft.

Without an audit selection, the conversational profile has no tools or MCP servers. Select retained evidence explicitly:

```text
/claude --audit RUN_ID Examine Luna's second finding against the captured source.
Codex, challenge Claude's explanation against the same retained audit.
/claude Reconsider Codex's challenge.
/claude --audit OTHER_RUN_ID Now examine this audit instead.
```

`--audit RUN_ID` binds the turn to an existing terminal audit. Claude can retrieve its independently attributed structured findings and verified sealed source through the existing read-only source bridge. The selected run, snapshot and manifest identity are retained in the operator turn. Later `/claude` turns in that thread inherit the selection, including after resume. An explicit different run replaces the evidence permission; previous discussion remains visible, but the previous audit's tools and source permissions do not carry over. A separate thread starts without a selection. `--model` and `--audit` can appear in either order before the message.

Claude's evidence tool also accepts `view=diagnostics` to retrieve the same immutable report as Codex's `audit_read(view=diagnostics)`. This includes complete reviewer summaries, unstructured opinions and execution diagnostics, with their original attribution. `opinionAvailable` in the reviewer roster indicates retained reviewer output; it does not mean the prose appears in the structured findings view. Reports can be paged losslessly using UTF-8 byte `offset`, `maxBytes` and returned `nextOffset`. A reviewer's reference to captured guidance remains a reviewer claim; retrieving that claim does not retrieve or verify the guidance document itself.

To inspect the original guidance, both tools accept `view=guidance` with the audit's `snapshot` identity. Omit `path` to list the captured documents, then supply a listed path such as `AGENTS.md` or `AUDEX.md`. The response retains the document's original role, logical path, byte size and SHA-256, alongside the audit and snapshot identity. Reads verify the captured object and support lossless UTF-8 `offset`/`maxBytes` pagination with `nextOffset`. Missing documents are reported as unavailable; the current workspace's files are never substituted. These documents remain untrusted audit evidence, not instructions to the conversational model.

This gives Claude no live workspace access, write tools or audit execution authority. Missing, unfinished or mismatched audit identities are rejected. Changed live files do not replace the sealed source. Finding and source pages retain their attribution and identity; oversized bridge responses ask for a smaller page rather than silently truncating evidence.

Conversation does not create an audit, reserve a reviewer or start an automatic exchange between models. Finish or cancel an active turn before submitting `/claude`. Cancellation uses the normal turn interrupt and stops the owned client and bridge. A client reconnect observes the daemon-owned turn; failures and daemon restarts do not automatically replay Claude requests. As with the existing subscription route, stopping a local process does not establish whether remote quota has already been consumed.

Audex adds no message-count or token ceiling to this dialogue. The official client's context management remains enabled. The existing 2 MiB JSON-record capture safeguard still applies. The response is displayed when a complete, matching terminal result has been received. An incomplete or failed response is reported as an error.

Effort is left at the official client's default. The inspected 2.1.319 configured client and 2.1.321 PATH client advertise `--effort low|medium|high|xhigh|max`; M3 does not expose an effort override. See Anthropic's [CLI reference](https://code.claude.com/docs/en/cli-reference) and [model configuration](https://code.claude.com/docs/en/model-config). Client-help inspection establishes the available option syntax, not live model availability or an accepted M3 demonstration.

### Claude subscription reviewers

Audex supports `claude_subscription` through the official Claude client. `claude_api` and `ollama_local` remain unavailable.

Select the subscription route explicitly. The same selection works in `/audit` and the CLI:

```bash
audex audit --file ./example.rs \
    --reviewer 'openai:YOUR_NATIVE_MODEL' \
    --reviewer '{"provider":"anthropic","model":"claude-haiku-5-5","executionAdapter":"claude_client","authenticationRoute":"claude_subscription"}'
```

Omit the native entry for a subscription-only run. `--provider anthropic` alone does not select a subscription. A qualified served-model alias can be supplied as `expectedServedModel`; the client is still invoked with the requested model. No route, model or executable substitution is permitted.

The daemon reads `$CODEX_HOME/audex/reviewer-backends.json`. Provisioning this configuration, its dedicated profile and its runtime directory is an explicit operator action. The subscription section is:

```json
{
  "version": 1,
  "claudeSubscription": {
    "executable": "/absolute/resolved/path/to/claude",
    "clientVersion": "2.1.319",
    "profile": "/absolute/resolved/path/to/dedicated-profile",
    "runtimeRoot": "/var/tmp/audex"
  }
}
```

A dedicated subscription profile and the official Claude Code executable are required. Set `clientVersion` to the version that executable reports; Audex refuses a client that reports anything else. To change version, update `executable` and `clientVersion` together. Do not put credentials in this file. The profile must pass the existing isolation policy, including disabled hooks, connectors, synchronised plugins/skills, memory and cross-session messaging. The runtime must be an owned private directory outside the daemon's `HOME`, separate from the profile and short enough for the client's 103-byte Unix-socket path limit. Preflight refuses missing or incompatible setup; it does not create directories, modify a profile, refresh credentials or launch a client.

Before TUI confirmation, the daemon checks the entire ordered roster, including queued reviewers. Disclosure identifies source, brief and context hashes, execution/authentication routes, requested/resolved/expected served models, executable/version/hash, profile, runtime and backend/policy bindings. Start checks them again. Drift refuses commissioning without creating runnable attempts. Account eligibility and effective billing are checked by the owned client before Audex writes the prompt. The CLI prints the disclosure during submission and does not pause for confirmation.

An identical request key retrieves the existing audit even after configuration becomes unavailable; it starts no new work. Older audits without configuration bindings remain retrievable when their retained request identity matches. Changed request content under that key is refused.

This route consumes subscription quota. The official client can initiate corrective requests: up to three continuations after truncated output, two retries for a missing tool call and one empty-output nudge **per occurrence**. These are not a total request ceiling. Observed corrections and anomalies are retained in activity and reports, including interrupted attempts.

Cancel, Ignore, Retry and whole-audit Abort retain their existing exact-target behaviour. Stopping the owned client is best effort; already initiated requests or remote quota charges may continue. Retry creates fresh execution from the retained source and policy, and refuses configuration drift. Reconnect, viewing and history navigation do not replay or resume a subscription request.

### Natural-language requests

You can also ask the operator model to initiate an audit, for example:

> Audit `src/example.rs` for error handling defects. Use the normal isolated audit workflow, and do not modify the source.

The operator can propose the audit via the existing tool bridge. The same explicit confirmation and managed controller apply. Unlike a slash command, **interpreting a natural-language request uses an operator model turn**.

### Discussing a completed report

When an audit being observed in the TUI reaches a terminal state, Audex displays a concise result based on completed reviewer opinions:

- **Zero completed:** report that no reviewer completed and identify the failed or aborted jobs.
- **One completed:** present that reviewer's findings, without a comparison.
- **Two or more completed:** automatically queue one Codex comparison of those completed opinions.

This also applies when the overall audit was aborted: useful completed opinions remain available. Every result identifies failed or aborted jobs separately; they do not count as agreement, disagreement or clean reviews. A completed opinion with no findings still counts as completed. An unstructured completed opinion remains available in diagnostics.

The comparison waits behind any active turn and earlier queued input, preserving pending asynchronous questions, typed answers and the ordinary composer. Normal question-draft recovery still applies when the active turn ends. Codex retrieves the original completed opinions and sealed source, then assesses their relationships. It uses the conversation's current model and ordinary inference route, and never starts replacement reviews for failed or aborted jobs.

Every original finding remains independently attributed. Severity and confidence are reviewer assessments, and verified source locations do not establish that a defect is proven. Longer prose in the immediate findings view is explicitly marked as an excerpt; the full findings and evidence remain retrievable.

Ask follow-up questions normally, for example:

> Check the reported finding against the downstream validation. Identify which claims are supported by the captured code and which are overstated. Do not modify anything.

Use `/audit diagnostics RUN` for the complete immutable report, or ask Codex for diagnostics through `audit_read` with `view=diagnostics`. Individual reviewer opinions and source-tool histories are also available in the existing reviewer activity viewer. Diagnostic retrieval does not rerun reviewers.

Reopening an already completed audit with `/audit read RUN` displays its findings and stages a compact reference for your next question; it does not automatically repeat the comparison. If the TUI was closed before completion, reopen the run and ask Codex to compare it. A comparison queued while its conversation is in the background waits until that conversation is displayed again. Runs with fewer than two completed reviewer opinions do not start an automatic comparison.

The operator's ordinary tools and permissions remain separate from the restricted reviewers. Comparison instructions require read-only use of retained evidence and the original sealed source; neither retrieval nor discussion changes the immutable snapshot.

## 4. Command-line reference

Run `audex audit --help` for the local executable's current options.

### Submit

```bash
audex audit --file PATH [--model MODEL] [--provider PROVIDER]
audex audit --stdin --name LOGICAL_FILENAME [--model MODEL] [--provider PROVIDER]
```

Submission options:

| Option | Meaning |
| --- | --- |
| `--file PATH` | Capture one local regular file; relative paths use the CLI process's current directory. |
| `--stdin --name NAME` | Read a UTF-8 snippet from standard input and give it a logical filename. |
| `--model MODEL` | Set reviewer model. |
| `--provider PROVIDER` | Set reviewer provider. |
| `--brief TEXT` | Define review priorities. |
| `--audex-md PATH` | Supply an explicit project audit guidance document. |
| `--agents-md PATH` | Supply explicit evidence about developer conventions. |
| `--detach` | Return the durable run ID without waiting for completion. |
| `--request-key KEY` | Supply a stable idempotency key for an uncertain submission. |

When `--model` or `--provider` is omitted, the CLI consults the **daemon's configuration**, not an open TUI conversation. Specify them when reproducibility matters. If the daemon has no configured model, provide `--model`.

Context documents are **not discovered automatically**; they are supplied only when requested. They are disclosed to the reviewer as untrusted information, not inherited operator instructions. Source capture is limited to **32 MiB**; each optional context document to **64 KiB**. Native and Claude subscription reviewers receive the complete captured contents within that document limit. `AUDEX.md` may guide priorities within Audex’s fundamental rules; `AGENTS.md` remains evidence of conventions and cannot change scope or permissions.

By default, a submitted CLI audit waits for progress and the terminal result. Use `--detach` for non-blocking operation.

### Retrieve and cancel

```bash
audex audit list
audex audit list --limit 50
audex audit list --after CURSOR

audex audit read RUN
audex audit read RUN --follow
audex audit read RUN --diagnostics
audex audit cancel RUN
```

- `list` lists durable run summaries, newest first (default page size: 20; maximum: 100). The returned cursor pages towards older audits, including when creation timestamps tie. New submissions do not shift an existing page cursor.
- The CLI prints the concise findings view by default. `--diagnostics` prints the complete immutable Markdown report. Automatic Codex comparison takes place in the TUI conversation.
- Listing does not open results or reports. A damaged historical artefact therefore does not prevent listing other runs; retrieving the damaged artefact still fails integrity verification.
- `read RUN` returns the existing state and the Markdown report if one is ready; it never starts another review.
- `read RUN --follow` waits through recoverable observation-pending states until a terminal outcome or Ctrl-C. Attached submissions, cancellation watches and TUI observation do the same. A plain `read RUN` returns immediately; it can report exit 5 while work is pending.
- `cancel RUN` records cancellation intent and follows the original operation's outcome. Cancellation may race with completion; a completed result is retained.

### Individual reviewer attempts

Attempt commands address an exact assignment, so a delayed command cannot stop its replacement. Use `list` to obtain the current and historical assignment IDs:

```bash
audex audit attempt list RUN
audex audit attempt read RUN ASSIGNMENT --after-sequence 0 --limit 64
audex audit attempt cancel RUN ASSIGNMENT --action-key MY_STOP_KEY
audex audit attempt ignore RUN ASSIGNMENT --action-key MY_IGNORE_KEY
audex audit attempt retry RUN ASSIGNMENT --action-key MY_RETRY_KEY --authorise-retry
```

Cancel and Ignore acknowledge a durable decision: the available received prefix is frozen and the attempt's scheduling reservation is released. They return without waiting for the reviewer to terminate. Healthy siblings continue. A completed review that committed first remains completed.

Each native HTTP or WebSocket generation requires durable, one-use admission immediately before submission. Once an attempt's stop decision commits, later admissions for that attempt are refused. The same applies to superseded attempts and whole-audit stop decisions. A request admitted earlier may still be submitted or remain uncertain; a cancellation acknowledgement does not prove that provider-side inference or charging has stopped. Admission records survive restart, and uncertain requests are not automatically replayed.

Retry creates a fresh attempt using the retained source and original brief, with fresh execution history. If the earlier request may still be active, the command warns about overlapping charges and also requires `--acknowledge-possible-overlap`. The superseded attempt's output remains available in history; it is not supplied to the replacement.

Control commands print their action key before sending. If an acknowledgement is lost, repeat the same command with that key and identical arguments. The server returns the original decision without commissioning another attempt. A zero exit status acknowledges the control decision, not a successful review.

`attempt read` returns committed activity as JSON, with the next sequence cursor and any output cutoff, capture gap, truncation or failure. Assistant fragments remain unvalidated material. Source-tool readbacks and status entries are labelled separately. A completed opinion requires verified successful execution and non-empty final output; a partial message or disconnected client is insufficient. Capture runs without an open reader and retained entries survive daemon restart.

For completed opinions, the attempt includes extraction status and the exact original text in `rawResult`. The TUI history and frozen report also show the full original opinion when structured extraction fails. An unstructured opinion is completed work, so malformed JSON alone does not require Ignore or Retry.

### CLI exit codes

| Code | Meaning |
| --- | --- |
| `0` | Audit completed successfully, or `--detach` obtained acknowledgement of durable submission. |
| `1` | Local/request error (including inability to connect or read input). |
| `2` | Audit failed or completed with failures. |
| `3` | Audit cancelled. |
| `4` | Terminal unknown operation state. |
| `5` | Non-following read or slot release: work is still pending. |
| `130` | User pressed Ctrl-C; the frontend disconnected, **not** the reviewer cancelled. |

Scripts should not interpret `--detach` returning zero as a successful review verdict. A zero there means **the submission was accepted**.

## 5. Recipes

All shell recipes below use the `audex` helper defined in section 1.

### Audit a file and save its Markdown report

```bash
audex audit --file src/example.rs \
    --model gpt-5.6-luna --provider openai \
    > example-audit.md
```

Progress stays visible on stderr; only the report is redirected to the Markdown file.

### Audit a short snippet without creating a source file

```bash
cat <<'SOURCE' | audex audit --stdin --name example.rs \
    --model gpt-5.6-luna --provider openai
fn main() {
    println!("hello");
}
SOURCE
```

The source is captured under the logical filename `example.rs`. The snippet must be UTF-8.

### Launch an audit and return to it later

```bash
RUN=$(audex audit --file src/example.rs \
    --model gpt-5.6-luna --provider openai --detach)

printf 'Run: %s\n' "$RUN"
audex audit read "$RUN" --follow > example-audit.md
```

Store the printed run ID if you intend to read it from another terminal or after a restart.

### Ask a targeted question

```bash
audex audit --file src/example.rs \
    --brief 'Find concrete race conditions in cancellation and recovery. Provide evidence for every claim.' \
    --model gpt-5.6-luna --provider openai
```

A good brief describes the *kind* of issue to investigate. It does not need to restate internal workflow instructions.

### Include project-specific guidance deliberately

```bash
audex audit --file src/example.rs \
    --audex-md ./review-guidance.md \
    --agents-md ./developer-conventions.md \
    --brief 'Check compliance with documented resource and error-handling conventions'
```

Use actual files appropriate to the project. `--audex-md` supplies audit guidance; `--agents-md` supplies evidence about conventions. Neither is silently loaded from the operator environment.

### Compare two reviewers manually

```bash
audex audit --file src/example.rs --model MODEL_A --provider PROVIDER_A \
    > reviewer-a.md

audex audit --file src/example.rs --model MODEL_B --provider PROVIDER_B \
    > reviewer-b.md
```

These are **two separate single-reviewer audits**, not an automatically reconciled multi-model run. If the source might change between runs, preserve a stable copy of the file and submit that copy to both.

### Reopen a report for discussion

In the TUI:

```text
/audit read audex-run-...
```

Then send an ordinary message, such as:

> Investigate finding 1 against the current repository. Explain the actual consequence and whether existing validation prevents it. Do not edit files.

This is a deliberate operator inference request, not a rerun of the reviewer.

## 6. Recovery, safety and costs

**Closing the TUI, losing an SSH connection or pressing Ctrl-C in the CLI does not cancel submitted reviewer work.** Start a new client, run `audex audit list`, then `audex audit read RUN --follow` or `/audit read RUN`.

If a start request loses its acknowledgement, **do not automatically submit another request with a new key**. First inspect `audex audit list`. The CLI's `--request-key` lets you reuse the *same key with identical input* when an acknowledgement is genuinely uncertain. This preserves the controller's idempotency contract.

If the server retained a matching request before its response failed, the error includes its run ID and request key. Reusing a key for different input returns an explicit idempotency conflict without a recovery run ID. Recover that run with `audit read RUN --follow` or `/audit read RUN`. An identical retry compares the captured source, explicit brief and requested reviewer settings, not today's resolved model catalogue or policy. Changed source contents or explicit inputs conflict with the original key. Older runs created before stable requested-input identities were recorded retain their conservative legacy comparison; recover using a known run ID or inspect the listing; the server does not claim that a legacy request key alone proves matching input.

Compatibility is checked using `audit/capabilities`, independently of historical runs and artefact integrity. Both daemon and client must support this endpoint and audit protocol version **8**. Older daemons are refused before submission; the audit workflow does not upgrade them or fall back automatically.

Preparation leases are renewed while their owner remains active and unexpired. Cancellation, explicit slot release and lease expiry fence submission; an expired owner cannot revive its lease. Known unsubmitted failures and abandoned preparation are cleaned up without creating replacement inference. Potentially submitted work remains observation-only.

Healthy runs are normally observed once per second, with lifecycle hints waking recovery sooner. Unresolved observation failures back off to at most one attempt per minute. New cancellation intent bypasses the observation delay. Unchanged revisions do not repeatedly broadcast progress.

Attached CLI and TUI clients continue watching pending runs. Progress notifications can wake them immediately; fallback reads are once per second during normal execution and once per five seconds while observation is pending. Reading progress does not submit inference or bypass the controller's recovery backoff.

Missing or transient evidence retains the operation for observation and holds capacity until positive terminal evidence or explicit operator release. Positive contradictory identity or acceptance evidence instead produces terminal `unknown` (CLI exit 4), preserving its diagnostic and freeing normal capacity; it never permits replay of that operation. An unknown outcome does not prove that an inference stopped. To release it, first request cancellation, then acknowledge that its original reviewer may still execute and incur charges:

```bash
audex audit release-slot RUN \
    --acknowledge-concurrent-execution-and-charges \
    --reason 'Accept possible concurrent execution'
```

If `audit cancel RUN` remains attached because the outcome is unresolved, wait for the cancellation-requested status, then press Ctrl-C to disconnect before running the release command. This does not withdraw the recorded cancellation intent.

The TUI equivalent, `/audit release-slot RUN`, defaults to **No** and requires explicit confirmation. Release permanently fences resubmission of the original operation. It does not mark the audit complete or cancelled, prevent observation, or discard late valid results.

A completed review is an independent model opinion for you to assess. Completion means the authorised reviewer successfully finished and produced non-empty output. It does not establish that the opinion is correct or useful.

Structured extraction has three possible statuses:

- **Validated structured:** the original JSON passed schema, identity and source-reference checks.
- **Deterministically normalised:** those checks passed after removing a leading UTF-8 byte-order mark, a whole-response Markdown `json` fence, or both. LF and CRLF fence delimiters and outer ASCII whitespace are accepted. Each transformation and its parse-input digest are recorded.
- **Unstructured opinion:** extraction failed. The full original text is retained with the discrepancy; Audex does not invent a summary or findings. Plain English, malformed JSON and nonsensical text receive the same treatment. Incorrect claimed assignment IDs or source references cannot override Audex's verified provenance.

Raw output is immutable and bounded to 1 MiB. Empty, invalid UTF-8 or oversized output cannot establish a completed opinion. Failed, interrupted or unattributed execution remains incomplete even when some text was captured. Audex never repeats inference to repair output formatting.

A report can identify structured findings with severity and confidence, but those values are reviewer assessments. Treat references to source lines and byte offsets as evidence **to verify**, not automatic authorisation to modify code.

New result commits derive authoritative line numbers from captured source bytes and strictly validated byte ranges. Incorrect, partial or absent numeric reviewer line claims do not discard valid findings. The exact raw reviewer response is preserved, including its original claims; finding identities remain bound to that response. Lines are 1-based and LF-delimited; CRLF is one delimiter. Byte ranges remain half-open, and the end line contains the last included byte. Report format 2 distinguishes verified line ranges from migrated, unverified reviewer claims. Existing format-1 reports remain immutable; their line numbers were not checked against source and must be treated as reviewer claims.

Reviewer threads are private to the controller. Ordinary thread APIs and agent coordination tools cannot read, resume, fork, send input to or interrupt those roots. Use the audit protocol to observe or cancel their owning runs.

Audit submission may call a paid or usage-metered provider. TUI confirmation provides an opportunity to inspect the chosen model/provider; CLI submission begins when the command executes. Reading and listing existing runs do not resample the reviewer.

The controller records distinct stages such as `created`, `reviewing`, `reporting` and `completed`. An `unknown` or `observation_pending` state is **not** permission to retry inference: inspect or recover the original operation instead.

## 7. Build and update a local Linux/WSL package

Use [Build, install and update Audex](audex-build-install.md) for the authoritative source-build, complete-package installation and update procedure.

`audex app-server daemon update --from-cli` selects the package containing the executable that invokes it. Install the newly built package and invoke its `bin/audex` as documented before selecting it. Running the command through an older installed executable selects that older package again.

Package selection may restart the daemon. Do it when no audit or operator work needs to stay active, then reopen the TUI against the same runtime home and socket.

### Daemon lifecycle

```bash
audex app-server daemon version
audex app-server daemon start
audex app-server daemon stop
```

Stopping the daemon is optional after using the TUI or CLI. Leave it running when you want outstanding audits to complete.

## 8. Troubleshooting

| Symptom | What to check |
| --- | --- |
| `CODEX_HOME` does not exist | Without an override, Audex creates `$HOME/.chatgpt/Audex/codex-home`. If you explicitly set `CODEX_HOME`, use a non-empty directory path and create it with `mkdir -p "$CODEX_HOME"` before starting. |
| `/audit` says it requires the managed daemon | Check `audex app-server daemon version`, exit the TUI and launch using `audex --remote unix://` rather than implicit discovery or `--no-daemon`. |
| Running version is a stock Codex release | Run `type -a audex` and check for a shadowing alias, function or older executable. Verify the installed and selected package manifests using the [installation procedure](audex-build-install.md), and use the same effective `CODEX_HOME` for client and daemon. |
| Audit protocol is incompatible | Use the matching packaged client and daemon. Update the daemon explicitly when the package changes. |
| CLI cannot find a daemon socket | Start it with `audex app-server daemon start`. The CLI does not start or update the daemon itself. |
| Packaging says `CODEX_REPO_ROOT` is missing | Invoke the script with `CODEX_REPO_ROOT="$PWD"` from the repository root. |
| Rust cannot find `core`/`std` for musl | Pass `--target x86_64-unknown-linux-gnu` when building for the GNU toolchain. |
| Bubblewrap cannot find `libcap.pc` | On Ubuntu, install `libcap-dev` and rerun the build. |
| A reviewer result has not appeared after disconnect | Use `audex audit list` and `audex audit read RUN --follow`; do not resubmit automatically. |
| An audit completed without a comparison | Fewer than two completed opinions do not trigger comparison. If two or more completed while the TUI was closed, reopen it with `/audit read RUN` and ask Codex to compare them. Check any displayed comparison submission error. |

## 9. Current boundaries

The implemented auditing workflow accepts **one source file or snippet and an ordered roster of independent reviewers per run**. Native reviewers and explicitly configured Claude subscription reviewers share the sealed snapshot and common brief. Report format 5 preserves each reviewer's attribution and extraction status, including full unstructured opinions and completed peers alongside failed or interrupted attempts. Historical reports, including formats 2, 3 and 4, retain their original identities and bytes. Clients use audit protocol 8; incompatible daemons must be updated explicitly before submitting new work.

Automatic whole-repository source selection, deterministic finding deduplication and automated build/test verification remain outside the current public workflow. The TUI completion comparison is a Codex assessment of retained independent claims; it does not replace them or establish verification.

The native Codex conversation remains available for other work. An Audex reviewer is a distinct, restricted execution context, not an unrestricted second operator.
