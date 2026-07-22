# Haptic VST Roadmap

This is the central planning and implementation-handoff document. It describes
the system we have, the work that is active or plausibly next, and the research
questions still worth exploring. Detailed current behaviour belongs in
`ARCHITECTURE.md` and the feature documents under `docs/`.

When work is completed, remove its issue narrative from this file. Fold any new
capability into the current baseline, move reusable technical learning to the
relevant design document or `docs/learnings.md`, and leave the execution history
to tests and Git.

## Project direction

The project is a compositional instrument for spatial vibration across a table
of up to 32 transducers. MIDI and MPE provide note and per-note expression;
stable VST parameters provide track-level stimulus configuration and DAW
automation. The server translates those controls into a 32-channel physical
field in the 20–200 Hz range.

The foundational idea is to place each transducer at a coordinate in an
abstract space and sample a stimulus at those coordinates. The space may be a
physical table, a more abstract topology, or eventually a mapped body plan.
Wave-like models are compelling because motion, wavelength, interference, and
propagation become perceptible and composable relationships rather than merely
independent channel levels.

Five principles guide development:

1. **Composition first.** A useful control must be recordable, editable, or
   automatable in the Ableton/Push workflow unless it is purely operational.
2. **Gesture and configuration are different layers.** MIDI/MPE carries note
   performance; each plugin instance owns the parameters describing the kind of
   stimulus it emits.
3. **The server owns system truth.** It owns voices, layout, routing, rendering,
   and hardware. The plugin is a controller; the viewer is the whole-system
   observer.
4. **Real-time behaviour is designed, not hoped for.** Callback work must be
   allocation-free, lock-free, nonblocking, and bounded.
5. **Models should remain inspectable.** Mathematical meaning, viewer behaviour,
   offline tests, and captured output should agree closely enough to make
   experimentation intelligible.

## Current baseline

### Runtime architecture

- A VST3 controller plugin, real-time server process, and observer/test GUI
  communicate over versioned, length-prefixed bincode frames on a Unix socket.
- The GUI is the primary interactive application: it attaches to an existing
  server or starts a managed server automatically. The process boundary remains
  for fault isolation, but ordinary use requires only one launch.
- Each connection begins with `Hello` and is accepted only after exact protocol
  version, role, identity, and configuration validation. A controller reports a
  connection only after `HelloAccepted`.
- Configuration and voice identity are per instance. Concurrent DAW tracks do
  not share stimulus type or wave parameters, and voices are keyed by
  `(instance_id, channel, note)`.
- Disconnect, voice stealing, note-off, and panic all enter bounded lifecycle
  paths intended to prevent indefinitely sustained voices.
- The server audio callback owns the engine. Commands, layout updates, levels,
  and voice snapshots cross thread boundaries through fixed-capacity rings or
  bounded queues.

### Sound and control model

- The engine has eight fixed Wave voices and eight fixed Travelling Wave voices.
- **Wave** is a moving-source propagation model using per-transducer
  scatter-write delay lines. It retains in-flight energy, produces Doppler
  frequency and amplitude behaviour, and limits source motion to half the wave
  speed to preserve monotonic arrivals.
- **Travelling Wave (TW)** is an instantaneous radial phasor. It shares source
  position, envelope, pressure, and distance decay with Wave, but has no delay
  history, Doppler, source-speed cap, or propagation tail. Its spatial scale can
  be expressed as speed or fixed wavelength and automated live.
- MIDI frequencies use standard equal temperament with no transposition, then
  clamp to the 20–200 Hz haptic band. Note names use Ableton's octave
  convention; the default test note is MIDI 36 / C1 / 65.4 Hz.
- Pitch bend maps to source x, CC74/timbre to source y, pressure to intensity,
  and strike velocity to amplitude. Stimulus type, scale, and distance decay
  are stable DAW parameters.

### Rendering, observation, and operation

- The engine always renders 32 logical transducer channels. Monitor routing
  maps physical device outputs to those logical channels only at the final
  hardware copy.
- Physical devices prefer a supported 48 kHz `f32` configuration. If no
  multichannel device exists, the server deliberately falls back to the default
  device while retaining all 32 logical channels.
- The delay engine renders at one thirty-second of the device rate and uses a
  polyphase sinc reconstruction filter. Wave emissions use a bandlimited sinc
  scatter kernel and generation-based delay-line clearing.
- Layout and per-transducer gains come from `haptic.toml` and hot-reload off the
  audio thread. Invalid updates leave the accepted layout running.
- The viewer displays all active Wave and TW voices, summed or filtered by
  instance. Its spatial field is a phase-aligned geometric preview, not an exact
  reconstruction of synchronized oscillator phase and delay-line history.
- Headless mode runs the complete engine against a paced 48 kHz, 32-channel
  memory sink. It uses an isolated per-process socket by default and does not
  contend with the production server or open audio hardware.
