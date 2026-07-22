# Haptic VST Architecture

This document describes the system that runs now. Project direction and future
work live in [`ROADMAP.md`](ROADMAP.md); build and operational procedures live
in [`BUILD.md`](BUILD.md) and [`TESTING.md`](TESTING.md).

## System shape

Haptic VST separates DAW integration from real-time audio-device ownership.
Many controller instances can send notes and configuration to one server, while
one or more observers can inspect the server's combined state.

```text
 Ableton / VST host
        │ MIDI + MPE + automation
        ▼
 ┌───────────────────┐        framed Unix socket
 │ Haptic Controller │ ─────────────────────────────┐
 │ one per DAW track │                              │
 └───────────────────┘                              ▼
                                             ┌───────────────┐
 ┌───────────────────┐   status + test       │ haptic-server │
 │   haptic-viewer   │ ◀───────────────────▶ │ 32-ch engine  │
 │ observer + console│                       └───────┬───────┘
 └───────────────────┘                               │
                                             monitor routing
                                                     │
                                                     ▼
                                                audio device
```

The split establishes clear ownership:

- The **plugin** owns one track's outgoing patch and translates host events to
  commands. It does not claim to display whole-system state.
- The **server** owns instance registration, voices, synthesis, layout, routing,
  status publication, and the physical audio device.
- The **viewer** is an observer of server state. Its test console is also a
  controller instance with its own identity and configuration.
- The **protocol crate** is the shared boundary. A protocol change is not
  complete until every producer and consumer agrees.

## Workspace map

| Path | Responsibility |
|---|---|
| `haptic-protocol` | Wire commands/status, frame codec, shared limits, stimulus configuration, and geometric TW helpers. |
| `haptic-server` | IPC listener, connection lifecycle, fixed-capacity stimulus engine, CPAL output, layout loading, and headless sink. |
| `haptic-plugin` | VST3 controller, MIDI/MPE merge state, automatable parameters, reconnect worker, and editor. |
| `haptic-plugin-standalone` | Standalone host for the same controller plugin. |
| `haptic-viewer` | Server observer, field visualisation, instance filtering, monitor routing, and test console. |
| `xtask` | VST3 bundling commands. |
| `tools/test_note.py` | DAW-free scripted protocol client. |

The server is still concentrated in `haptic-server/src/engine.rs`; splitting
its lifecycle, DSP, reconstruction, routing, and snapshot responsibilities is
an active roadmap priority.

## Protocol and connection lifecycle

Transport is a Unix-domain stream socket. Each message is:

```text
u32 little-endian payload length
bincode payload
```

The frame decoder accumulates fragmented reads and can yield multiple coalesced
frames. Frames are bounded by the shared maximum size.

Every connection must begin with exactly one `Hello` containing:

- the exact `PROTOCOL_VERSION`;
- a client-supplied `instance_id`;
- `Controller` or `Observer` role; and
- the instance's initial `InstanceConfig`.

The server validates the handshake, rejects a duplicate live identity, reserves
an engine registry slot, and replies with `HelloAccepted`. A plugin only marks
itself connected after receiving that acknowledgement. Commands before `Hello`,
incompatible versions, and invalid non-finite values are rejected before they
reach the audio-thread queue. Finite control values are clamped to shared
bounds where the protocol defines a bounded domain.

Later commands do not carry an instance ID. The IPC layer stamps them with the
identity bound to their connection before creating an internal
`EngineCommand`. This prevents a client from controlling another instance's
voices.

Voice identity is:

```text
(instance_id, MIDI channel, MIDI note)
```

On terminal connection loss the IPC layer retries a `DisconnectInstance`
command until the engine can accept it. The engine releases the instance's
voices and reclaims its fixed registry slot. Critical lifecycle capacity is
kept available under control-message pressure.

Controllers receive the handshake acknowledgement but no continuous status.
Observers receive layout, routing, active voices, and transducer levels and
must continue reading. Status writes use bounded per-client buffers with
resumable partial writes; temporary `WouldBlock` is backpressure, not immediate
connection failure.

