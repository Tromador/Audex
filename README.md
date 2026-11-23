# Audex

**Independent source-code audits with multiple AI reviewers, built on [OpenAI Codex](https://github.com/openai/codex).**

Audex is a personal fork of Codex that does one additional job: send the same selected source to independent reviewers, retain their original findings, and help the operator work through agreements, disagreements and supporting evidence.

It is not a general-purpose CI service, an automated code-fixing system, or an official OpenAI or Anthropic product. The aim is to make a second opinion on code useful without spending half the day shuffling text between chat windows.

## What it does

- **Independent reviews.** Run a native Codex reviewer and an official Claude CLI reviewer on the same file, set of paths, directory or supplied snippet. Reviewer/model selection and reasoning effort are configurable.
- **Live source, no invented scope.** Audex passes directory targets directly to reviewers. It does not enumerate them, generate file inventories, apply inclusion rules, or snapshot the source tree. Reviewers investigate using their own tools.
- **Read-only execution.** Native reviewers use Codex's sandboxed inspection; Claude uses a restricted set of read-only source and reference tools, a dedicated profile, and disabled project hooks.
- **Useful reference access.** Reviewers can consult selected GitHub read tools, Context7 documentation and public web search. An optional private Zen Storage connector also permits read-only access to files outside the local Linux filesystem. Reference lookup is distinct from unrestricted shell networking.
- **Retained evidence.** Audit runs, original reviewer output, findings and diagnostics remain available for inspection, reopening, cancellation and reviewer retries.
- **Compare and challenge.** The Codex TUI can reconcile reviewers' conclusions, show individual findings and sources, and support follow-up discussion with Claude through `/claude`.

A finding is a model's claim, not a proven defect. Agreement between models can be useful, but is not verification. A failed reviewer is not silently counted as a clean review.

## About Zen Storage

**Zen Storage is a private MCP (Model Context Protocol) filesystem service used by the original Audex installation.** It is not part of Audex, is not published as a downloadable product, and is not something a third party can install by following this README.

It connects selected filesystems to ChatGPT and the Audex reviewers through a Windows-hosted service and tunnel. Those locations are exposed as drive-prefixed paths, for example `O:/home/example/project/src/file.cpp`. Depending on what the operator has connected, these may represent files on another computer, a Windows drive, or other storage. The reviewers use the connector's **read-only** operations to inspect those files. Audex recognises the drive-prefixed target and passes it to the connector instead of treating it as a normal Linux pathname.

**Zen Storage is optional.** Ordinary Audex reviews of local files and directories do not need it. GitHub, Context7 and web-search capabilities are separate integrations. Examples using `O:/...` specifically demonstrate the author's private setup; they are not portable paths or general installation instructions. Without that connector, use source paths available to the machine running Audex.

## Quick example

Audex uses a managed app-server. With an already configured installation:

~~~bash
audex app-server daemon start
audex --remote unix://
~~~

Inside the **Audex TUI**:

~~~text
/audit /path/to/project/src
~~~

The configured default preset starts the selected reviewers independently. An explicit pair looks like this:

~~~text
/audit /path/to/source.cpp --reviewer native:gpt-6.2-astra:high --reviewer claude_subscription:claude-opus-5-5:high
~~~

You can focus the job with a brief and guidance:

~~~text
/audit /path/to/source.cpp --brief "Find concrete defects in ownership and error handling." --guidance-text "Preserve the public API; distinguish source evidence from assumptions."
~~~

On an installation with the private Zen Storage connector configured, reviewers can also compare a remote file with upstream GitHub contents:

~~~text
/audit O:/home/example/project/src/file.cpp --guidance-text "Use read-only GitHub MCP to compare this file against owner/repository:src/file.cpp on main. Report differences and any access failures."
~~~

Inspect and discuss an audit after it runs:

~~~text
/audit list
/audit read RUN_ID
/claude --audit RUN_ID Challenge the reasoning behind reviewer 1's second finding.
~~~

A new audit uses a freshly selected reviewer roster. Existing reviewer attempts can be retried, but another independent reviewer cannot currently be added to a run already created.

## Running and building it

Audex retains the underlying Codex codebase and builds a separate `audex` executable. Its current operator-managed build and deployment target is **Linux x86-64, including WSL**. Using Audex requires the appropriate model/client authentication and an installed managed daemon.

The executable can be compiled from the source tree with:

~~~bash
cd codex-rs
cargo build --release -p codex-cli --bin audex
~~~

**Building the executable alone is not a complete installation.** Deployment also needs its companion executables and packaged resources, followed by installation or update of the independently managed daemon. See the handbook rather than using an upstream Codex installer.

For the current behaviour, reviewer configuration, external tool permissions, full slash-command reference and deployment workflow, read the **[Audex operator handbook](docs/audex-production-operator-handbook.md)**.

## Project relationship and licence

Audex is derived from [OpenAI Codex](https://github.com/openai/codex). Much of the source tree remains upstream Codex; Audex-specific orchestration and TUI integration live in the Rust application. Upstream Codex's installation instructions install **Codex**, not Audex.

This is an independent project. The original Codex documentation is available at [developers.openai.com/codex](https://developers.openai.com/codex).

The repository is licensed under [Apache-2.0](LICENSE).