- A managed server receives an inherited stdin lifetime pipe. Closing or
  crashing the GUI closes that pipe and makes the owned server shut down through
  its normal lifecycle; an externally launched server remains independent.
- Automated tests cover protocol framing and handshakes, connection lifecycle,
  voice ownership, Wave/TW signal models, routing, layout, reconstruction, and
  real Unix-socket integration. An opt-in orbit capture provides deeper DSP
  evidence when propagation behaviour changes.

## Active priorities

These priorities prepare the existing system for broader stimulus research
without weakening the real-time or protocol foundations. They are ordered by
the amount of risk they remove from subsequent work, but can be advanced in
small independent slices.

### 1. Make the server easier to extend

`haptic-server/src/engine.rs` currently owns voice lifecycle, instance
configuration, Wave and TW implementation, delay DSP, reconstruction, routing,
snapshot construction, and much of their test suite. The fixed-capacity design
is appropriate, but the concentration of responsibilities makes every new
stimulus more difficult to review and increases the chance of missing one
lifecycle path.

Work toward modules for:

- voice ownership, allocation, stealing, release, and disconnect cleanup;
- reusable envelope and controller smoothing;
- Wave and TW stimulus implementations;
- delay-line and scatter-kernel DSP;
- device-rate reconstruction and output safety;
- monitor routing; and
- observer snapshots.

The split must be behavioural: do not introduce trait objects, dynamic
allocation, shared locks, or cross-module abstractions that obscure callback
cost. Move one coherent subsystem at a time while its existing tests remain
green.

**Next move:** extract the delay-line/scatter kernel and reconstruction code
with no numerical changes, then establish narrow module-level tests around the
extracted interfaces.

**Done when:** adding a fixed-capacity stimulus does not require modifying
unrelated delay, routing, or reconstruction internals, and every lifecycle path
has one obvious owner.

### 2. Make callback safety measurable

The callback has been manually hardened, but its allocation-free property is
still an architectural claim rather than a continuously checked invariant.
The most important cases are not only steady rendering: allocation, voice
stealing, panic, reconnect configuration, layout replacement, and delayed-tail
cleanup must also remain bounded.

Add a focused harness capable of detecting allocation and deallocation while
exercising both server and plugin callback-critical paths. Keep timing evidence
separate from allocation evidence: callback histograms describe latency on a
given machine, while an allocator guard should provide a deterministic failure
when forbidden memory activity occurs.

**Next move:** choose a test-only allocator instrumentation strategy and pin
server note bursts, voice stealing, panic, and layout application before
covering the plugin callback.

**Done when:** the important callback transitions fail a test if they allocate,
deallocate, lock, block, format, or log, and the ordinary 64-frame processing
case remains covered.

### 3. Bound and simplify transport work

Framing, handshake validation, partial nonblocking writes, and disconnect
cleanup are in place. Read-side work still needs an explicit per-client budget:
one busy sender should not monopolise an IPC iteration or delay status delivery
to other clients. Buffer handling should also avoid repeated front-of-vector
drains as traffic grows.

The intended change is operational rather than a transport rewrite:

- cap bytes, frames, or commands decoded per client per iteration;
- retain partial frames across iterations;
- use read and compaction cursors instead of frequent prefix removal;
- preserve lifecycle capacity under parameter or MPE floods; and
- add fairness and fragmented/coalesced-frame tests.

**Next move:** measure the current loop structure, define a conservative decode
budget, and add a two-client test in which a flooding controller cannot starve
an observer or another controller.

**Done when:** per-client read, decode, and write work is explicitly bounded,
partial frames remain correct, and one client cannot starve cleanup or status
publication.

### 4. Publish accepted configuration state

Layout updates currently travel to the engine and observer side through
separate bounded paths. Monitor routing and layout displays should derive from
state the engine has actually accepted, not from an optimistic IPC-thread
mirror. This matters increasingly once configuration becomes richer or updates
can be rejected.

Replace split best-effort publication with a small versioned accepted-state
snapshot or acknowledgement path. Preserve off-callback parsing and avoid
deallocating replaced state on the audio thread.

**Next move:** document the ownership and failure semantics of the existing
layout and routing paths, then design the smallest versioned state publication
that covers both.

**Done when:** the viewer cannot display a layout or route the engine rejected,
and dropped intermediate updates converge to the latest accepted state.

## Next horizon

### Stable protocol identifiers and capability negotiation

Exact protocol version matching prevents silent bincode incompatibility, but
enum declaration order is still the wire identity. Before third-party clients,
dynamic descriptors, or a larger stimulus vocabulary, introduce explicit
stable discriminants and a capability exchange. This should be designed after
the server boundaries are clearer, because protocol types should reflect stable
ownership rather than the current monolith.

