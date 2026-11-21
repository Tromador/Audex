# Audex 1.0.0: Production operations and operator handbook

**Updated:** 21 November 2026. **Host:** Zen, WSL Ubuntu.  
**Purpose:** Running, testing and maintaining the two-reviewer audit system, especially for the Linux port of UE4SS's `File` dependency.

This document describes the **current live-source, file-backed implementation**. Some older Audex documentation describes a superseded snapshot-based system. For disputed behaviour, inspect `codex-rs/tui/src/audit_service.rs`, `codex-rs/tui/src/claude.rs` and `codex-rs/app-server/src/audex/`.

## 1. First use

```bash
audex --version
audex
```

Slash commands below are entered in **Audex's TUI**, not a shell. A normal audit needs only a path:

```text
/audit /path/to/source
```

The installed default pair is:

| Role | Model | Effort |
| --- | --- | --- |
| Native | `gpt-6.2-astra` | `high` |
| Claude subscription | `claude-opus-5-5` | `high` |
| Conversational `/claude` | `claude-opus-5-5` | Claude client default |

The default audit preset and Claude conversation model are configured in:

`~/.audex/audex/reviewer-backends.json`

```json
{
  "version": 1,
  "presets": {
    "default": [
      "native:gpt-6.2-astra:high",
      "claude_subscription:claude-opus-5-5:high"
    ]
  },
  "claudeConversationModel": "claude-opus-5-5",
  "claudeSubscription": {
    "executable": "/home/stef/.local/bin/claude",
    "profile": "/home/stef/.audex/claude-profile",
    "runtimeRoot": "/home/stef/.audex/tmp"
  }
}
```

After changing reviewer presets, **exit the TUI and restart the managed daemon**:

```bash
audex app-server daemon stop
audex app-server daemon start
```

Conversational Claude chooses the configured model, but `/claude` has no effort-setting option. The default high effort above applies to the two **audit reviewers**.

## 2. Complete `/audit` slash-command reference

### Start

```text
/audit PATH [PATH ...]
       [--with PRESET]
       [--reviewer ROUTE:MODEL[:EFFORT]] ...
       [--brief "TEXT"]
       [--guidance FILE | --guidance-text "TEXT"]
```

For source supplied as text rather than a pathname:

```text
/audit --snippet "SOURCE TEXT" --name filename.ext [reviewer/guidance options]
```

| Option | Meaning |
| --- | --- |
| `PATH` | One or more existing files or directories; quote spaces. Relative paths use the current TUI thread working directory. |
| `--with PRESET` | Use a named preset from `reviewer-backends.json`. |
| `--reviewer ROUTE:MODEL[:EFFORT]` | Choose a reviewer. Repeat as needed. Routes: `native`, `claude_subscription`, `claude_api` (requires its own backend configuration). |
| `--brief "TEXT"` | Override the default brief: "Find concrete defects supported by evidence in the selected source." |
| `--guidance FILE` | Include local UTF-8 instructions from an operator-selected file; absolute paths are easiest. |
| `--guidance-text "TEXT"` | Supply inline instructions instead. Mutually exclusive with `--guidance`. |
| `--snippet` and `--name` | Audit a supplied snippet; both must be present and no filesystem paths can be supplied. |
| `--` | Stop parsing flags; remaining tokens are paths. |

**Preset nuance:** Without `--reviewer` or `--with`, Audex uses `default`. With one or more explicit `--reviewer` selections and no `--with`, only those reviewers are used. If `--with` and `--reviewer` are combined, the explicit reviewers are **added** to the preset, not substituted for it.

Examples:

```text
/audit /home/stef/projects/Audex/scripts/codex_package

/audit /path/to/foo.cpp --reviewer native:gpt-6.2-astra:high --reviewer claude_subscription:claude-opus-5-5:high

/audit /path/to/component --brief "Find ownership errors and incorrect behaviour" --guidance-text "Preserve the public API."

/audit --snippet "int f() { return 42; }" --name demo.cpp --reviewer native:gpt-6.2-astra:medium
```

