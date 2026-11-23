# Audex operator handbook

This handbook covers the current Audex 1.0.0 source-audit workflow and the local Linux/WSL deployment. Audex is a fork of [OpenAI Codex](https://github.com/openai/codex), not a distribution installed by upstream Codex's installers.

Audex starts independent code reviewers, keeps their original results, and lets the operator compare or discuss their findings. The coordinator does not perform its own source inventory or snapshot the target. For an introduction, see the [repository README](../README.md).

## Start an audit

With a configured installation and managed app-server:

~~~bash
audex app-server daemon start
audex --remote unix://
~~~

Enter the following **inside the TUI**, not in a shell:

~~~text
/audit /absolute/path/to/source
~~~

The installed `default` preset determines which reviewers run. An example two-reviewer configuration is:

| Reviewer | Route | Model | Effort |
| --- | --- | --- | --- |
| OpenAI | `native` | `gpt-6.2-astra` | `high` |
| Anthropic | `claude_subscription` | `claude-opus-5-5` | `high` |

These are configured choices, not hard-coded model requirements. The installation's selections live in `~/.audex/audex/reviewer-backends.json`.

A normal audit can be as small as one source file. For a directory, Audex passes the directory path to each reviewer without listing its files, applying filters, hashing it or constructing a snapshot. Each reviewer decides what to inspect. Explicit file targets also allow reviewers to read their containing directories for context. Sources remain **live**; findings relate to what the reviewers observed during that run, not to a preserved source revision.

## Audit command reference

~~~text
/audit PATH [PATH ...]
       [--with PRESET]
       [--reviewer ROUTE:MODEL[:EFFORT]] ...
       [--brief "TEXT"]
       [--guidance FILE | --guidance-text "TEXT"]

/audit --snippet "SOURCE TEXT" --name filename.ext [options]
~~~

| Input | Behaviour |
| --- | --- |
| `PATH` | Existing file or directory. Quote paths containing spaces. Relative paths use the TUI thread's working directory. |
| `--with PRESET` | Use the named preset from reviewer-backends.json. |
| `--reviewer ROUTE:MODEL[:EFFORT]` | Choose an explicit reviewer; repeat for independent reviewers. |
| `--brief TEXT` | Replace the default objective, “Find concrete defects supported by evidence in the selected source.” |
| `--guidance FILE` | Read operator guidance from a local UTF-8 file. |
| `--guidance-text TEXT` | Supply guidance inline instead of from a file. |
| `--snippet` and `--name` | Review supplied source text instead of filesystem paths. Both are required. |
| `--` | Stop option parsing; remaining arguments are paths. |

With neither `--with` nor `--reviewer`, Audex uses `default`. Explicit `--reviewer` selections alone replace the default; combining `--with` and `--reviewer` adds the explicit reviewers to the chosen preset.

Examples:

~~~text
/audit /path/to/project/src --brief "Find concrete ownership and I/O defects."

/audit /path/to/source.cpp --reviewer native:gpt-6.2-astra:high --reviewer claude_subscription:claude-opus-5-5:high

/audit /path/to/file.cpp --guidance-text "Compare against upstream through read-only GitHub MCP. Report source differences, not presumed bugs."

/audit --snippet "int f() { return 42; }" --name example.cpp
~~~

Multiple supplied paths are all audit targets. There is no separate reference-path parameter; guidance can explain a reference's intended role without changing that fact.

## Read, compare, retry and discuss

~~~text
/audit list
/audit list --offset N
/audit read RUN_ID
/audit cancel RUN_ID
/audit cancel RUN_ID REVIEWER
/audit retry RUN_ID REVIEWER
~~~

`list` browses the retained run history; `read` reopens a run; `cancel` stops a whole run or one reviewer; `retry` starts another attempt for a selected reviewer and retains the previous attempt. Reviewer selectors are one-based positions or stored labels such as `reviewer-2`.

After multiple reviewers complete, Audex can compare their original reports, separating agreement, disagreement and unique findings. A reviewer failure is not a clean review and does not count as agreement. Source-line references and model confidence are evidence to inspect, **not automatic verification of a defect**.

Ask the TUI model to show original output, specific findings and evidence, or diagnostics. These are conversational requests, not `/audit` subcommands. For example:

~~~text
Show reviewer 2's original report from the active audit.
Show finding 3 with its evidence and source reference.
Compare the reviewers' conclusions and identify any unsupported claims.
~~~

For a Claude conversation:

~~~text
/claude [--model MODEL] [--audit RUN_ID | --no-audit] MESSAGE
~~~

Without an explicit run, `/claude` uses the active audit where applicable. `--audit` selects retained audit context; `--no-audit` starts an ordinary conversation. Conversational follow-up is not an additional independent reviewer: it can see the preceding discussion. The `/claude` command has no effort override.

Audex can retry an existing reviewer, but **cannot add a new reviewer to an already created audit**. To change the reviewer roster, start another audit.

## External reference tools and remote files

Audit reviewers can use selected, explicitly authorised reference tools; they are **not limited to offline source reads**.

| Capability | Native Codex reviewer | Claude reviewer |
| --- | --- | --- |
| Local source inspection | Codex's read-only sandboxed inspection | Official Claude client `Read`, `Grep` and `Glob` |
| Remote file paths | Read-only Zen Storage connector | Read-only Zen Storage MCP |
| GitHub references | Selected read-only GitHub connector tools | Selected read-only GitHub MCP tools |
| Library documentation | Context7 | Context7 MCP |
| Public web references | Web search | `WebSearch` |

These tools are for gathering reference evidence. They do not turn an audit into a general web crawler, give reviewers arbitrary networked shell access, or authorise source changes. External content is untrusted; the reviewed code and its configuration must not override audit permissions.

For a remote target exposed by Zen Storage, supply its drive-prefixed path directly, for example:

~~~text
/audit O:/home/example/project/src/file.cpp --brief "Compare this implementation with its upstream counterpart."
~~~

A Zen Storage path is passed through as an opaque target. It does not need to be mounted as a Linux directory. For read-only comparisons with GitHub, include the repository, branch and file path in guidance so reviewers can identify the reference unambiguously:

~~~text
/audit O:/home/example/project/src/file.cpp --guidance-text "Retrieve owner/repo:src/file.cpp on main through the read-only GitHub MCP tool. Compare it against the supplied local file. State any access failures rather than substituting memory or an unverified source."
~~~

Both native and Claude reviewers have been exercised with GitHub and Zen Storage together. Context7 and public web search have also been exercised independently. External tool access remains conditional on the connector being configured and available in the running installation.

### Claude tool selection and isolation

For audit runs, Audex launches the official Claude CLI with a dedicated profile, project settings excluded (`--setting-sources user`), hooks disabled (`disableAllHooks`), `--strict-mcp-config` and `--permission-mode dontAsk`. Its built-in source tools and selected MCP names are allowed explicitly. Project files cannot grant their own tools or executable hooks permission.

Audex selects configured Context7, GitHub and (when needed) Zen Storage servers from the operator's `~/.claude.json`. Only a deliberately maintained list of read-only MCP tool names is permitted. The actual Claude GitHub tool interface includes `mcp__github__get_file_contents`, `mcp__github__list_commits` and `mcp__github__search_code`. A tool that exists but is missing from the allowlist will be denied by `dontAsk`.

The conversational `/claude` route is more restricted than an independent audit. It is not a way to launch another audit or perform unrestricted external research.

## Reviewer configuration and authentication

The following is an **example** `~/.audex/audex/reviewer-backends.json`. Adjust model choices and absolute executable/profile paths for the machine:

~~~json
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
    "executable": "/home/your-user/.local/bin/claude",
    "profile": "/home/your-user/.audex/claude-profile",
    "runtimeRoot": "/home/your-user/.audex/tmp"
  }
}
~~~

