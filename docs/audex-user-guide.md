# Audex User Guide

Audex adds **durable, isolated source auditing** to the Codex command-line application. An audit captures an immutable copy of one explicitly selected source file or snippet, runs a restricted reviewer against that snapshot, and retains a report that can be retrieved after the terminal disconnects or the application restarts.

Audex currently provides two operator interfaces:

- **Interactive TUI:** start and monitor audits within a normal Codex conversation, then discuss the report with the operator model.
- **CLI:** submit audits from the shell, retrieve Markdown reports, and integrate results into scripts.

Both interfaces use the **same local managed app-server and audit store**. The executable in the present source build is still named `codex`. The shell helper named `audex` below gives it a distinct command without replacing an existing Codex installation.

## 1. Quick start

### 1.1 Set up a shell command

These examples assume a Linux or WSL installation with the packaged Audex executable at `~/.chatgpt/Audex/package/bin/codex` and a dedicated runtime directory at `~/.chatgpt/Audex/home`.

In Bash:

```bash
export AUDEX_HOME="$HOME/.chatgpt/Audex"

audex() {
    CODEX_HOME="$AUDEX_HOME/home" "$AUDEX_HOME/package/bin/codex" "$@"
}
```

Add the function to your preferred shell startup configuration if you want it available in future terminals. It sets `CODEX_HOME` for each Audex invocation only: ordinary `codex` sessions continue to use their own installation and state.

Create the runtime directory if necessary:

```bash
mkdir -p "$AUDEX_HOME/home"
```

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
| `/audit read RUN` | Reopen a saved run, keep observing through pending states and display its report. |
| `/audit cancel RUN` | Request cancellation of an existing run. |
| `/audit release-slot RUN` | Explicitly confirm releasing an unresolved run's capacity reservation; this does not establish its outcome. |

For source submissions, paths are resolved relative to the **conversation's working directory**, not necessarily the directory of a later shell invocation. An absolute path is an option when there is any doubt.

### Model and provider selection

By default, TUI audits use the **current operator conversation's model and provider**. Change the operator model using the normal TUI controls before proposing an audit, or override it explicitly with `--model` and `--provider`. Always inspect the values on the confirmation screen before approving, especially when different models have different costs.

The reviewer's reasoning effort is taken from the conversation's current selection.

### Natural-language requests

You can also ask the operator model to initiate an audit, for example:

> Audit `src/example.rs` for error handling defects. Use the normal isolated audit workflow, and do not modify the source.

The operator can propose the audit via the existing tool bridge. The same explicit confirmation and managed controller apply. Unlike a slash command, **interpreting a natural-language request uses an operator model turn**.

### Discussing a completed report

A completed report appears immediately in the TUI. **Displaying it does not initiate another model inference request.**

When you next submit an ordinary operator message, Audex attaches the report as **untrusted additional context** to that deliberate turn. For example:

> Check the reported finding against the downstream validation. Identify which claims are supported by the code and which are overstated. Do not modify anything.

The context is then retained with that conversation turn. If you leave the TUI before asking a question, use `/audit read RUN` after restarting to stage the durable report again.

The operator's ordinary tools and permissions are separate from the restricted reviewer. Discussing a report does not change the immutable source snapshot.

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

Context documents are **not discovered automatically**; they are supplied only when requested. They are disclosed to the reviewer as untrusted information, not inherited operator instructions. Source capture is limited to **32 MiB**; each optional context document to **64 KiB**.

By default, a submitted CLI audit waits for progress and the terminal result. Use `--detach` for non-blocking operation.

### Retrieve and cancel

```bash
audex audit list
audex audit list --limit 50
audex audit list --after CURSOR

audex audit read RUN
audex audit read RUN --follow
audex audit cancel RUN
```

- `list` lists durable run summaries, newest first (default page size: 20; maximum: 100). The returned cursor pages towards older audits, including when creation timestamps tie. New submissions do not shift an existing page cursor.
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

Retry creates a fresh attempt using the retained source and original brief, with fresh execution history. If the earlier request may still be active, the command warns about overlapping charges and also requires `--acknowledge-possible-overlap`. The superseded attempt's output remains available in history; it is not supplied to the replacement.

Control commands print their action key before sending. If an acknowledgement is lost, repeat the same command with that key and identical arguments. The server returns the original decision without commissioning another attempt. A zero exit status acknowledges the control decision, not a successful review.

`attempt read` returns committed activity as JSON, with the next sequence cursor and any output cutoff, capture gap, truncation or failure. Assistant fragments remain unvalidated material, including incomplete or invalid JSON. Source-tool readbacks and status entries are labelled separately. Only the canonical validated terminal result establishes a completed review. Capture runs without an open reader and retained entries survive daemon restart.

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

Compatibility is checked using `audit/capabilities`, independently of historical runs and artefact integrity. Both daemon and client must support this endpoint and audit protocol version **2**.

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

