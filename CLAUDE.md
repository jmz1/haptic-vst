# Haptic VST — project guide

32-channel haptic stimulus system: VST3 controller plugin (nih-plug + egui) and a standalone real-time server (cpal) communicating over a Unix socket. Research context: vibratory stimulus table, 20–200 Hz, spatial wave simulation.

## Where things live

- **Current state & priorities**: `ROADMAP.md` — the single source of truth for what is done, in progress, and next. Read it at the start of any session.
- **Planning & architecture history**: `docs/planning/` (see its README for reading order). Frozen reference; do not edit.
- **Architecture / Rust reference**: `ARCHITECTURE.md`. Build instructions: `BUILD.md`, `RUST_BUILD_GUIDE.md`. Running & testing the system: `TESTING.md`.

## Session conventions

- When completing, abandoning, or reprioritising roadmap items, update `ROADMAP.md` status in the same session — it is the handoff medium between coding sessions (Claude Code) and planning/review sessions (Cowork).
- Prefer committing at coherent milestones with descriptive messages; git history doubles as the project log.

## Code Handling Guidelines

- Ignore TODO comments unless asked to process them
- always ignore JMZTODO comments
