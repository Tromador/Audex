# Build, install and update Audex on WSL

Audex builds a real `audex` executable. The compatibility `codex` executable uses the same CLI implementation. Internal helpers and the managed daemon retain their existing Codex filenames.

Use a complete package for normal operation. Copying the application executable alone omits the code-mode host, execution server, sandbox and search resources.

## Build prerequisites

Use the Rust toolchain selected by `codex-rs/rust-toolchain.toml`, a working native build toolchain, and locally available locked Cargo dependencies. No build command below installs dependencies or downloads models.

The code-mode host also needs the V8 archive and generated Rust bindings matching the locked V8 version, target and sandbox configuration. Obtain these inputs separately from the official Codex release and verify their release manifest against `third_party/v8/rusty_v8_*_release_manifests.sha256`. Then verify the archive and bindings against that authenticated manifest. A Cargo offline flag alone does not stop third-party build scripts fetching their own inputs.

The following example selects the GNU Linux target for x86-64 WSL. The daemon package target must match the application build target. Use the corresponding target-specific V8 inputs; do not substitute a musl archive for a GNU build.

```bash
cd /path/to/Audex
git rev-parse HEAD
export CODEX_REPO_ROOT="$PWD"
export TMPDIR="$HOME/scratch/codex/Audex/tmp"
mkdir -p "$TMPDIR"
export CARGO_NET_OFFLINE=true
export AUDEX_V8_INPUTS=/path/to/verified/local/v8-inputs
export RUSTY_V8_ARCHIVE="$AUDEX_V8_INPUTS/librusty_v8_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.a.gz"
export RUSTY_V8_SRC_BINDING_PATH="$AUDEX_V8_INPUTS/src_binding_ptrcomp_sandbox_release_x86_64-unknown-linux-gnu.rs"
test -f "$RUSTY_V8_ARCHIVE"
test -f "$RUSTY_V8_SRC_BINDING_PATH"
unset RUSTY_V8_SKIP_DOWNLOAD
```

Here the V8 archive setting names a verified local file. The build script can extract that file; it has no need to retrieve an archive or bindings from the network. Record the source revision, input checksums, target and profile with the resulting package.

## Build and assemble a complete package

The explicit build keeps the compatibility application available. The package recipe selects the genuine Audex target and all required companions.

```bash
cd codex-rs
cargo build --locked --offline \
  --target x86_64-unknown-linux-gnu --profile dev-small \
  --bin audex --bin codex --bin codex-code-mode-host \
  --bin codex-exec-server --bin bwrap
cd ..

export AUDEX_RG_BIN="$(command -v rg)"
export AUDEX_PACKAGE_STAGE="$HOME/scratch/codex/Audex/package-candidate"
test ! -e "$AUDEX_PACKAGE_STAGE"
python3 scripts/build_codex_package.py \
  --variant audex --target x86_64-unknown-linux-gnu \
  --cargo-profile dev-small --strip none \
  --rg-bin "$AUDEX_RG_BIN" --package-dir "$AUDEX_PACKAGE_STAGE"
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

Check the executable directly before installation:

```bash
"$AUDEX_PACKAGE_STAGE/bin/audex" --version
"$AUDEX_PACKAGE_STAGE/bin/audex" audit --help
"$AUDEX_PACKAGE_STAGE/bin/audex" completion bash
```

Help and completion do not commission inference. Set an explicit disposable `CODEX_HOME` when validating a candidate against test runtime state.

## Install and select the local backend

This section changes the installation, PATH and managed runtime. Apply it only when those changes are authorised. It is separate from building a candidate package.

Inspect existing command resolution first:

```bash
type -a audex
```

A shell function can shadow a real executable even after PATH changes. Retire an existing wrapper or function deliberately; keep its runtime-home selection and preserve existing audit data.

Install the full package at a stable location. The example keeps one previous package and refuses to overwrite an existing backup or staging directory.

```bash
export AUDEX_INSTALL_ROOT="$HOME/.local/share/audex"
mkdir -p "$AUDEX_INSTALL_ROOT"
test ! -e "$AUDEX_INSTALL_ROOT/package.new"
test ! -e "$AUDEX_INSTALL_ROOT/package.previous"
cp -a "$AUDEX_PACKAGE_STAGE" "$AUDEX_INSTALL_ROOT/package.new"
if [ -e "$AUDEX_INSTALL_ROOT/package" ]; then
    mv "$AUDEX_INSTALL_ROOT/package" "$AUDEX_INSTALL_ROOT/package.previous"
fi
mv "$AUDEX_INSTALL_ROOT/package.new" "$AUDEX_INSTALL_ROOT/package"
export PATH="$AUDEX_INSTALL_ROOT/package/bin:$PATH"
type -a audex
audex --version
```

Add that PATH directory to the normal shell startup configuration using `vi` if persistent shell configuration changes are authorised. Verify resolution in a fresh shell. The application itself remains a regular compiled file; no application symlink or shell launcher is required.

Without an explicit `CODEX_HOME`, the Audex build uses `$HOME/.chatgpt/Audex/codex-home`. An explicit `CODEX_HOME` overrides that default. The separate compatibility `codex` build keeps its existing home behaviour.

Start the established managed daemon and explicitly select this complete package:

```bash
audex app-server daemon start
audex app-server daemon update --from-cli
audex app-server daemon version
audex audit list
audex --remote unix://
```

Package replacement asks for confirmation and warns if running work may be interrupted. The existing daemon machinery copies and pins the local package. Its internal `bin/codex` file contains the compiled Audex application, preserving Audex home/update behaviour and package provenance. Internal managed selection links are separate from the operator-facing executable identity.

A matching CLI version alone does not prove that the daemon changed. Inspect the daemon version/selected package and verify audit capability through `audit list`. CLI audit commands use that dedicated managed runtime. The explicit remote TUI command connects to the same runtime.

## Update from a selected source revision

Build and validate a new complete candidate from the chosen source revision using the same explicit target, profile and verified inputs. Repeat the installation steps, preserving the prior package, then run:

```bash
audex app-server daemon update --from-cli
audex app-server daemon version
audex audit list
```

Reopen the TUI after the daemon replacement. Existing runtime data is preserved. Do not replace a package while an unreviewed or unwanted audit is still running.

`audex update` prints local-package update guidance and refuses upstream replacement. Audex also excludes public upstream update choices from its TUI and pins its daemon against automatic upstream updates. The separate Codex application retains its normal update behaviour.
