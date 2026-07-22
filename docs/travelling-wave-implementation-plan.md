# Travelling Wave (`tw`) stimulus implementation plan

*Proposed and implemented 2026-07-22. This document records the implementation
path for the instantaneous Travelling Wave stimulus, abbreviated `tw`/`TW`.
`ROADMAP.md` remains the source of truth for current priorities.*

## Implementation status

Complete in protocol v3: the shared schema/model and bounds, eight-voice
allocation-free engine pool, old placeholder removal, stable plugin parameters,
sequence-checked reconnect snapshot, 16-voice observer status, viewer/test
console, scripted client, lifecycle and closed-form tests, and a release
headless smoke test. The old second stimulus slot deliberately maps to
`TravellingWave`; only `Wave` and `TravellingWave` exist at runtime.

## 1. Objective

Replace the existing placeholder `Standing` stimulus with `TravellingWave`, a
stimulus that has the spatial character of the current delay-line `Wave`
stimulus without modelling propagation history. After this work the complete
stimulus-type vocabulary is exactly:

- `Wave`: the existing delay-line propagation and Doppler model; and
- `TravellingWave` (`tw`/`TW`): the new instantaneous radial phasor model.

`TravellingWave` is the Rust variant name, `Travelling Wave` is the UI label,
and `tw`/`TW` are concise command-line and display abbreviations. `Standing`,
`StandingWave`, and the existing in-phase placeholder are removed.

A Travelling Wave voice has:

- a sinusoidal source frequency selected by the existing haptic MIDI-note map;
- a two-dimensional source position controlled by MPE bend and timbre;
- pressure, velocity, attack, and release behavior consistent with `Wave`;
- the same distance-decay law and parameters as `Wave`;
- a radial phase pattern controlled either by wave speed or by fixed
  wavelength;
- live spatial-scale automation while a note is held; and
- a matching geometric viewer representation.

It deliberately has no delay lines, in-flight energy, propagation tail,
Doppler shift, Doppler amplitude gain, or source-speed limit.

## 2. Signal model

For transducer `i`, at distance `d_i` from the current source position:

```text
theta[n + 1] = theta[n] + 2*pi*f*dt
k_target = 2*pi*f/c                 (speed mode)
         = 2*pi/lambda              (fixed-wavelength mode)
g(d) = (1 + d/d0)^(-p)
y_i[n] = A[n] * pressure[n] * g(d_i) * sin(theta[n] - k[n]*d_i)
```

The current hard-coded `Wave` attenuation, `1 / (1 + 2*d)`, is exactly the
default `d0 = 0.5 m`, `p = 1`. The implementation should extract one shared
attenuation function so `Wave`, `TravellingWave`, and the viewer cannot drift.

`k`, the spatial wavenumber, is the canonical live state. Interpolating `k`
rather than `c` or `lambda` gives a continuous spatial phase pattern when the
author changes either representation or switches between them.

### Control timing

- A parameter target received by the engine starts affecting `k` on the next
  internal render frame. There is no distance-dependent wait and no stored
  history to drain.
- Patch automation should use the existing measured-spacing ramp discipline:
  ramp from the current `k` toward the new target over the expected interval
  until the next update. This starts immediately, while avoiding block-rate
  phase steps and zipper sidebands.
- A note starts at its configured spatial scale without an onset glide.
- MPE position and pressure keep the existing controller smoothing, but a
  TW source follows the smoothed position directly. It is not constrained by
  `SOURCE_SPEED_FRACTION`, because there is no scatter-write arrival ordering
  to protect.

In fixed-wavelength mode, every note uses the configured `lambda`; its
effective speed is `c = f*lambda`. In speed mode, every note uses the configured
`c`; its wavelength is `lambda = c/f`.

## 3. Public controls and apply semantics

Keep VST3 parameter IDs stable and define all host-visible parameters up front;
the plugin cannot safely add and remove automation parameters dynamically.

| Parameter | Proposed representation | Apply semantics |
|---|---|---|
| stimulus type | replace the existing `Standing` choice with `Travelling Wave` | new notes select the pool; changing type does not mutate an existing voice |
| propagation speed | retain existing `wave_speed` ID and range, relabel as needed | latched for delay-line `Wave`; live for TW in speed mode |
| TW scale mode | new enum: `Speed`, `Fixed wavelength` | live; changes the source used to calculate target `k` |
| TW wavelength | new logarithmic float parameter | live in fixed-wavelength mode |
| attenuation knee `d0` | new logarithmic float parameter in metres | live for `Wave` emissions and TW output |
| attenuation exponent `p` | new float parameter | live for `Wave` emissions and TW output |

