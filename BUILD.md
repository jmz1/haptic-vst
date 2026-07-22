# Building Haptic VST

Run commands from the workspace root. The repository uses a Cargo workspace
and a small `xtask` wrapper around `nih_plug_xtask` for VST3 bundles.

Testing and live-process sequencing are documented separately in
[`TESTING.md`](TESTING.md).

## Prerequisites

- A current Rust toolchain installed through `rustup`.
- `cargo` available on `PATH`. A normal rustup installation can be activated
  for the current shell with:

  ```bash
  source "$HOME/.cargo/env"
  ```

The plugin depends on the project's `jmz1/nih-plug` fork, so the first build
may need network access to fetch Git dependencies.

## Workspace checks

Use the narrowest command while iterating. Before handing off Rust changes, run:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
```

Strict linting for larger changes:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Build the VST3 bundle

Development bundle:

```bash
cargo xtask bundle haptic-plugin
```

Optimized release bundle:

```bash
cargo xtask bundle haptic-plugin --release
```

macOS universal bundle, when both Rust targets are installed:

```bash
cargo xtask bundle-universal haptic-plugin --release
```

Output:

```text
target/bundled/haptic-plugin.vst3
```

VST3 bundles are directories. The outer directory timestamp is not a reliable
build identity and a DAW may retain an already loaded library after rebuilding.
The editor and plugin log show:

```text
build <content hash> · protocol <version>
```

The deterministic hash covers the plugin and shared protocol source, workspace
manifests, and lockfile. Use it to distinguish dirty-worktree bundles and stale
DAW-loaded binaries.

CLAP export is currently disabled in `haptic-plugin/src/lib.rs`. A
`haptic-plugin.clap` left in `target/bundled/` is stale and is not a supported
output of the current workspace.

## Build individual applications

Server:

```bash
cargo build -p haptic-server --release
```

Interactive Haptic application and its server helper:

```bash
cargo build -p haptic-server -p haptic-viewer --release
```

Both executables land beside one another in `target/release/`. Launching
`haptic-viewer` then attaches to an existing server or starts the sibling
`haptic-server` automatically.

Standalone controller host:

```bash
cargo build -p haptic-plugin-standalone --release
```

Corresponding binaries are placed under `target/release/`.

For an ordinary interactive run after building both executables:

```bash
cargo run -p haptic-viewer --release
```

The application window is titled **Haptic** and owns any server it starts. To
run the engine independently or headlessly:

```bash
cargo run -p haptic-server --release
cargo run -p haptic-plugin-standalone --release
```

Use `haptic-viewer --connect-only` when a manually managed server must remain
independent. `--server-bin PATH` or `HAPTIC_SERVER_BIN` selects a helper that is
not beside the application. The DAW-free and headless workflows in `TESTING.md`
are usually faster than rebuilding and reloading a plugin host.

## Optional local installation on macOS

Bundling does not install the plugin. If DAW integration is the test target,
copy the completed bundle into the user's VST3 directory:

```bash
mkdir -p "$HOME/Library/Audio/Plug-Ins/VST3"
ditto target/bundled/haptic-plugin.vst3 \
  "$HOME/Library/Audio/Plug-Ins/VST3/haptic-plugin.vst3"
```

Installation is intentionally a separate manual action: ordinary builds and
automated tests should not modify a DAW directory or start a GUI host.

After replacement, some DAWs must be fully quit and restarted before they
unload the old dynamic library. Compare the build hash displayed by the plugin
with the newly built bundle rather than relying on file timestamps.

## Crate and feature details

`haptic-plugin` builds both:

- `cdylib`, used by the VST3 wrapper; and
- `rlib`, used by `haptic-plugin-standalone`.

Its `standalone` Cargo feature enables nih-plug's standalone support. Normal
plugin bundling selects the wrapper configuration through the xtask tooling;
the workspace does not expose separate user-facing VST3/CLAP feature flags.

The supported export is VST3. Host compatibility should be verified in the
actual target DAW rather than inferred from a generic format list.

## Troubleshooting builds

### Cargo is not found

Activate the rustup environment as shown under Prerequisites, or invoke Cargo
from the path reported by `rustup`.

### A build is unexpectedly stale

- Confirm the editor's content hash and protocol version.
- Build the release bundle again with `cargo xtask bundle haptic-plugin
  --release`.
- Replace the installed bundle rather than copying into an extra nested
  directory.
- Fully restart the host if it has already loaded the previous library.

### Universal build target is missing

Install the needed targets, for example:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

Then rerun the universal bundle command.

### A dependency or generated output is inconsistent

Start with non-destructive diagnostics:

```bash
cargo check --workspace
cargo tree -p haptic-plugin
```

Avoid using `cargo clean` as a routine fix; it removes the entire workspace
build cache and usually obscures the actual dependency or host-loading issue.
