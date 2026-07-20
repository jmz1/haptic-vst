# Haptic VST — Status Review & Roadmap

*Prepared 2026-07-20 as an implementation handoff brief. Supersedes the phase plans in `docs/planning/minimal-prototype-implementation-guide.md` for prioritisation purposes; that document remains the architectural reference. Historical planning documents live in `docs/planning/`.*

## Status update (2026-07-20, post-implementation)

- **Phase A — complete.** All of G1–G5 and velocity disentanglement implemented and unit/integration tested (13 tests). Exit criterion verified against the live server with a scripted MPE lifecycle (note on → bend/pressure/timbre sweeps → note off → panic) over the socket.
- **Phase B — complete except item 4.** Engine now owned by the audio callback (no locks on the audio path), `rtrb` SPSC command queue drained once per callback, `static mut` replaced with atomics, per-event success logging removed and MPE events unlogged. Health harness reports callback p50/p99/max and stream errors every 5 s; `--test-tone` implemented. 90 s stress at ~770 cmds/s: release build p50 ≈ 2.1 ms / max ≈ 4.5 ms against a ~10.7 ms budget, zero stream errors, zero queue overflows. Item 4 (per-block envelope/MPE processing) deferred — headroom makes it unnecessary for now.
- **Phase C — complete.** `TransducerLevels` (per-block RMS, 32 logical channels pre-truncation) broadcast at ~60 Hz over the framed protocol; plugin reader thread feeds the egui grid, which renders live levels. Verified end-to-end at 60 Hz.
- **Housekeeping:** dead `handle_command` removed (shared `From<HapticCommand>` conversion), `rtrb` adopted (crossbeam/parking_lot dropped from the server), stale `ARCHITECTURE.md` sections refreshed. CLAP export remains disabled in the working tree — decision still open.
- **Phase D — complete.** TOML transducer layout (`haptic.toml`, `--config <path>`): physical metres (required by the wave model), `[table]` dimensions, `[grid]` shorthand plus `[[transducer]]` per-channel position/gain overrides, per-transducer gains applied in the engine. Default layout is a cell-centred 4×8 grid over a nominal 1 m × 2 m table. Hot reload via a 1 Hz mtime watcher feeding a second SPSC ring into the audio callback; invalid edits are rejected and the running layout kept. Verified live: gain edit attenuated a held note ~50× within ~1.5 s and restored cleanly.
- **Delay-line overflow fixed.** The 1×2 m table exposed a latent bug: worst-case propagation delay (table diagonal at 20 m/s ≈ 112 ms) exceeded the 100 ms delay-line capacity, and the overflow path read the just-written sample (zero delay) instead. Buffers are now 16384 samples (~341 ms @ 48 kHz) with delays clamped, regression-tested. Default wave speed is now 20 m/s (engine and plugin parameter).
- **Phase visualiser (`haptic-viewer`) implemented.** Standalone eframe/egui binary attached to the server socket as a read-only status client; works identically with real or dummy audio output. Server broadcasts `ServerStatus::Layout` (on connect + hot reload) and `ServerStatus::VoiceState` (most recent active delay-line voice: frequency, wave speed, source position, amplitude, actual per-transducer delays) at up to 250 Hz. Viewer renders the configured layout as circles; hue = relative phase (zero → blue, OKLCH h≈264°), lightness/chroma = local amplitude with binary-search chroma clamp into the sRGB gamut (clip-free hue sweeps); MPE source shown as a cross; vsync-paced continuous repaint renders at display refresh (120 fps on ProMotion; fps counter on screen). Note: clients that never read status are dropped by design when their socket buffer fills.
- **Known follow-up:** MPE→source-position mapping is still hardcoded ±20 cm (pitch bend) / 0–20 cm (timbre) — disproportionate to the 1×2 m table; should scale from the configured table dimensions.
- **Next:** Phase E (stimulus research track: real standing-wave spatial structure, SpatialSweep, phasor-model experiment offline first).

## 1. Where this codebase sits among the planning documents

Three architectures were planned across the project's documents:

1. **Single-plugin VST** ("Haptic VST Implementation Plan") — everything in one plugin. *Not implemented.*
2. **Static Allocation Architecture** — rich stimulus engine design (ADSR, ParameterMapping, SpatialSweep, ChaoticNetwork, TransducerConfig with gain reduction). *Ancestor design; partially carried forward in simplified form.*
3. **Hybrid / Minimal Prototype** ("Haptic VST Minimal Prototype - Implementation Guide", now at `docs/planning/minimal-prototype-implementation-guide.md`) — VST controller plugin + standalone haptic server over a Unix socket. **This is what the repo implements.**

The implementation deliberately simplified the Static Allocation design when porting into the hybrid layout: ADSR became a fixed-time attack/sustain/release envelope, SpatialSweep and ChaoticNetwork were dropped in favour of a StandingWaveStimulus stub, and ParameterMapping and TransducerConfig were omitted.