**Audit semantics:** Directory paths are passed directly to reviewers, without snapshotting, listing or hashing the tree. Explicit file targets let reviewers read containing directories for context. The source is live during the review. Both reviewers are read-only and have no network access. Native uses Codex sandboxed shell inspection; Claude uses its official restricted client with `Read`, `Grep` and `Glob`. An empty finding list is acceptable.

Every supplied directory is presently an audit target. There is **no separate reference-directory option**; guidance can explain that the second path is an upstream comparison tree, but cannot structurally change target selection.

### List, reopen, cancel and retry

```text
/audit list
/audit list --offset N
/audit read RUN_ID
/audit cancel RUN_ID
/audit cancel RUN_ID REVIEWER
/audit retry RUN_ID REVIEWER
```

| Subcommand | Result |
| --- | --- |
| `list` | Show retained audits (20 per page; `--offset` selects another page). |
| `read RUN_ID` | Reopen a retained audit and its summary. |
| `cancel RUN_ID` | Cancel an audit. Partial retained evidence remains available. |
| `cancel RUN_ID REVIEWER` | Cancel one reviewer in a run. |
| `retry RUN_ID REVIEWER` | Create another attempt for one reviewer, retaining earlier attempts. |

Reviewer references are one-based positions or retained labels, e.g. `1` and `reviewer-1`, **not** `astra` or `opus` aliases.

### Inspect the result conversationally

The TUI provides model-visible audit tools for `reviewers`, `findings`, an individual `finding`, `output`, `diagnostics` and original `report`. These are **not additional slash-command subcommands**. Ask the model directly:

- "Show Claude's original report from the active audit."
- "Show reviewer 1's finding 2 with its evidence and source reference."
- "Compare the reviewers' findings, separating agreement from omission."
- "Show diagnostics for the failed attempt."

An original report is displayed from retained output directly rather than being paraphrased. Other questions are answered conversationally using retained evidence. The current run is also automatically compared after both reviewers complete.

## 3. Complete `/claude` slash-command reference

```text
/claude [--model MODEL] [--audit RUN_ID | --no-audit] MESSAGE
```

| Part | Meaning |
| --- | --- |
| `--model MODEL` | Override the configured conversation model for this turn. |
| `--audit RUN_ID` | Discuss this particular retained audit. |
| `--no-audit` | Have an ordinary Claude conversation even when an audit is active. |
| `MESSAGE` | Required conversation message; options are parsed only at the beginning. |

`--model=MODEL` and `--audit=RUN_ID` work too. Without an explicit run ID, an active audit is automatically used; without any audit, Claude is a tool-free conversation. Claude receives the visible discussion, so a short follow-up can refer to its previous offer.

Examples:

```text
/claude What do you think about Astra's findings?
/claude Yes please do.
/claude --model claude-opus-5-5 Examine the consequences of finding 3.
/claude --audit 01a1ff0d-dd03-7841-b6d7-eda0f1866935 Review the reported cache defects.
/claude --no-audit Explain how Linux file descriptors differ from Windows handles.
```

With an audit selected, Claude can inspect retained summaries, findings, outputs and the authorised source roots. It may **not** execute or change files, use the network or start another audit. Follow-up conversation is deliberately **not an independent reviewer pass**: Claude now sees prior reviewer conclusions. The original audits remain independent.

There is currently **no `--effort` flag for `/claude` conversations**; reviewer effort is selected in `/audit` or the default preset.

## 4. Working with scope guidance and upstream references

For UE4SS, the local paths are:

- Modified Linux-port tree: `/home/stef/paltest/tarballs/RE-UE4SS/deps/first/File`
- Local upstream checkout: `/home/stef/projects/RE-UE4SS/deps/first/File`
- Upstream provenance: https://github.com/UE4SS-RE/RE-UE4SS
- Reusable guidance: `/home/stef/projects/Audex/docs/audex-ue4ss-linux-port-guidance.md`

The guide describes the agreed job: implement the Linux backend while preserving its public API; use upstream to distinguish new errors from inherited behaviour; avoid a general Windows bug hunt; recognise deliberately scoped lenient UTF-8 and ABI-preserving mapping changes without suppressing actual defects.