Protocol v3 still derives bincode enum identity from declaration order. Exact
version matching makes incompatible builds fail clearly, but stable explicit
discriminants and capability negotiation remain future work.

## Plugin event and parameter flow

The host calls `HapticPlugin::process()` on its audio thread. The plugin keeps a
16-entry per-channel `MpeData` cache so an event that changes one expression
dimension does not reset the others. It translates:

- Note On/Off into voice lifecycle commands;
- pitch bend into source x;
- channel/poly pressure into intensity; and
- CC74/timbre into source y.

Strike velocity controls amplitude independently of pressure. Standard MIDI
frequency is calculated without transposition and clamped to 20–200 Hz by the
server. UI note names use Ableton's octave convention, where MIDI 60 is C3.

The callback hands commands to a bounded nonblocking channel owned by the IPC
worker. A full queue drops noncritical outgoing work instead of blocking the
host audio thread. The worker owns the socket, handshake, reconnection, and
configuration replay.

Host-visible parameters are stable even when a selected stimulus does not use
all of them:

- stimulus type: Wave or Travelling Wave;
- wave speed;
- TW scale mode and wavelength; and
- distance-decay knee and exponent.

Each plugin instance publishes its complete configuration through a
sequence-checked atomic snapshot. The reconnect worker retries until it reads a
coherent snapshot; the plugin callback does not lock or allocate to publish it.

Parameter apply timing is part of the sound:

- stimulus type affects new notes;
- Wave speed is latched for a delay-line Wave voice;
- TW speed/wavelength mode and spatial scale update held TW voices with a
  wavenumber ramp; and
- decay changes affect TW directly and new Wave emissions, while already
  scheduled Wave energy retains its emission-time gain.

## Server threads and data movement

The server has three principal execution contexts:

1. **Audio callback or dummy-audio loop.** Owns `StimulusEngine`, drains engine
   commands once per callback, renders audio, and publishes bounded levels and
   voice snapshots.
2. **IPC thread.** Owns the listener and clients, validates/decode commands,
   pushes engine commands, and publishes observer status.
3. **Configuration watcher.** Polls `haptic.toml` metadata at about 1 Hz,
   parses changes off the audio thread, and offers accepted candidates to
   bounded engine/observer paths.

Shutdown is coordinated through an atomic flag. Audio callback timing is
recorded through lock-free counters and logarithmic histogram buckets; a
monitor thread formats and reports the results away from the callback.

The important flows are:

```text
socket command   -> IPC validation -> rtrb command ring -> audio callback
layout candidate -> config watcher -> bounded layout ring -> audio callback
logical levels   <- IPC broadcast  <- bounded levels ring <- audio callback
voice snapshot   <- IPC broadcast  <- bounded snapshot ring <- audio callback
```

## Real-time contract

The audio callback and per-internal-frame stimulus paths must not:

- allocate or deallocate;
- acquire locks;
- perform blocking operations or I/O;
- format strings or routinely log;
- depend on unbounded work supplied by another thread.

The implementation supports this with:

- fixed eight-slot pools for each stimulus type;
- fixed owner and instance registries;
- `rtrb` SPSC rings for cross-thread engine state;
- bounded plugin command transport;
- generation-tagged delay cells, so reset and panic do not clear megabytes;
- precomputed FIR and scatter kernels; and
- explicit drop/coalescing behaviour where noncritical state outruns a queue.

The callback records timing through atomics, but the current project does not
yet have a formal allocator guard covering every callback transition. That is
an active roadmap item.

## Engine lifecycle

`StimulusEngine` owns two `StimulusPool`s:

```text
WaveStimulus             8 slots
TravellingWaveStimulus   8 slots
```