A report can identify findings with severity and confidence, but those values are reviewer assessments. Treat references to source lines and byte offsets as evidence **to verify**, not automatic authorisation to modify code.

New result commits derive authoritative line numbers from captured source bytes and strictly validated byte ranges. Incorrect, partial or absent numeric reviewer line claims do not discard valid findings. The exact raw reviewer response is preserved, including its original claims; finding identities remain bound to that response. Lines are 1-based and LF-delimited; CRLF is one delimiter. Byte ranges remain half-open, and the end line contains the last included byte. Report format 2 distinguishes verified line ranges from migrated, unverified reviewer claims. Existing format-1 reports remain immutable; their line numbers were not checked against source and must be treated as reviewer claims.

Reviewer threads are private to the controller. Ordinary thread APIs and agent coordination tools cannot read, resume, fork, send input to or interrupt those roots. Use the audit protocol to observe or cancel their owning runs.

Audit submission may call a paid or usage-metered provider. TUI confirmation provides an opportunity to inspect the chosen model/provider; CLI submission begins when the command executes. Reading and listing existing runs do not resample the reviewer.

The controller records distinct stages such as `created`, `reviewing`, `reporting` and `completed`. An `unknown` or `observation_pending` state is **not** permission to retry inference: inspect or recover the original operation instead.

## 7. Build and update a local Linux/WSL package

This section is for running **your own source build** rather than a packaged release. These commands match the repository's current Python package builder and a GNU/Linux Rust toolchain.

### Build

From the Audex checkout:

```bash
cd ~/projects/Audex/codex-rs
cargo build -p codex-cli --bin codex

cd ~/projects/Audex
CODEX_REPO_ROOT="$PWD" python3 scripts/build_codex_package.py \
    --target x86_64-unknown-linux-gnu \
    --package-dir "$HOME/.chatgpt/Audex/package" \
    --entrypoint-bin codex-rs/target/debug/codex \
    --strip none
```

The builder also compiles or collects required auxiliary executables. On Ubuntu, building the bundled Bubblewrap executable requires the `libcap-dev` package (for `libcap.pc`). The builder's default Linux target is musl; use the explicit GNU target above unless the musl toolchain and dependencies have been installed deliberately.

The builder needs `CODEX_REPO_ROOT` when invoked directly; the repository's `just assemble-codex-package` recipe normally sets this variable.

### Replace a previously built local package

After rebuilding the CLI:

```bash
cd ~/projects/Audex
CODEX_REPO_ROOT="$PWD" python3 scripts/build_codex_package.py \
    --target x86_64-unknown-linux-gnu \
    --package-dir "$HOME/.chatgpt/Audex/package" \
    --entrypoint-bin codex-rs/target/debug/codex \
    --strip none --force
```

The daemon has its **own installed copy** of the package. Replacing the source package alone does not update that running backend. Install and pin the rebuilt package explicitly:

```bash
audex app-server daemon update --from-cli
```

This operation may restart the daemon; do it when no audit or operator work needs to stay active. Restart your TUI against the same socket afterwards.

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
| `CODEX_HOME` does not exist | Create `"$AUDEX_HOME/home"` with `mkdir -p` before the first daemon start. |
| `/audit` says it requires the managed daemon | Check `audex app-server daemon version`, exit the TUI and launch using `audex --remote unix://` rather than implicit discovery or `--no-daemon`. |
| Running version is a stock Codex release | Verify your `audex` function and `CODEX_HOME`. Both the client and the managed daemon must use the Audex installation. |
| Audit protocol is incompatible | Use the matching packaged client and daemon. Update the daemon explicitly when the package changes. |
| CLI cannot find a daemon socket | Start it with `audex app-server daemon start`. The CLI does not start or update the daemon itself. |
| Packaging says `CODEX_REPO_ROOT` is missing | Invoke the script with `CODEX_REPO_ROOT="$PWD"` from the repository root. |
| Rust cannot find `core`/`std` for musl | Pass `--target x86_64-unknown-linux-gnu` when building for the GNU toolchain. |
| Bubblewrap cannot find `libcap.pc` | On Ubuntu, install `libcap-dev` and rerun the build. |
| A reviewer result has not appeared after disconnect | Use `audex audit list` and `audex audit read RUN --follow`; do not resubmit automatically. |
| An audit report appeared but was not discussed | Ask an ordinary TUI question, or after reopening the TUI run `/audit read RUN` first. |

## 9. Current boundaries

The implemented auditing workflow accepts **one source file or snippet and one reviewer per run**. It produces durable, evidence-oriented results and integrates those results with the normal Codex operator conversation.

Automatic whole-repository source selection, simultaneous multi-model review, finding deduplication, cross-reviewer reconciliation, and automated build/test verification are **not yet user-facing audit features**. Use separate runs and operator-led verification for those tasks.

The native Codex conversation remains available for other work. An Audex reviewer is a distinct, restricted execution context, not an unrestricted second operator.