Initial shared limits should live in `haptic-protocol`, not be copied between
UIs and the engine:

```text
speed:      0.25 .. 1000 m/s       (existing)
wavelength: 0.00125 .. 50 m        (covers c/f over 20..200 Hz)
d0:         0.01 .. 10 m
p:          0 .. 4
```

The neutral/default attenuation remains `d0 = 0.5 m`, `p = 1`, preserving the
current sound exactly. A zero exponent intentionally produces no distance
falloff.

Inactive controls remain registered with the DAW but are visually disabled or
de-emphasized in the custom editor. This preserves automation compatibility.

## 4. Protocol and state model

This is a coordinated protocol change and must bump `PROTOCOL_VERSION`. Every
producer and consumer must be rebuilt together.

### Shared types

Add:

```text
StimulusType::TravellingWave
SpatialScaleMode::{Speed, Wavelength}
DistanceDecay { d0_m, exponent }
TravellingWaveConfig { scale_mode, wave_speed, wavelength_m }
```

Delete `StimulusType::Standing` and replace its second enum position with
`StimulusType::TravellingWave`. The exact-match protocol version still bumps,
but retaining the second plugin enum position gives old DAW automation a
deterministic migration: an old `Standing` selection becomes `Travelling Wave`
rather than an invalid value. Remove all standing-specific wire fixtures and
match arms.

Extend `InstanceConfig` with `TravellingWaveConfig` and `DistanceDecay`.
Preserve the existing `wave_speed` field and semantics for delay-line `Wave` so
old DAW sessions retain the same stable host parameter ID and sound after
migration.

Add parameter messages for scale mode, wavelength, `d0`, and `p`. Validation at
the socket boundary must reject non-finite values and clamp finite values to
the shared ranges before they enter the audio queue.

`SetParameter` processing has two responsibilities:

1. update the sending instance's configuration for future notes and reconnects;
2. for parameters marked live, update every active voice owned by that instance.

Delay-line wave speed remains latched at note-on. The same speed message updates
active TW voices only when their scale mode is `Speed`.

### Voice snapshots

Extend `VoiceInfo` so the viewer receives the effective, interpolated spatial
scale and actual decay values used by the engine. Recommended fields are:

```text
scale_mode
wavelength_m
atten_d0_m
atten_exponent
```

`wave_speed` may remain for compatibility and display; for TW it is the
effective `frequency * wavelength_m`. The viewer should calculate its phasor
from `wavelength_m`, which avoids reconstructing a different value after live
mode changes.

Replacing the four-voice standing pool with an eight-voice TW pool raises the
possible voice count from 12 to 16. Replace the hand-maintained
`MAX_ACTIVE_VOICES = 12` assumption with a value derived from, or at least
compile-time checked against, all pool capacities. Keep the serialized maximum
under `MAX_FRAME_SIZE` with a frame-budget test.

### Plugin reconnect snapshot

The plugin currently packs its entire reconnect configuration into one
`AtomicU64`. The expanded configuration no longer fits. Replace that packing
with a small lock-free atomic snapshot (per-field atomics guarded by a sequence
counter) so the plugin audio callback still performs no lock, allocation, or
blocking work and the reconnect thread cannot observe a torn configuration.

## 5. Server implementation shape

Before replacing the pool, extract any reusable pieces currently duplicated
between `WaveStimulus` and `StandingWaveStimulus`, then delete
`StandingWaveStimulus` completely:

- envelope state and attack/release stepping;
- MPE smoothing and MPE-to-table-position mapping;
- distance attenuation and its bounded parameter interpolation; and
- oscillator phase advance.

Keep these as concrete, preallocated DSP components rather than trait objects
or heap-owning abstractions in the render path.

Add a fixed-capacity
`StimulusPool<TravellingWaveStimulus, MAX_TRAVELLING_WAVE_STIMULI>` and
parallel owner table. `TravellingWaveStimulus` should contain only fixed-size
scalar state: oscillator, envelope, MPE interpolation, source position,
current/target spatial wavenumber, decay interpolation, and configuration
mode. It must not allocate or own a `DelayLine`.