### Deviations from the minimal prototype spec (deliberate, from git history)

- `SetWaveSpeed` command and plugin parameter removed; wave speed is now derived per-stimulus from note velocity (20–500 m/s over velocity 0–127).
- Delay buffers moved to heap (`Box<[f32; 4800]>`) to fix a server stack overflow.
- `rtrb` ring buffer replaced with `crossbeam_channel::unbounded` for the IPC→audio command path (rtrb remains an unused workspace dependency).
- nih-plug and egui-baseview are consumed from forks (`github.com/jmz1/nih-plug`) carrying a macOS baseview crash fix; a standalone build target (`haptic-plugin-standalone`) was added and works.
- CLAP export commented out (uncommitted working-tree change); VST3 only at present.
- Extensive `nih_log!` diagnostics added throughout the plugin; log defaults to `/Users/jmz/tmp/log/haptic-vst.log`.

## 2. Current feature set

**Working:**
- Cargo workspace: `haptic-protocol`, `haptic-server`, `haptic-plugin`, `haptic-plugin-standalone`, `xtask` (nih_plug_xtask bundling).
- Plugin: VST3 + standalone; forwards NoteOn / NoteOff / PolyPressure / MidiPitchBend to the server as bincode `HapticCommand`s via a dedicated IPC worker thread; egui GUI with connection status and a static 4×8 transducer grid.
- Server: cpal output with 32-channel device discovery and stereo fallback; Unix-socket listener with multi-client support; stimulus engine with WaveStimulus pool (8) and StandingWaveStimulus pool (4); velocity-based routing (< 64 → wave, ≥ 64 → standing); delay-line wave propagation with fractional (linear-interpolated) delays, distance attenuation and output clamping; Ctrl-C shutdown.

**Notable emergent property:** delay distances are recomputed every sample from the MPE-driven source position, so a moving source already produces genuine Doppler shift through the delay lines. No explicit Doppler code is needed — this should be preserved and validated, not reimplemented.

## 3. Gap analysis

### Blocking playability
| # | Issue | Location |
|---|-------|----------|
| G1 | **Note-off unimplemented.** No note→stimulus mapping; `EngineCommand::NoteOff` is a TODO. Stimuli sustain until pool exhaustion or Panic. | `haptic-server/src/engine.rs` |
| G2 | **MPE updates dropped by server.** `EngineCommand::MpeUpdate` falls into the `_ => {}` arm; `mpe_update()` on stimuli is never called. | `engine.rs` `process()` |
| G3 | **Plugin has no per-channel MPE state.** Each event overwrites the other MPE dimensions with defaults (a pitch-bend update sends `pressure: 1.0`, `timbre: 0.5`). Needs a per-channel `MpeData` cache merged into each outgoing update. | `haptic-plugin/src/lib.rs` |
| G4 | **No IPC message framing.** Server does one `read()` → one `deserialize`. Coalesced or fragmented messages are corrupted or silently dropped — will bite as soon as MPE update rates rise. Needs length-prefixed framing on both ends. | `haptic-server/src/ipc.rs`, `haptic-plugin/src/ipc_client.rs` |
| G5 | **Frequency mapping is audio-centric.** MIDI 69 → 440 Hz; only notes ≈ E0–G3 land in the transducers' 20–200 Hz band. Needs a deliberate note→frequency map for the haptic range (design decision: transpose, compress, or table-driven). | `engine.rs` `note_on()` |

### Real-time hygiene
| # | Issue | Location |
|---|-------|----------|
| R1 | Engine wrapped in `Arc<Mutex>` with `try_lock` **per sample** in the audio callback; contention yields silent samples, and locking per sample is needless overhead. Restructure to lock once per callback (or eliminate the mutex entirely by owning the engine in the callback and feeding it via a lock-free queue). | `haptic-server/src/audio.rs` |
| R2 | Command queue is `crossbeam_channel::unbounded` (heap-allocating sends, unbounded growth). Restore the planned `rtrb` SPSC ring buffer (already a workspace dep). | `engine.rs` |
| R3 | Command draining happens inside per-sample `process()`; move to once per callback block. | `engine.rs` / `audio.rs` |
| R4 | `static mut LOG_COUNTER` in plugin `process()` — unsound pattern; replace with `AtomicU32` or remove with the debug logging. | `haptic-plugin/src/lib.rs` |
| R5 | Hot-path `nih_log!` calls on every MIDI event (and per-event socket sends) should be feature-gated or trace-level before performance work is meaningful. | `haptic-plugin/src/lib.rs` |
| R6 | IPC listener polls with 1 ms sleep; timestamps are sent but never used for scheduling. Acceptable short-term; note as a latency floor. | `ipc.rs` |