The native reviewer uses Codex's established authentication. Claude subscription uses the official Claude CLI and a **separate private profile**; do not reuse personal `~/.claude` or put tokens in the JSON configuration.

A typical first-time Claude profile setup is:

~~~bash
mkdir -p -m 700 "$HOME/.audex/claude-profile"
CLAUDE_CONFIG_DIR="$HOME/.audex/claude-profile" claude auth login
~~~

Changing the reviewer settings does not require a Rust rebuild. Exit the TUI and restart the managed daemon:

~~~bash
audex app-server daemon stop
audex app-server daemon start
~~~

Access to the selected external services depends on the user's own credentials, installed clients and connector configuration.

## Build and update on Linux/WSL

This section describes the existing Linux x86-64 local-package layout. It is **not** an upstream Codex installer and does not claim turnkey installation on other systems.

The Rust executable is built from the repository:

~~~bash
cd /path/to/Audex/codex-rs
cargo build --release -p codex-cli --bin audex
~~~

A normal installation also needs companion binaries and resources, including the code-mode host, execution server, sandbox helper and ripgrep. Use `scripts/build_codex_package.py` to assemble a complete Audex package for first installation or when those components change. Check its options and the repository's [build prerequisites](audex-build-install.md) before assembling from scratch; the prerequisites include matching local V8 inputs.