Integrate the pool into every lifecycle path:

- note allocation and deterministic voice stealing;
- note-off and MPE routing by `(instance_id, channel, note)`;
- disconnect release;
- panic/reset;
- active/releasing checks and owner reaping;
- render summation before layout gains, reconstruction, routing, and clamp; and
- active-voice snapshots.

Removal must cover the old standing pool, owner table, note routing, MPE
routing, disconnect/release/panic handling, snapshot construction, tests, and
all non-historical UI/docs references. There must be no dormant standing
runtime path left behind.

The initial capacity should be eight TW voices, matching `Wave`. At 32
transducers this adds a bounded 256 direct phasor evaluations per internal
frame in the worst case. Measure it, but do not optimize by adding lookup tables
unless callback timing shows a real need.

## 6. Controller and viewer behavior

### Plugin

- Replace `Standing Wave` with `Travelling Wave (TW)` in the stimulus selector
  without changing the existing `stim_type` parameter ID or its numeric slot.
- Register stable parameters for TW scale mode, wavelength, `d0`, and `p`.
- Send live parameter changes through the bounded command channel and update
  the lock-free reconnect snapshot.
- Show speed, wavelength, and the derived counterpart for TW, while making
  it clear which value is fixed.
- Preserve the existing build hash and protocol version display.

### Viewer test console

- Replace the Standing choice with `TW` in the stimulus selector.
- In TW mode, offer speed/fixed-wavelength selection and the corresponding
  active slider, plus shared decay controls.
- Do not retrigger a held TW note for spatial-scale or decay changes. Send the
  parameter update live.
- Continue to retrigger for note, velocity, or stimulus-type changes.
- Keep delay-line `Wave` speed changes latched/retriggered, matching engine
  semantics.

### Viewer field

For TW, calculate the same complex radial field as the engine's relative
phase:

```text
phasor_i = amplitude * g(d_i) * exp(-j*2*pi*d_i/lambda)
```

Draw both source cursors, but they should normally coincide for TW because it
has no wave-speed-dependent source chase. Label the mode `TW`, show whether
speed or wavelength is fixed, and show the effective `c` and `lambda`.

The viewer remains a phase-aligned geometric preview across voices because
snapshots still do not carry synchronized oscillator phase. For an individual
TW voice its relative spatial phase and attenuation should, however, match
the engine exactly.

Update `tools/test_note.py` to encode the bumped protocol and expose `--type tw`,
`--scale-mode`, `--wavelength`, `--atten-d0`, and `--atten-p` so the new mode is
testable without a DAW.

## 7. Implementation milestones

Each milestone should be a focused commit when commits are requested.

### Milestone 1 — schema and closed-form model

1. Add shared types, ranges, defaults, validation, and protocol round-trip tests.
2. Replace `Standing` with `TravellingWave`, bump the exact-match protocol
   version, and update all match expressions and fixtures.
3. Add pure functions for distance gain and TW relative phasor.
4. Test default attenuation equivalence and representative phase values before
   connecting the model to the real-time engine.

Exit: every workspace consumer compiles against the new schema and the DSP
equation is pinned by deterministic unit tests.

### Milestone 2 — remove Standing and add `TravellingWaveStimulus`

1. Extract shared envelope/controller/attenuation components without changing
   existing `Wave` output.
2. Remove the standing stimulus, pool, owners, lifecycle paths, and snapshots.
3. Implement the allocation-free `TravellingWaveStimulus` and its live
   wavenumber ramp.
4. Add the TW pool and complete lifecycle/ownership integration.
5. Publish truthful snapshots including an explicit empty state.

Exit: headless engine tests can start, move, automate, release, disconnect, and
panic TW voices with finite bounded output and no delay-line tail. Engine and
protocol stimulus-type matches contain only `Wave` and `TravellingWave`.

### Milestone 3 — plugin and reconnect safety

1. Add the stable host parameters and editor controls.
2. Replace packed reconnect configuration with the sequence-checked atomic
   snapshot.
3. Route live automation without locks or allocations in `process()`.
4. Rebuild the release VST bundle and verify the embedded build hash changes.

Exit: standalone/plugin-core tests prove coherent reconnect configuration and
live commands; a bundle is available for later DAW testing, but is not installed.

### Milestone 4 — viewer and DAW-free end-to-end testing