Each pool has a parallel fixed owner table. Note On selects the sending
instance's configured stimulus type, allocates a free slot, or steals
deterministically—prefer an oldest releasing voice, otherwise an oldest active
voice. Note Off and MPE update locate the slot through the instance/channel/note
ownership key.

An envelope closes the source on note-off. TW can finish when that envelope is
inactive; Wave remains active until its latest possible scattered arrival has
been consumed. Disconnect follows release semantics rather than leaving a
sustained owner, while Panic resets all pools and ownership immediately.

Both stimuli share concrete fixed-state components for envelope behaviour,
controller smoothing, oscillator phase, and distance decay. They deliberately
do not share propagation semantics. See [`docs/wave.md`](docs/wave.md) and
[`docs/travelling-wave.md`](docs/travelling-wave.md).

## Render path and output routing

The engine produces 32 logical samples per internal render frame. At a 48 kHz
device rate, internal synthesis runs at 1.5 kHz (`RENDER_DECIMATION = 32`),
whose 750 Hz Nyquist remains above the 200 Hz stimulus ceiling. A 512-tap
polyphase Kaiser-windowed sinc reconstructs device-rate samples with 16 taps per
phase.

Per device frame:

1. Drain commands once at callback entry.
2. Advance or reuse an internal 32-channel render frame.
3. Sum active Wave and TW voices.
4. Apply layout gains and bounded logical mixing.
5. Reconstruct device-rate samples through the polyphase filter.
6. Apply final output safety bounds.
7. Copy the selected logical channels to physical device outputs according to
   monitor routing.

Logical levels and viewer state are measured before physical routing. A stereo
fallback therefore does not turn the engine into a stereo engine: it merely
allows two of the 32 logical channels to be auditioned at once.

The default per-transducer gain is 0.5, reserving headroom for the maximum 2×
Doppler arrival bunching allowed by the Wave source-speed limit. An explicit
layout gain overrides the default.

## Layout and device selection

`haptic.toml` describes table dimensions, a grid shorthand, and optional
per-channel position/gain overrides. The default is a cell-centred 4×8 layout
over a nominal 1 m × 2 m table.

A present but invalid startup configuration is a hard error. During hot reload,
an invalid edit is reported and the running layout is kept. Parsing happens off
the callback.

The server searches for a device exposing at least 32 output channels. If none
exists, it deliberately uses the system default device for development and
monitoring. Within the selected channel layout it prefers a supported 48 kHz
`f32` configuration; otherwise it chooses the closest supported rate and
reports the deviation. The server currently requires `f32` output samples.

## Observation contract

Observers receive:

- RMS levels for all 32 logical channels at about 60 Hz;
- layout and physical routing state;
- a fixed-capacity snapshot of up to 16 active voices; and
- an explicit empty active-voice snapshot when nothing is sounding.

`VoiceInfo` contains identity, note type, frequency, effective scale, decay,
requested/effective position, and amplitude. It does not contain synchronized
oscillator phase or Wave delay-line contents. The viewer therefore reconstructs
relative spatial phase geometrically and phase-aligns voices for display.

For a single TW voice, the relative phase and distance gain use the same shared
closed-form helper as the engine and can match closely. For Wave, or for the
absolute interference of multiple voices, the display remains a deliberately
labelled preview rather than exact output truth.

## Operational profiles

### Production/audio-device mode

- Uses `/tmp/haptic-vst.sock` unless overridden.
- Refuses to replace another live server at that endpoint.
- Removes a proven-stale socket selectively.
- Opens the selected CPAL device.

### Headless/dummy mode

- Uses a paced 48 kHz, 32-channel memory sink.
- Does not enumerate or open physical audio hardware.
- Defaults to `/tmp/haptic-vst-test-<pid>.sock`.
- Accepts `--socket` or `HAPTIC_SOCKET_PATH` for a stable test endpoint.

Both profiles run the same engine, IPC, snapshots, layout watcher, and health
reporting. Headless mode is therefore the preferred DAW-free integration path,
not a reduced mock implementation.