Guided run, using the installed default pair:

```text
/audit /home/stef/paltest/tarballs/RE-UE4SS/deps/first/File /home/stef/projects/RE-UE4SS/deps/first/File --guidance /home/stef/projects/Audex/docs/audex-ue4ss-linux-port-guidance.md
```

The upstream directory is **technically another target**, but the guidance asks reviewers to use it as a baseline, not a separate bug-fixing assignment. The GitHub URL is provenance, not a fetch instruction; reviewers have no network access.

Source on Orac is mounted manually via SSHFS, for example:

```bash
sshfs paltest@192.168.4.23:/home/paltest /home/stef/paltest
```

WSL must have `sshfs` and the mountpoint available. The mount is not automatic. Audex itself stays installed on Zen; Orac needs no NFS, Samba or Audex daemon.

## 5. Authentication

**OpenAI:** On first run, Audex's normal interactive account login sets up the native reviewer credentials.

**Claude subscription:** Use an independent profile, rather than personal `~/.claude`:

```bash
mkdir -p -m 700 "$HOME/.audex/claude-profile"
CLAUDE_CONFIG_DIR="$HOME/.audex/claude-profile" claude auth login
```

The configured CLI is `/home/stef/.local/bin/claude`. Keep the dedicated profile private and never copy its tokens into guidance files. Both reviewer routes have already completed production audits using the configured subscriptions.

## 6. Building and installing the complete production package

The current release is **1.0.0**. The workspace version was raised from `0.0.0` so the daemon would request the full modern OpenAI model catalogue. Updating only the standalone CLI left the managed daemon reporting `0.0.0`; the remedy was to update its independently installed package.

When a complete new build already exists at `~/scratch/codex/Audex/production-package`:

```bash
audex app-server daemon stop

cp -a "$HOME/scratch/codex/Audex/production-package/." \
  "$HOME/.audex/packages/standalone/releases/1.0.0-x86_64-unknown-linux-gnu/"

audex app-server daemon update --from-cli
```

**Do not forget `update --from-cli` after replacing the executable package.** It installs the current standalone package as the managed daemon package. For configuration-only edits, do **not** rebuild or reinstall: just restart the daemon (`stop` then `start`).

The installed layouts and data paths are:

| Item | Path |
| --- | --- |
| `audex` entry point | `~/.local/bin/audex` |
| Selected standalone release | `~/.audex/packages/standalone/current` |
| Managed daemon release | `~/.audex/packages/app-server-daemon/current` |
| Current backend settings | `~/.audex/audex/reviewer-backends.json` |
| Retained run history | `~/.audex/audex/runs/` |

**The scratch build tree is disposable.** Future builds need a fresh Cargo target and locally verified V8 inputs if scratch has been cleaned. The proven build recipe was:

```bash
cd ~/projects/Audex
build="$HOME/scratch/codex/Audex"
v8="$build/production-inputs/rusty-v8-152.2.0-x86_64-unknown-linux-gnu"
export CARGO_TARGET_DIR="$build/target"
export CARGO_BUILD_JOBS=8
export RUSTY_V8_ARCHIVE="$v8/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.a.gz"
export RUSTY_V8_SRC_BINDING_PATH="$v8/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.rs"

CODEX_REPO_ROOT="$PWD" python3 scripts/build_codex_package.py \
  --variant audex \
  --target x86_64-unknown-linux-gnu \
  --cargo-profile release \
  --package-version 1.0.0 \
  --package-dir "$build/production-package" \
  --rg-bin "$(command -v rg)" \
  --force
```

See `docs/audex-build-install.md` for prerequisites and V8 input verification. Its older prerelease-version examples predate this 1.0.0 deployment. The `--force` flag replaces the **staging package directory**. The final Rust `audex(bin)` release compilation uses Thin LTO and can remain CPU-bound for several minutes; we observed it complete successfully.

## 7. Relevant regression tests

