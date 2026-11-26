# Build, install and update Audex on WSL

> **Current installation layout (November 2026):** Audex uses `~/.audex/packages/standalone/current` with `~/.local/bin/audex` pointing to its `bin/audex`; the managed daemon uses `~/.audex/packages/app-server-daemon/current`. The installation and update procedures further down this document describe an older layout and must not be used for the current deployment. Follow [the production operator handbook](audex-production-operator-handbook.md#build-and-update-on-linuxwsl) instead.

Audex builds a real `audex` executable. The compatibility `codex` executable uses the same CLI implementation. Internal helpers and the managed daemon retain their existing Codex filenames.

Use a complete package for normal operation. Copying the application executable alone omits the code-mode host, execution server, sandbox and search resources.

## Current development and release build procedure (Linux x86-64 / WSL)

This procedure was established during the 1.1.0 native-continuity work. It applies to source development and candidate packages, independently of the installed Audex version. The historical build and installation examples below are retained for reference, not as the current release recipe.

### Toolchain and verified V8 inputs

Use the repository's pinned Rust toolchain and native build tools. Install the separate-process test runner if it is not already available:

```bash
cargo install --locked cargo-nextest
```

Scope Cargo operations to the packages under development. An unscoped workspace build/check can pull in unrelated ALSA, GLib/GStreamer and V8 dependencies. The CLI-only build does not require the code-mode host, but a complete package and the native tool-availability tests do.

The code-mode host requires the Codex-built V8 archive and generated Rust bindings for the version resolved in Cargo.lock, the target and the ptrcomp_sandbox_release profile. Use the existing packaging helper to obtain both inputs and authenticate the release checksums against the repository's pinned manifest. From the repository root, in a Bash shell:

```bash
export AUDEX_BUILD_ROOT="$HOME/scratch/Audex"
mkdir -p "$AUDEX_BUILD_ROOT"

V8_INPUTS=$(CODEX_REPO_ROOT="$PWD" PYTHONPATH="$PWD/scripts" python3 -c '
import os
from pathlib import Path
from codex_package.targets import TARGET_SPECS
from codex_package.v8 import fetch_codex_v8_artifacts

pair = fetch_codex_v8_artifacts(
    TARGET_SPECS["x86_64-unknown-linux-gnu"],
    cache_root=Path(os.environ["AUDEX_BUILD_ROOT"]) / "v8",
)
print(pair.archive.parent)
') && export \
    RUSTY_V8_ARCHIVE="$V8_INPUTS/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.a.gz" \
    RUSTY_V8_SRC_BINDING_PATH="$V8_INPUTS/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.rs"
```

The helper downloads missing inputs and verifies cached inputs. It resolves the version from Cargo.lock, so the acquisition command does not need editing on V8 version changes. Do not substitute a musl archive for the GNU target. Both environment overrides must be set together. Keep them available in the shell for the code-mode host build and release packaging; reuse a verified cache if available.

### Regression test environment

Run one Cargo operation at a time from codex-rs. This example uses the standard debug target directory; the tests locate companion binaries in that directory.

```bash
cd codex-rs
export TMPDIR="$AUDEX_BUILD_ROOT/tmp"
mkdir -p "$TMPDIR"
export AUDEX_CLIENT_TEST_RUNTIME_ROOT="$TMPDIR"
export RUST_MIN_STACK=16777216
export INSTA_UPDATE=no
TEST_HOME=$(mktemp -d "$AUDEX_BUILD_ROOT/home.XXXXXX")

t() {
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    HOME="$TEST_HOME" \
    AUDEX_S3_TEST_KEY=synthetic-s3-key "$@"
}
```

- Unoptimised Core tests can overflow the default 2 MiB test-thread stack. A 16 MiB stack was sufficient in verification.
- Use an empty private HOME: Claude tests require that no personal .claude directory or .claude.json exists there. The synthetic credential is a fixture, not a real API key.
- HOME and TMPDIR must be separate sibling directories, not nested. A test HOME beneath TMPDIR causes the native sandbox write-denial regression test to fail.
- Keep Cargo and Rustup pointed at the real toolchain/cache even when HOME is replaced.
- INSTA_UPDATE=no avoids generating pending snapshot files during test runs.
- AUDEX_CLIENT_TEST_RUNTIME_ROOT is required by the Claude conversation integration fixtures, which create and manage their own temporary runtime directories beneath that root.

Format and compile the affected packages, then build the actual helper binaries required by the Audex tests:

```bash
cargo fmt --check -p codex-protocol -p codex-core -p codex-app-server -p codex-app-server-client -p codex-tui
cargo check -p codex-app-server -p codex-tui --tests
cargo build -p codex-cli --bin audex
cargo build -p codex-code-mode-host
```

The following are the regression suites used for native continuity. Select the relevant suites for subsequent changes; a filter selecting zero tests is not a pass.

```bash
t cargo test -p codex-core --lib session::turn_input
t cargo test -p codex-app-server --lib audex
t cargo test -p codex-app-server --lib listener_shutdown_tests
t cargo test -p codex-app-server --test all suite::v2::audex::
t cargo test -p codex-app-server --test all suite::v2::daemon_update_recovery::
t cargo test -p codex-app-server --test all suite::v2::thread_resume::
t cargo test -p codex-app-server-client --lib audit
```

For Claude continuity changes, run the Audex app-server unit and integration suites above together with the Claude conversation projection and integration suites:

```bash
t cargo test -p codex-app-server --lib turn_processor::claude
t cargo test -p codex-app-server --test all suite::v2::claude_conversation::
```

For the full TUI unit suite use nextest, not plain cargo test. The upstream tests assume per-test process isolation; shared processes can produce duplicate Insta assertions and leak process-global state. TUI tests also emit terminal control sequences and query the terminal, so detach from the controlling terminal as well as redirecting stdin and output:

```bash
NEXTEST_PROFILE=local t setsid -w cargo nextest run --no-fail-fast -p codex-tui --lib \
    </dev/null >"$TMPDIR/tui-tests.log" 2>&1
```

The validated native-continuity run used nextest 0.9.152 and returned 6,398 passed (one flaky test passed on retry), four skipped. The intermittent test was app::tests::session_lifecycle_requests::dynamic_tool_requests_ignore_other_namespaces_and_dispatch_tui_namespace, with a thread/resume list_turns error. If it fails again, rerun it in isolation before proposing changes; its underlying cause has not been established.

The TUI uses the upstream 0.0.0 version and non-WSL shortcut baseline in test builds to avoid host/version-dependent snapshots. Production builds retain the actual version and WSL detection.

When finished, remove the private test HOME and the task-owned temporary test files/directories after inspecting logs. Preserve verified V8 inputs until the build work is finished. No development cleanup should touch the installed packages or retained audit runs.

### Assemble the complete release candidate

From the repository root, ensure Cargo.lock matches the workspace version in codex-rs/Cargo.toml. Normal Cargo development builds update workspace entries; if necessary, update them explicitly before packaging:

```bash
cargo update --manifest-path codex-rs/Cargo.toml --workspace --offline
```

The compiled workspace version is reported to the provider and affects model availability. The package builder defaults its manifest version to the workspace version; omit --package-version for a normal release. The packaging flag alone does not change the compiled version.

With the verified V8 overrides still exported:

```bash
build="$AUDEX_BUILD_ROOT/release"
export CARGO_TARGET_DIR="$build/target"
export CARGO_BUILD_JOBS=8

CODEX_REPO_ROOT="$PWD" python3 scripts/build_codex_package.py \
  --variant audex \
  --target x86_64-unknown-linux-gnu \
  --cargo-profile release \
  --package-dir "$build/production-package" \
  --rg-bin "$(command -v rg)"

"$build/production-package/bin/audex" --version
```

This is the package-builder entry point used by the just recipe, without requiring just to be installed. Source packaging uses locked, offline Cargo resolution; dependencies must already be cached. The selected V8 artifacts must be available locally. Add --force only when intentionally replacing the scratch package at that destination.

Check the executable's version against Cargo.toml before deploying. Follow the [operator handbook](audex-production-operator-handbook.md#build-and-update-on-linuxwsl) for the current standalone and managed daemon layout. Building the candidate does not establish interactive acceptance.

## Build prerequisites (historical example)

Use the Rust toolchain selected by `codex-rs/rust-toolchain.toml`, a working native build toolchain, and locally available locked Cargo dependencies. No build command below installs dependencies or downloads models.

The code-mode host also needs the V8 archive and generated Rust bindings matching the locked V8 version, target and sandbox configuration. Obtain these inputs separately from the official Codex release and verify their release manifest against `third_party/v8/rusty_v8_*_release_manifests.sha256`. Then verify the archive and bindings against that authenticated manifest. A Cargo offline flag alone does not stop third-party build scripts fetching their own inputs.

The following example selects the GNU Linux target for x86-64 WSL. The daemon package target must match the application build target. Use the corresponding target-specific V8 inputs; do not substitute a musl archive for a GNU build.

Start from a clean checkout of the source revision you intend to package. Replace the `/path/to/...` placeholders with explicit local paths. Keep the checkout unchanged throughout the build and assembly: a commit ID does not identify uncommitted edits.

```bash
cd /path/to/Audex
AUDEX_SOURCE_REV="$(git rev-parse --verify HEAD)"
export AUDEX_SOURCE_REV
export AUDEX_PACKAGE_VERSION="0.0.0-audex.g$AUDEX_SOURCE_REV"
printf 'Source revision: %s\nPackage version: %s\n' "$AUDEX_SOURCE_REV" "$AUDEX_PACKAGE_VERSION"
export CODEX_REPO_ROOT="$PWD"
export TMPDIR="/path/to/audex-build/tmp"
mkdir -p "$TMPDIR"
export CARGO_NET_OFFLINE=true
export AUDEX_V8_INPUTS=/path/to/verified/local/v8-inputs
export RUSTY_V8_ARCHIVE="$AUDEX_V8_INPUTS/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.a.gz"
export RUSTY_V8_SRC_BINDING_PATH="$AUDEX_V8_INPUTS/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.rs"
test -f "$RUSTY_V8_ARCHIVE"
test -f "$RUSTY_V8_SRC_BINDING_PATH"
unset RUSTY_V8_SKIP_DOWNLOAD
```

Here the V8 archive setting names a verified local file. The build script can extract that file; it has no need to retrieve an archive or bindings from the network. Retain the input checksums, target and profile with the resulting package. The revision-bearing `AUDEX_PACKAGE_VERSION` is written to its manifest; the `g` prefix makes the revision a valid semantic-version prerelease identifier.

## Build and assemble a complete package

The explicit build keeps the compatibility application available. The package recipe selects the genuine Audex target and all required companions.

```bash
export AUDEX_RG_BIN="$(command -v rg)"
export AUDEX_PACKAGE_STAGE="/path/to/audex-build/package-$AUDEX_SOURCE_REV"
(
    set -e
    test ! -e "$AUDEX_PACKAGE_STAGE"
    test ! -L "$AUDEX_PACKAGE_STAGE"
    cd codex-rs
    cargo build --locked --offline \
      --target x86_64-unknown-linux-gnu --profile dev-small \
      --bin audex --bin codex --bin codex-code-mode-host \
      --bin codex-exec-server --bin bwrap
    cd ..
    python3 scripts/build_codex_package.py \
      --variant audex --target x86_64-unknown-linux-gnu \
      --cargo-profile dev-small --strip none \
      --package-version "${AUDEX_PACKAGE_VERSION:?select a source revision first}" \
      --rg-bin "$AUDEX_RG_BIN" --package-dir "$AUDEX_PACKAGE_STAGE"
)
```

`just assemble-audex-package` is the same package-builder entry point with `--variant audex` supplied. Audex packaging requires an explicit local ripgrep executable. Its source builds use locked, offline Cargo resolution and refuse missing cached V8 inputs rather than download them.

An already built, verified companion can be supplied explicitly with the builder's `--code-mode-host-bin`, `--exec-server-bin` or `--bwrap-bin` option. Record its provenance and checksum. That is package assembly with a prebuilt input, not evidence that the companion was freshly built from the selected revision.

The package contains:

```text
codex-package.json
bin/audex
bin/codex-code-mode-host
bin/codex-exec-server
codex-resources/bwrap
codex-path/rg
```

Check the staged manifest and executable directly before installation. The manifest must show `variant: audex`, `entrypoint: bin/audex` and the exact expected package version printed below:

```bash
printf 'Expected package version: %s\n' "$AUDEX_PACKAGE_VERSION"
python3 -m json.tool "$AUDEX_PACKAGE_STAGE/codex-package.json"
"$AUDEX_PACKAGE_STAGE/bin/audex" --version
"$AUDEX_PACKAGE_STAGE/bin/audex" audit --help
"$AUDEX_PACKAGE_STAGE/bin/audex" completion bash
```

The manifest version identifies the packaged source revision. `audex --version` and the daemon's application version report the compiled workspace version; both can remain `0.0.0` across different source revisions. They do not replace the manifest checks.

Help and completion do not commission inference. Set an explicit disposable `CODEX_HOME` when validating a candidate against test runtime state.

## Install and select the local backend

This section changes the installation, PATH and managed runtime. Apply it only when those changes are authorised. It is separate from building a candidate package.

Inspect existing command resolution first:

```bash
type -a audex
```

A shell function can shadow a real executable even after PATH changes. Retire an existing wrapper or function deliberately; keep its runtime-home selection and preserve existing audit data.

Install the full package at a stable location. The block keeps one previous package and refuses an existing `package.new` or `package.previous`, including dangling symlinks. Its subshell enables `errexit` itself, so a failed precondition stops the block even when pasted into an ordinary shell. Run it as a standalone block, without wrapping it in an `if`, `&&` or `||` condition that suppresses that behaviour.

```bash
export AUDEX_INSTALL_ROOT="$HOME/.local/share/audex"
(
    set -e
    test -d "${AUDEX_PACKAGE_STAGE:?select a staged package first}"
    test -f "$AUDEX_PACKAGE_STAGE/codex-package.json"
    test -x "$AUDEX_PACKAGE_STAGE/bin/audex"
    mkdir -p "$AUDEX_INSTALL_ROOT"
    test ! -e "$AUDEX_INSTALL_ROOT/package.new"
    test ! -L "$AUDEX_INSTALL_ROOT/package.new"
    test ! -e "$AUDEX_INSTALL_ROOT/package.previous"
    test ! -L "$AUDEX_INSTALL_ROOT/package.previous"
    cp -a -- "$AUDEX_PACKAGE_STAGE" "$AUDEX_INSTALL_ROOT/package.new"
    if [ -e "$AUDEX_INSTALL_ROOT/package" ] || [ -L "$AUDEX_INSTALL_ROOT/package" ]; then
        test -d "$AUDEX_INSTALL_ROOT/package"
        test ! -L "$AUDEX_INSTALL_ROOT/package"
        mv -T -- "$AUDEX_INSTALL_ROOT/package" "$AUDEX_INSTALL_ROOT/package.previous"
    fi
    mv -T -- "$AUDEX_INSTALL_ROOT/package.new" "$AUDEX_INSTALL_ROOT/package"
    printf 'Expected package version: %s\n' "$AUDEX_PACKAGE_VERSION"
    python3 -m json.tool "$AUDEX_INSTALL_ROOT/package/codex-package.json"
    "$AUDEX_INSTALL_ROOT/package/bin/audex" --version
)
```

Only after that block succeeds, add the installed directory to PATH and check resolution:

```bash
export PATH="$AUDEX_INSTALL_ROOT/package/bin:$PATH"
hash -r
type -a audex
audex --version
```

Add that PATH directory to the normal shell startup configuration using `vi` if persistent shell configuration changes are authorised. Verify resolution in a fresh shell. The application itself remains a regular compiled file; no application symlink or shell launcher is required.

Without an explicit `CODEX_HOME`, the Audex build creates and uses `$HOME/.chatgpt/Audex/codex-home`. An explicit `CODEX_HOME` overrides that default; use a non-empty directory path and create it before starting. Use the same setting for every CLI, TUI and daemon command. The separate compatibility `codex` build keeps its existing home behaviour.

Start the established managed daemon and explicitly select this complete package. `update --from-cli` uses the package containing the invoking executable, not the package most recently built elsewhere. Invoke the installed path directly here so a shadowing command cannot select an older package:

```bash
"$AUDEX_INSTALL_ROOT/package/bin/audex" app-server daemon start
"$AUDEX_INSTALL_ROOT/package/bin/audex" app-server daemon update --from-cli
"$AUDEX_INSTALL_ROOT/package/bin/audex" app-server daemon version
"$AUDEX_INSTALL_ROOT/package/bin/audex" audit list
"$AUDEX_INSTALL_ROOT/package/bin/audex" --remote unix://
```

Package replacement asks for confirmation and warns if running work may be interrupted. The existing daemon machinery copies and pins the local package. Its internal `bin/codex` file contains the compiled Audex application, preserving Audex home/update behaviour and package provenance. Internal managed selection links are separate from the operator-facing executable identity.

Check the managed daemon's selected release under the effective runtime home, preserving an explicit `CODEX_HOME` override:

```bash
(
    set -e
    AUDEX_RUNTIME_HOME="${CODEX_HOME-$HOME/.chatgpt/Audex/codex-home}"
    AUDEX_SELECTED_RELEASE="$(readlink -f "$AUDEX_RUNTIME_HOME/packages/app-server-daemon/current")"
    test -d "$AUDEX_SELECTED_RELEASE"
    printf 'Selected release: %s\nExpected package version: %s\n' "$AUDEX_SELECTED_RELEASE" "$AUDEX_PACKAGE_VERSION"
    python3 -m json.tool "$AUDEX_SELECTED_RELEASE/codex-package.json"
)
```

Its `version` must match the staged and installed manifests' revision-bearing version. Its `variant` remains `audex`; daemon normalisation changes `entrypoint` to `bin/codex` and records `sourceEntrypoint: bin/audex`. The selected directory is named `local-<digest>`; that digest identifies the package contents, not the Git revision.

`daemon version` reports the running application and managed path. The manifest identifies the selected package, which can exist while the daemon is stopped. Verify audit capability through `audit list`. CLI audit commands use that dedicated managed runtime; the explicit remote TUI command connects to it too.

## Update from a selected source revision

Build and validate a new complete candidate from the chosen source revision using the same explicit target, profile and verified inputs. Recompute `AUDEX_SOURCE_REV` and `AUDEX_PACKAGE_VERSION` and use a fresh staging destination.

Before a subsequent replacement, explicitly retire `package.previous` once you no longer need that backup. This deletes only the previous installed package; retain it elsewhere first if you still need it:

```bash
(
    set -e
    test -n "${AUDEX_INSTALL_ROOT:?set the installation root first}"
    test -d "$AUDEX_INSTALL_ROOT/package.previous"
    test ! -L "$AUDEX_INSTALL_ROOT/package.previous"
    rm -r -- "$AUDEX_INSTALL_ROOT/package.previous"
)
```

If there is no previous backup, omit that retirement block. If `package.new` remains from an unsuccessful installation, inspect it and remove or relocate it before retrying; do not bypass the installation guards.

Repeat the installation block and resolution checks above, preserving the currently installed package as the new backup. Then invoke the newly installed executable:

```bash
"$AUDEX_INSTALL_ROOT/package/bin/audex" app-server daemon update --from-cli
"$AUDEX_INSTALL_ROOT/package/bin/audex" app-server daemon version
"$AUDEX_INSTALL_ROOT/package/bin/audex" audit list
```

Repeat the selected-release manifest check. Its version must now match the new staged and installed package version; the previous version remains in `package.previous/codex-package.json`.

Reopen the TUI after the daemon replacement. Existing runtime data is preserved. Do not replace a package while an unreviewed or unwanted audit is still running.

`audex update` prints local-package update guidance and refuses upstream replacement. Audex also excludes public upstream update choices from its TUI and pins its daemon against automatic upstream updates. The separate Codex application retains its normal update behaviour.
