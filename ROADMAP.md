# Haptic VST — Status Review & Roadmap

*Prepared 2026-07-20 as an implementation handoff brief. Supersedes the phase plans in `docs/planning/minimal-prototype-implementation-guide.md` for prioritisation purposes; that document remains the architectural reference. Historical planning documents live in `docs/planning/`.*

## Status update (2026-07-20, post-implementation)

- **Phase A — complete.** All of G1–G5 and velocity disentanglement implemented and unit/integration tested (13 tests). Exit criterion verified against the live server with a scripted MPE lifecycle (note on → bend/pressure/timbre sweeps → note off → panic) over the socket.
- **Phase B — complete except item 4.** Engine now owned by the audio callback (no locks on the audio path), `rtrb` SPSC command queue drained once per callback, `static mut` replaced with atomics, per-event success logging removed and MPE events unlogged. Health harness reports callback p50/p99/max and stream errors every 5 s; `--test-tone` implemented. 90 s stress at ~770 cmds/s: release build p50 ≈ 2.1 ms / max ≈ 4.5 ms against a ~10.7 ms budget, zero stream errors, zero queue overflows. Item 4 (per-block envelope/MPE processing) deferred — headroom makes it unnecessary for now.
- **Phase C — complete.** `TransducerLevels` (per-block RMS, 32 logical channels pre-truncation) broadcast at ~60 Hz over the framed protocol; plugin reader thread feeds the egui grid, which renders live levels. Verified end-to-end at 60 Hz.
- **Housekeeping:** dead `handle_command` removed (shared `From<HapticCommand>` conversion), `rtrb` adopted (crossbeam/parking_lot dropped from the server), stale `ARCHITECTURE.md` sections refreshed. CLAP export remains disabled in the working tree — decision still open.
- **Phase D — complete.** TOML transducer layout (`haptic.toml`, `--config <path>`): physical metres (required by the wave model), `[table]` dimensions, `[grid]` shorthand plus `[[transducer]]` per-channel position/gain overrides, per-transducer gains applied in the engine. Default layout is a cell-centred 4×8 grid over a nominal 1 m × 2 m table. Hot reload via a 1 Hz mtime watcher feeding a second SPSC ring into the audio callback; invalid edits are rejected and the running layout kept. Verified live: gain edit attenuated a held note ~50× within ~1.5 s and restored cleanly.
- **Delay-line overflow fixed.** The 1×2 m table exposed a latent bug: worst-case propagation delay (table diagonal at 20 m/s ≈ 112 ms) exceeded the 100 ms delay-line capacity, and the overflow path read the just-written sample (zero delay) instead. Buffers are now 16384 samples (~341 ms @ 48 kHz) with delays clamped, regression-tested. Default wave speed is now 20 m/s (engine and plugin parameter).
- **Phase visualiser (`haptic-viewer`) implemented.** Standalone eframe/egui binary attached to the server socket as a status client (originally read-only; the test console below made it a full read/write client); works identically with real or dummy audio output. Server broadcasts `ServerStatus::Layout` (on connect + hot reload) and `ServerStatus::VoiceState` (most recent active delay-line voice: frequency, wave speed, source position, amplitude, actual per-transducer delays) at up to 250 Hz. Viewer renders the configured layout as circles; hue = relative phase (zero → blue, OKLCH h≈264°), lightness/chroma = local amplitude with binary-search chroma clamp into the sRGB gamut (clip-free hue sweeps); MPE source shown as a cross; vsync-paced continuous repaint renders at display refresh (120 fps on ProMotion; fps counter on screen). Note: clients that never read status are dropped by design when their socket buffer fills.
- **Viewer test console + monitor routing.** MPE→source-position mapping now spans the configured table (bend −1..1 → x across width, timbre 0..1 → y along length; centre at bend 0/timbre 0.5) — resolves the earlier disproportion follow-up. The viewer starts/stops a test note (channel 15) with note/velocity/wave-speed sliders (retrigger on release), drag-on-table source placement, and an orbit mode. New `Parameter::MonitorRoute` connects any physical device output to any logical channel (engine applies routing when copying to the device; logical levels/phase stay pre-routing, so all 32 cells visualise even on stereo); left/right-click a cell routes it to output L/R, with badges showing current routing, and `ServerStatus::MonitorRouting` (device channel count + routes) broadcast on connect/change keeps all clients in sync.
- **Source velocity limit + dual cursors.** The effective source position now chases the MPE-requested position at no more than a fixed fraction of the stimulus's wave speed (`SOURCE_SPEED_FRACTION = 0.8` in `engine.rs`), so the source can never outrun its own waves (an instant MPE jump previously moved delays faster than one sample per sample, reading the delay lines backwards / collapsing the Doppler model). Position snaps at note-on, then advances toward the request at up to 0.8·c; regression-tested. `VoiceState` gained `requested_pos` (protocol change — rebuild server, viewer, and plugin together); the viewer draws a ring at the requested position, a cross at the effective source, and a tether while it catches up.
- **Orbit pitch-discontinuity + distortion debugged and fixed (buffer-capture verified).** Captures over two orbit periods (headless harness: `orbit_capture_writes_debug_buffers` in `engine.rs`, drives `process_block` with the viewer's exact orbit command stream against a dummy 32-ch sink) showed (a) pitch steps at the orbit period wherever a transducer's propagation delay crossed the 341 ms delay-line capacity clamp — Doppler vanishes while clamped — and (b) an FM sideband comb at the audio-block/MPE-send rate from the stepped position targets. Fixes: the engine now renders at `device_rate / RENDER_DECIMATION` (48 kHz → 1.5 kHz; Nyquist 750 Hz ≫ the 200 Hz band) with per-frame linear upsampling to the device rate, stretching delay capacity to ~10.9 s so the clamp is unreachable for realistic layouts (wave-speed floor lowered to 0.25 m/s, viewer slider aligned); and MPE targets are linearly ramped over their measured arrival spacing then smoothed by two cascaded 15 ms one-poles, making trajectory smoothness independent of client send cadence. Verified: c=1 m/s pitch-jump events 395→1, in-band junk −41.6→−53 dB; c=10 clean to −84 dB; per-sample delay-line cost also dropped 32×.
- **Polyphase sinc reconstruction filter.** The linear-interp upsampler's images (~−50 dB above 1 kHz) were audible on monitors; replaced with a 512-tap Kaiser-windowed sinc (β=10, 16 taps/branch, cutoff at the internal Nyquist) evaluated polyphase — one filter serves as both interpolator and heavy reconstruction lowpass, unity DC gain per branch, ~5.3 ms group delay. Capture-verified: images fell −50 → −107 dB (f32 noise floor); in-band Doppler/junk metrics unchanged. Design-sanity unit test checks per-branch DC gain and >90 dB first-image rejection.
- **Delay line switched to scatter-write / sequential-read (2026-07-21).** The delay line was fixed-write / interpolated-read — physically the moving-*listener* model (delay evaluated at reception time), and the reason the source needed a velocity cap to stop the read tap overtaking the write head. For a moving source and fixed listener the correct arrangement is **interpolating writes, fixed reads**: each emitted sample is scattered (2-tap linear, accumulating) into its emission-time arrival slot `n + τᵢ(n)` and a sequential read pointer consumes arrivals. This removes the read-backwards failure mode outright and adds the physically correct Doppler *amplitude* gain (bunched arrivals on approach) the old single-tap read couldn't produce. `SOURCE_SPEED_FRACTION` lowered 0.8 → 0.5 so the scatter arrival index stays monotonic and gap-free (`da/dn ∈ [0.5, 1.5]`) with no special-casing. Distance attenuation moved to the write. Verified: all engine tests pass, a new `scatter_delay_line_shifts_pitch_with_source_motion` unit test pins Doppler direction + amplitude gain, and the c=1 orbit capture shows a clean ~6 s periodic pitch *and* amplitude swing (RMS ~2.4×), bounded output, zero non-finite samples. Design doc §3–§6 updated. Two-rate render, polyphase sinc, MPE smoothing, snapshots, viewer untouched.
- **Scatter deposit made bandlimited; output headroom trimmed (2026-07-21).** Live testing of the scatter-write line at low wave speed (c=2 m/s, 4 s orbit) surfaced two artifacts, both diagnosed from `orbit_capture` samples: (1) **warble** — the 2-tap linear deposit's gain varies with the fractional arrival phase, so a sweeping delay amplitude-modulates the output into granulation spurs (~−20 dB); (2) **popping** — the (real) Doppler bunching gain stacking on the near-field `1/(1+2d)` gain railed the ±1 clamp on close passes. Fixes: the deposit is now a **windowed-sinc splat** (`SPLAT_TAPS=8`, Kaiser β=9, 128 phases, unit-sum per phase — flat in-band gain < 0.1 dB across phases), cutting granulation spurs ~20 dB in both Doppler directions (isolated A/B: spreading −20→−41 dB, bunching −35→−56 dB; stationary at the −138 dB f32 floor) while keeping the full Doppler amplitude behaviour; and the default per-transducer gain is trimmed to 0.5 (`DEFAULT_TRANSDUCER_GAIN`, −6 dB), sized for the worst-case 2× bunching of an advancing source at the 0.5·c limit. Re-capture: peak 1.03→0.61, railed samples 1586→0. New unit test `splat_kernel_has_unit_dc_and_flat_in_band_gain_across_phases`; config default-gain tests updated. Design doc §3/§6 updated.
- **Design docs started (2026-07-20).** `docs/doppler-delay-line-design.md` documents the delay-line Doppler source (emergent-Doppler model, stability layers, two-rate architecture, failure-mode history). `docs/syllabary-protocol.md` is a **draft** of the expressive note-type protocol on top of MPE (syllable vocabulary, four-layer control model bounded by Push 3 / Live 12 MPE, per-instance syllable selection, handshake + namespaced-parameter migration plan, configurable attenuation `d0`/`p` as the first namespaced params). Nothing from the draft is implemented; its migration step 1 (handshake + `instance_id`) also fixes a real latent bug — voice identity `(channel, note)` collides across concurrent plugin instances.
- **Next:** the client/server ownership rework below (in progress, started 2026-07-21) — this *is* syllabary migration step 1, brought forward. Then Phase E (stimulus research track: real standing-wave spatial structure, SpatialSweep, phasor-model experiment offline first).

## Architecture direction: multi-instance controllers, per-instance note-type config, observer viewer (design intent, 2026-07-21)

*Captured as the handoff record for the rework begun this session. This is syllabary migration step 1 made concrete.*

**The problem (diagnosed from the code, not just symptoms).** The parameter channel is **write-only and unsynchronised**: `Parameter::WaveSpeed` / `StimulusType` / `MonitorRoute` travel only client→server, and no `ServerStatus` echoes current parameter state back. The server holds one **global** `wave_speed` / `stimulus_type`, applied to new notes at note-on. Both the plugin and the viewer keep their *own* local copy and push to that global, so it is **last-writer-wins** with no reconciliation — the two on-screen values (plugin 20 m/s, viewer 0.31 m/s in the 2026-07-21 screenshot) are disconnected local echoes, neither of which is server truth. Secondary bug: the plugin `wave_speed` FloatParam range is **20–500 m/s**, so the entire interesting low-c Doppler regime is unreachable from the host.

**The principle.** Split control by intent: **MIDI/MPE = performance gesture** (which note, how hard, expressive modulation) vs **VST host parameters = configuration** (what *kind* of note this instance emits). Wave speed is config, not gesture — which is why deriving it from note velocity (an earlier, since-removed deviation) was a category error. This rule sorts any future control.

**The target architecture.** Split by role; the two features below turn out orthogonal once the roles are clean, sharing exactly one enabling primitive — **identity (`instance_id` + note-type) on both commands and status**.

- **Plugin = controller/client only.** Owns and displays *its own outgoing configuration* (note type + that type's parameters). The in-plugin RMS grid pseudo-visualiser is **deprecated** — the plugin cannot honestly show system state, so it drops its `TransducerLevels`/`Layout` consumption and becomes (near) pure write-side, plus a connection indicator.
- **Viewer = observer, 1:1 with the server.** The single renderer of *system* truth: all active voices, the summed field, routing, layout. Gains **filters** — which instance(s)/note-type(s)/voice(s), or the sum.
- **Per-instance config replaces the global.** Each client registers an `instance_id` + `InstanceConfig` via a **handshake** on connect; notes from that instance inherit *that instance's* config at note-on. No shared global ⇒ no last-writer-wins contention (instance A's notes use A's config, B's use B's). Wave speed is already captured per-*stimulus* at note-on; it just needs to read the owning instance's config instead of a global.
- **Voice identity becomes `(instance_id, channel, note)`**, fixing the latent cross-instance `(channel, note)` collision.
- **Squaring multi-instance with the viewer:** many controllers fan *in* to one server; one observer watches the *whole* server (per-server, not per-instance). The only coupling is that the status stream must carry `instance_id`/note-type so the viewer's filters have something to filter on — the same identity primitive the multi-instance work introduces.

**Implementation plan (milestones, committed independently).**
1. **Plugin wave-speed range fix** — match the engine (0.25–1000 m/s, skewed so sub-20 is reachable). Isolated bug, unblocks low-c testing. ✅ *(done)*
2. **Protocol** (`haptic-protocol`) — `HapticCommand::Hello { instance_id, role, config }`; `InstanceConfig { stimulus_type, wave_speed }`; `ClientRole { Controller, Observer }` (gates status delivery); replaced the single-voice `VoiceState` with a multi-voice `ActiveVoices` carrying per-voice `VoiceInfo` (`instance_id` + `note_type`, compact — the viewer recomputes phase/amplitude geometrically, so the 32-float delay array is dropped). `MonitorRoute` stays server-global. ✅ *(done — Hello/ActiveVoices roundtrip + frame-budget tests)*
3. **Server** (`haptic-server`) — per-connection `instance_id` bound on `Hello` and stamped onto every command (`EngineCommand::from_wire`); fixed-capacity instance→config registry in the engine (linear scan, no audio-thread alloc); per-instance config applied at note-on; `(instance_id, channel, note)` voice identity; all active voices broadcast tagged with identity; status delivery gated by role (observers greeted + streamed, controllers get nothing so a pure write-side plugin is never dropped for an unread socket). ✅ *(done — multi-instance isolation + socket handshake/stamping/gating tests)*
4. **Plugin client-mode** — sends `Hello` (Controller) + config on connect; IPC client is now write-only (reader thread + levels removed); editor shows only connection status + this instance's note-type config (RMS grid deleted). ✅ *(done)*
5. **Viewer** — sends `Hello` (Observer); consumes `ActiveVoices`; renders the summed complex field across voices (interference-correct) and per-voice cursors; a filter picker narrows to a single instance or sums all; its test console is its own instance, so its wave-speed slider no longer fights the plugin. ✅ *(done)*

Notes: coordinated protocol change — server, viewer and plugin rebuilt together. Interim server→client parameter echo deliberately skipped (per-instance config supersedes it). "Note type" is, for now, the `InstanceConfig` bundle (stimulus type + params) tagged by `instance_id`; note-type *filtering* is plumbed (`VoiceInfo.note_type`) but degenerate while all wave voices are `Wave` — it becomes meaningful as the syllabary vocabulary lands on the same identity primitive. Still open: the inverted `requested` vertical axis (trivial timbre→y flip); CLAP export; a human-readable instance label in the viewer's filter (currently a short id hash).

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