### Missing planned subsystems
- **Status feedback (server→plugin):** `ServerStatus::TransducerLevels` and `PerformanceMetrics` exist in the protocol but are never sent or read; GUI grid is a placeholder.
- **Configuration:** transducer layout hardcoded (4×8 grid, 5 cm pitch → 35 × 15 cm, far smaller than a body-sized table); no TOML config, gains/calibration, or hot reload.
- **Envelope:** fixed 100 ms attack / 500 ms release; no ADSR, no velocity or MPE shaping.
- **StandingWaveStimulus** outputs all transducers in phase — a placeholder, not a standing wave (no spatial node/antinode structure).
- **Stimulus research track** (project outline): phasor-representation wave model, SpatialSweep, ChaoticNetwork — none started; the outline names "what wave model is computationally practical in real time" as the top research priority.
- Docs partially stale: `ARCHITECTURE.md` still shows `MidiConfig` and pool details from earlier revisions; `BUILD.md`/`RUST_BUILD_GUIDE.md` should be re-verified after the next dependency touch.

## 4. Prioritised work plan

Context for prioritisation: a multichannel interface is available but the table is not yet built, so validation is via metering, recording and visualisation rather than skin. Chosen focus: **playable end-to-end first, then real-time robustness.**

### Phase A — Playable end-to-end
1. **Note lifecycle (G1).** Add a `(channel, note) → (pool, slot)` map in the engine (fixed-size array, no allocation). Route NoteOff to the owning stimulus's `note_off()`; handle voice stealing when pools are full (steal oldest-in-release, else oldest).
2. **MPE state tracking in plugin (G3).** Per-channel `MpeData` cache (16 entries); merge each incoming dimension and send the merged struct. Include initial pitch-bend/timbre state at NoteOn.
3. **MPE routing in server (G2).** Deliver `MpeUpdate` to stimuli owned by that channel via the note map from step 1. Apply simple one-pole smoothing to decoded MPE values in the stimulus (the existing `JMZTODO` in `engine.rs`).
4. **IPC framing (G4).** Length-prefixed (u32 LE) bincode frames both directions; accumulating read buffer on the server; bounded write with disconnect detection on the client.
5. **Haptic frequency mapping (G5).** Map the playable MIDI range onto 20–200 Hz. Suggested default: keep equal-temperament ratios but transpose so middle C ≈ 65 Hz, with min/max clamps; make the mapping a server-side function that a config value can later select.
6. **Disentangle velocity overloading.** Velocity currently sets amplitude *and* wave speed *and* stimulus type. Keep velocity→amplitude; move stimulus-type selection and wave speed to plugin parameters (restores DAW automation, which the current design lost when `SetWaveSpeed` was removed). Requires a `SetParameter`-style command in the protocol.

*Exit criterion: a held MPE note can be started, bent, pressed, moved and released, with correct per-note behaviour, verified by recording the 32-channel output (or metering) — no hangs, no stuck notes, no dropped MPE.*

### Phase B — Real-time robustness
1. Own the engine inside the audio callback; replace `Arc<Mutex>` + per-sample `try_lock` with an `rtrb` SPSC command queue drained once per callback (R1–R3).
2. Replace `static mut` counter with atomics; gate hot-path logging behind a feature flag or `nih_trace!` (R4, R5).
3. Add a simple latency/health harness: server prints buffer underruns and callback timing percentiles; a `--test-tone` mode that plays a known pattern across all 32 channels for interface bring-up (valuable with interface-only hardware).
4. Process in blocks rather than per-sample where possible (envelope and MPE smoothing per block, oscillator phase per sample).

*Exit criterion: no allocation, locking, or logging in the audio callback; clean 30-minute run at 48 kHz / 64-sample buffers with zero underruns while spamming MPE.*

### Phase C — Feedback & visualisation (next after A/B)
- Implement `ServerStatus::TransducerLevels` broadcast (~60 Hz, decimated RMS per transducer) using the framing from A4; render live levels in the egui grid. This becomes the primary validation instrument until the table exists.

### Phase D — Configuration
- TOML transducer layout + per-transducer gain (the Static Allocation design's `TransducerConfig` is the model), hot reload via a config-swap command, realistic default layout sized for the actual table.

### Phase E — Stimulus research track
- Proper standing-wave stimulus (spatial phase structure), SpatialSweep with path definition, and the phasor-representation wave model experiment from the project outline. Recommend prototyping the phasor model offline (plot/render fields) before committing it to the real-time engine.

## 5. Housekeeping
- Decide the fate of the uncommitted change disabling CLAP export (commit it with rationale, or restore CLAP).
- Remove or `#[cfg]`-gate dead code paths (`StimulusEngine::handle_command`, unused `rtrb` dep until Phase B re-adopts it).
- Refresh `ARCHITECTURE.md` after Phase A (note map, framing, parameter flow) and keep `docs/planning/minimal-prototype-implementation-guide.md` as the historical handoff.