1. Remove Standing controls/rendering and add TW test controls and field rendering.
2. Update the scripted client.
3. Extend the real-socket integration test to exercise a held TW note while
   changing speed, wavelength mode, wavelength, position, and decay.
4. Run an isolated headless server plus scripted client/viewer-protocol smoke
   test on a dedicated socket.

Exit: TW can be driven and observed end-to-end without a DAW or audio device,
and a held voice does not reconnect, retrigger, or stick during automation.

### Milestone 5 — capture, performance, and documentation

1. Add a deterministic multichannel capture comparing engine output with the
   closed-form TW field over moving-source and scale sweeps.
2. Check for discontinuities, non-finite samples, clamp incidence, automation
   sidebands, and callback-budget regression at maximum pool occupancy.
3. Update `ARCHITECTURE.md`, `TESTING.md`, current design docs, and `ROADMAP.md`.
4. Run format, workspace check, tests, strict Clippy, and the targeted headless
   capture/smoke checks.

Exit: the implementation meets the acceptance criteria below and is documented
as current behavior rather than a proposal.

## 8. Acceptance criteria

- A TW note produces the closed-form radial phase and shared distance gain at
  all 32 logical transducers without constructing or touching delay lines.
- Moving the source updates the spatial field from the next render frame,
  subject only to the documented controller smoothing; there is no propagation
  tail or Doppler effect.
- Speed and fixed wavelength can be automated while a note is held. The spatial
  pattern begins changing immediately and transitions continuously without a
  note retrigger.
- Fixed wavelength gives the same spatial pattern for different note
  frequencies while effective speed changes with frequency.
- Speed mode gives wavelength `c/f` for every note.
- Default decay is numerically equivalent to the current Wave decay. Live decay
  changes affect active TW voices and new Wave emissions without unsafe output.
- Viewer relative phase and gain agree with engine captures for a single TW
  voice within floating-point tolerance.
- Disconnect, panic, voice stealing, and release cannot leave a TW voice
  sounding or owned.
- The engine remains 32 logical channels regardless of physical device count.
- No audio callback or per-sample path gains allocation, deallocation, locks,
  blocking I/O, or routine logging.
- Full workspace format/check/test/Clippy gates pass, and the existing Wave
  capture metrics do not regress outside an explicitly documented tolerance.

## 9. Risks and future-roadmap considerations

- **Automation phase sensitivity.** At large distances or very short
  wavelengths, a small scale change produces a large phase change. Ramping
  wavenumber is mandatory; direct block-step assignment is likely to click.
- **Spatial aliasing.** Wavelengths below roughly twice the transducer spacing
  cannot be represented as a sampled physical field. Keep the broad protocol
  range for experiments, but show a viewer warning based on actual layout
  spacing rather than silently clamping the model.
- **Summed headroom.** Replacing four Standing slots with eight TW slots adds
  four voices to worst-case superposition. Preserve the final clamp and capture
  maximum-occupancy cases; consider an explicit mixer headroom policy before
  increasing pools again.
- **Snapshot/schema growth.** Pool capacities and status frame size must be
  checked together. This is a warning against adding another pool without
  updating the protocol capacity test.
- **Parameter evolution.** The present enums are still order-sensitive under
  bincode. Exact protocol negotiation prevents silent mismatch, but stable
  numeric syllable/parameter IDs remain necessary before third-party clients or
  dynamic syllable descriptors.
- **Standing migration.** Old sessions that stored the second stimulus enum
  value will select Travelling Wave after upgrade. Document this deliberate
  semantic replacement; do not retain a hidden compatibility implementation
  of the in-phase Standing placeholder.
- **Shared decay semantics.** A live decay change affects TW immediately but
  delay-line Wave only at emission; already-scheduled Wave energy retains its
  emission-time gain. The UI and docs must state this difference.
- **Future pitch control.** Note frequency is currently latched and MPE bend is
  source x-position. If a later binding makes frequency continuous, fixed
  wavelength naturally changes effective speed while speed mode changes
  wavelength; the canonical-wavenumber design supports both without a protocol
  redesign.

## 10. Non-goals for this tranche

- replacing the existing delay-line `Wave` model;
- retaining or implementing any standing-wave stimulus;
- synchronizing oscillator phase in observer snapshots;
- redesigning the complete syllabary descriptor protocol;
- installing or opening the VST in a DAW; or
- exercising physical audio hardware without an explicit request.