Claude reviewer effort support (`low`/`medium`/`high`/`xhigh`/`max`) was added to the launcher and verified through both Claude routes. **17/17 Claude tests passed**, and formatting checks passed.

The existing test fixture requires a synthetic API credential and a private, clean `HOME`. From `~/projects/Audex/codex-rs`, preserve the real Cargo and Rustup locations before isolating HOME:

```bash
(
  real_home="$HOME"
  test_home=$(mktemp -d)
  trap 'rm -rf -- "$test_home"' EXIT
  CARGO_HOME="$real_home/.cargo" \
  RUSTUP_HOME="$real_home/.rustup" \
  HOME="$test_home" \
  AUDEX_S3_TEST_KEY=synthetic-s3-key \
    cargo test --locked --offline -p codex-app-server --lib audex::service::claude::tests
)
```

This test-only secret is not a live API key. Rebuild/regression work may use `CARGO_TARGET_DIR` explicitly; clearing scratch removes the earlier target and its incremental compilation cache.

## 8. Practical acceptance/playbook

This is the next stage: exercise Audex as an operator, **not** convert UE4SS findings into an Audex development task.

1. Start Audex; check `audex --version` and `audex app-server daemon version`.
2. Submit a small file/directory to `/audit` using the defaults. Confirm **two** reviewers at **high** effort; inspect status and automatic comparison.
3. Ask the main model to display one full original reviewer report, a specific numbered finding, its evidence and source line.
4. Use `/audit list`, then `/audit read RUN_ID` to reopen past work.
5. Ask `/claude` a question about an active run; follow its offer with a brief response such as "Yes please do." Check continuity.
6. Exercise `--model`, `--audit` and `--no-audit` conversational options.
7. Run a focused guided audit. Evaluate whether findings respect the task scope and distinguish source provenance.
8. On a disposable run, test cancel/retry and preserved earlier attempts if useful.
9. Note concrete UI friction, surprising behaviour and reproducible faults. Pass UE4SS source findings to the separate UE4SS workstream, not the Audex acceptance backlog.
10. Do **not** request an audit of the full 5.7 GB UE4SS repository.

**Acceptance observations, 21 November 2026:**

| Run | Astra | Claude | Observed outcome |
| --- | ---: | ---: | --- |
| Unguided `File` audit | 26 (max) | 5 (max) | Four overlapping claims; broad library defect inventory |
| Guided `File` plus upstream | 14 (high) | 2 (high) | Narrower, port-focused findings with provenance distinctions |
| Claude retained discussion | — | Opus conversation | Brief follow-up correctly continued the prior offer |

Successful operation, reviewer isolation, evidence retention and guidance sensitivity are demonstrated by these runs. Whether any **particular code finding** is correct remains for its own code review; agreement is not verification.

## 9. Known boundaries and troubleshooting

| Symptom / limitation | Practical answer |
| --- | --- |
| CLI installed at new version but model menu remains stale | Managed daemon is independently installed; after binary updates run `audex app-server daemon update --from-cli`. |
| Default preset not picked up | Confirm `~/.audex/audex/reviewer-backends.json` and restart daemon. |
| Claude authentication fails | Check `CLAUDE_CONFIG_DIR=~/.audex/claude-profile`, not the ordinary Claude profile. |
| Orac source path inaccessible | Check the WSL SSHFS mount; Windows drive mappings need not appear in WSL. |
| GitHub fetch unavailable to reviewers | Deliberate network isolation. Supply a local checkout and explicit permitted paths. |
| Audit on two paths includes the reference | Current design treats both as targets; guidance alone distinguishes their intended roles. |
| Asking `/claude` for high effort | Not supported by conversational slash syntax; audit reviewers support effort. |
| Cargo offline tests try to fetch a toolchain | Don't redirect Cargo/Rustup to a clean HOME when isolating tests. |

**Promising future features, not current acceptance blockers:** controlled read-only access to GitHub reference sources, and first-class separate review targets versus reference directories.

**Do not erase** `~/.audex/audex/runs` or `~/.audex/packages` when cleaning build scratch.