The design must also reconcile dynamic server descriptions with VST3's fixed
host-visible parameter IDs. Candidate approaches include a stable superset of
parameters, generic fixed slots, or separate plugin classes. Runtime addition
of arbitrary DAW parameters is not a viable assumption.

### Syllabary and expressive note vocabulary

The longer-term composition model treats each stimulus family as a “syllable”:
a small, nameable combination of synthesis model, MPE bindings, and stable
track-level parameters. Wave and TW are the first concrete vocabulary. The
next step is not immediately a generic descriptor protocol; it is to use real
composition experiments to identify the next one or two useful syllables and
the common lifecycle they actually require.

The current design direction and open questions live in
[`docs/composition.md`](docs/composition.md).

### Hardware and perceptual validation

Most correctness evidence currently comes from unit tests, headless runs,
viewer geometry, monitoring, and buffer capture. When the table and interface
are available, establish repeatable physical bring-up and calibration:

- verify channel order, polarity, gain, and transducer heterogeneity;
- measure usable output and resonances across 20–200 Hz;
- evaluate the perceptual consequences of Wave motion, TW wavelength, decay,
  and multi-voice headroom; and
- feed measured constraints back into safe defaults rather than embedding
  machine-specific assumptions in the synthesis model.

## Research directions

These are valuable questions, not scheduled features.

### Boundaries, reflections, and richer spaces

Wave currently models a point source propagating directly to fixed
transducers; TW supplies an instantaneous radial phase field. Neither models
physical table boundaries or reflections. Possible future work includes image
sources, modal/standing structures, spatially varying wave speed or damping,
and non-Euclidean coordinate spaces. Any candidate should first be formulated
and rendered offline, with its perceptual purpose stated, before entering the
real-time engine.

This does not imply restoring the removed in-phase “Standing” placeholder. A
future modal or reflected model would be a new, explicitly defined stimulus.

### Body layouts and calibration layers

The current layout is geometric and transducer-centric. A later translation
layer may describe body plans, heterogeneous actuator types, local sensory
sensitivity, or mappings from abstract spaces such as a torus or connectome.
The central question is where calibration ends and composition begins: hardware
compensation should not silently rewrite an authored spatial relationship.

### Reactive and analysed inputs

Max for Live or another DAW-side layer can translate audio analysis into stable
VST automation without placing a general analysis or modulation engine in the
server. Composition experiments should determine which reactive sources are
musically useful before the wire protocol grows dedicated modulation concepts.

### Output conditioning

Potential high-pass filtering, DC protection, limiter policy, calibration EQ,
and reconstruction changes should be treated as one output-safety problem.
Any filter must be measured for phase, group delay, headroom, and interaction
with the existing sinc reconstruction rather than added as an isolated fix.

## Open decisions

- **Plugin formats:** VST3 is the supported export. CLAP remains disabled; only
  revisit it when there is a concrete host or distribution need.
- **Application packaging:** the unified behavior currently lives in the
  `haptic-viewer` binary for compatibility. A future macOS application bundle
  can rename the user-facing executable and carry `haptic-server` as a private
  helper without changing runtime ownership.
- **Human-readable instance identity:** the viewer currently exposes shortened
  generated IDs. Track or scene labels would improve multi-instance sessions,
  but ownership and persistence semantics need definition.
- **Viewer vertical orientation:** confirm whether increasing timbre should map
  toward the viewer's visual top or bottom and make the requested/effective
  cursor convention consistent.
- **Frequency expression:** note pitch is latched at note-on and pitch bend is
  spatial x. A later syllable may need continuous pitch, but it should not take
  that dimension away from existing stimuli without an explicit binding model.

## Deferred or excluded

- A DAW plugin that owns the audio device and complete synthesis engine: the
  controller/server split is the established architecture.
- Dynamic allocation or unbounded polyphony in the server callback.
- A hidden compatibility implementation of the removed Standing placeholder.
- Exact viewer reconstruction of Wave delay-line history without enough
  synchronized engine state to make the claim truthful.
- OSC, network transport, MIDI 2.0 discovery, or a general server modulation
  matrix without a demonstrated composition need.

## Related documents

- [`README.md`](README.md) — project introduction and reading map.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — current runtime architecture and
  real-time contracts.
- [`docs/wave.md`](docs/wave.md) — delay-line Wave model.
- [`docs/travelling-wave.md`](docs/travelling-wave.md) — instantaneous TW model.
- [`docs/composition.md`](docs/composition.md) — composition workflow and
  syllabary direction.
- [`docs/learnings.md`](docs/learnings.md) — reusable lessons from completed
  investigations.
- [`BUILD.md`](BUILD.md) and [`TESTING.md`](TESTING.md) — build and verification
  workflows.
- [`docs/planning/README.md`](docs/planning/README.md) — frozen historical plans;
  useful for provenance, not current state.
