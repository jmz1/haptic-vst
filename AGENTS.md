# Haptic VST repository guide

This repository implements a 32-channel haptic stimulus system: a VST3
controller plugin (`nih-plug` + `egui`) and a standalone real-time server
(`cpal`) connected over a Unix socket. The intended stimulus range is
20–200 Hz, with spatial wave simulation for a vibratory table.

## Start here

- Use `README.md` as the project and documentation map. Read `ROADMAP.md` at
  the start of a session; it is the source of truth for current state,
  priorities, and handoff notes.
- Use `ARCHITECTURE.md` for the system and Rust implementation reference.
- Use `BUILD.md` for build details and `TESTING.md` for live and automated test
  workflows.
- Treat `docs/planning/` as frozen architectural history. Do not edit it.

## Workspace map

- `haptic-protocol`: shared commands, status messages, and frame codec.
- `haptic-server`: real-time engine, audio-device output, configuration, and
  Unix-socket server.
- `haptic-plugin`: VST3 controller and plugin UI.
- `haptic-plugin-standalone`: standalone host for the plugin UI/controller.
- `haptic-viewer`: primary interactive application; phase visualizer, test
  console, and managed-server supervisor.
- `xtask`: plugin bundling commands.
- `haptic.toml`: default table layout and transducer configuration.

## Working conventions

- Preserve real-time audio constraints. Do not add allocation, blocking I/O,
  locks, or routine logging to the audio callback or per-sample paths.
- Keep protocol changes synchronized across every producer and consumer:
  protocol crate, server, plugin, viewer, scripts, and tests as applicable.
- Preserve framed IPC (`u32` little-endian length plus bincode payload).
  Clients must continue reading server status broadcasts while connected.
- Keep the engine's 32 logical channels independent of the physical output
  count; monitor routing performs the final device-channel mapping.
- Update tests and the relevant current documentation when behavior,
  commands, protocol, or architecture changes.
- When a roadmap item is completed, abandoned, or reprioritized, update
  `ROADMAP.md` in the same session. Do not rewrite historical planning docs.
- Do not process `TODO` or `JMZTODO` comments unless the user explicitly asks.
- Create focused, descriptive commits at coherent milestones without waiting
  for the user to ask. Commit often enough that completed work is recoverable,
  but never include unrelated working-tree changes. Do not push unless the user
  asks.

## Build and verification

Run the narrowest relevant checks during iteration. Before handing off a Rust
change, run these when the environment supports them:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
```

For real-time or audio-path changes, also follow the targeted live or capture
checks in `TESTING.md`. For a release plugin bundle, run:

```bash
cargo xtask bundle haptic-plugin --release
```

Do not install the bundle into a DAW directory, start GUI applications, or
exercise physical audio hardware unless the user asks.