The established managed layout is:

| Component | Location |
| --- | --- |
| Operator-facing CLI | `~/.local/bin/audex` |
| Selected standalone package | `~/.audex/packages/standalone/current` |
| Independently installed daemon | `~/.audex/packages/app-server-daemon/current` |
| Backend settings | `~/.audex/audex/reviewer-backends.json` |
| Durable run records | `~/.audex/audex/runs/` |

For **an existing complete installation**, when only the Audex executable changes and its companion binaries/resources remain valid, close the TUI and refresh it:

~~~bash
cd /path/to/Audex
audex app-server daemon stop
install -m 755 codex-rs/target/release/audex \
  "$HOME/.audex/packages/standalone/current/bin/audex"
audex app-server daemon update --from-cli
~~~

The daemon runs from its **own selected package**. Replacing only the standalone executable does not update the running daemon. For an assembled new package, install it in the standalone release location first, then run `audex app-server daemon update --from-cli`. For configuration-only changes, restart the daemon; no recompilation is necessary.

The run database and package directories are persistent state, not build scratch. Do not delete `~/.audex/audex/runs` or `~/.audex/packages` during cleanup.

## Practical diagnostics

| Symptom | Check |
| --- | --- |
| The CLI shows new code, but TUI or audit behaviour is unchanged | Verify the managed daemon's selected package and run `audex app-server daemon update --from-cli`. |
| An audit uses the wrong models | Inspect `reviewer-backends.json` and restart the daemon after changes. |
| Claude cannot authenticate | Check its dedicated profile and executable location, not the operator's personal Claude profile. |
| Claude sees a GitHub MCP tool but receives “permission denied … don't ask mode” | Compare the **actual advertised tool name** with the explicit read-only allowlist in `codex-rs/app-server/src/audex/service/claude/launch.rs`. Do not disable `dontAsk` to get past the error. |
| A Zen Storage target cannot be read | Check that its drive is exposed and the Zen Storage connector is available. |
| A reviewer reports no findings after a tool failure | Inspect the original report and diagnostics. Lack of access is not evidence that the source is clean. |
| A reference directory is treated as another audit target | This is the current target model; use guidance to state its reference role. |

For direct inspection of code and supported flags, consult `codex-rs/tui/src/audit_service.rs` and `codex-rs/app-server/src/audex/`.

Audex is designed for operator-directed, independent review. Its value is in evidence and useful disagreement, not in treating model agreement as proof.
