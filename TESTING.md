# Testing the haptic system

This document covers automated, headless, interactive, DAW, and physical-device
workflows. Build commands and bundle details live in [`BUILD.md`](BUILD.md).

Prefer the smallest workflow that exercises the behaviour under test. Most
engine, protocol, reconnect, and stimulus work can be verified without loading
a DAW or opening an audio device.

## Processes and roles

| Process | Typical command | Role |
|---|---|---|
| Haptic application | `cargo run -p haptic-viewer --release` | Primary GUI; observes state and attaches to or supervises a server. |
| server | `cargo run -p haptic-server --release` | Independent/headless engine, socket, layout, and physical audio device. |
| scripted client | `python3 tools/test_note.py` | Drives notes, MPE, parameters, and routing without a DAW. |
| VST3 plugin | loaded by a DAW | Converts host MIDI/MPE and automation to controller commands. |

If Cargo is not on `PATH` after a rustup installation:

```bash
source "$HOME/.cargo/env"
```

## Server profiles

Important server options:

```text
--config PATH             layout file; defaults to ./haptic.toml
--test-tone               100 Hz channel-cycling hardware pattern
--headless                paced 48 kHz, 32-channel memory sink
--dummy-audio             alias for --headless
--socket PATH             override socket and singleton namespace
--managed-lifetime-stdin  internal supervisor mode; exit on stdin EOF
HAPTIC_SOCKET_PATH        environment alternative to --socket
```

Normal mode uses `/tmp/haptic-vst.sock` and opens a physical device. It refuses
to replace another live server at that endpoint.

Headless mode opens no physical audio device and defaults to:

```text
/tmp/haptic-vst-test-<pid>.sock
```

The per-process endpoint prevents test servers from contending with production
or one another. Multi-process tests should choose an explicit stable endpoint:

```bash
cargo run -p haptic-server --release -- \
  --headless --socket /tmp/haptic-vst-test.sock
```

An occupied explicit endpoint fails immediately; the server never waits for a
test lock or unlinks another live server.

The Haptic application accepts:

```text
--connect-only            attach without starting or stopping a server
--server-bin PATH         override the managed server executable
--config PATH             layout passed to a managed server
--headless                start the managed server with dummy audio
--test-tone               start the managed server in hardware test-tone mode
--socket PATH             endpoint used for both attachment and launch
HAPTIC_SERVER_BIN         environment alternative to --server-bin
HAPTIC_SOCKET_PATH        environment alternative to --socket
```

By default it probes the selected endpoint. A reachable server is treated as
external and left running when the window closes. If no server is reachable,
the application starts a sibling `haptic-server`, captures its logs, and shuts
it down when the application exits.

## Fastest end-to-end test: headless plus script

Terminal 1:

```bash
cargo run -p haptic-server --release -- \
  --headless --socket /tmp/haptic-vst-test.sock
```

Terminal 2:

```bash
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock
```

Useful scripted variants:

```bash
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock --orbit
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock \
  --note 48 --velocity 80 --duration 5
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock \
  --wave-speed 100
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock \
  --type tw --scale-mode wavelength --wavelength 0.125 \
  --atten-d0 0.75 --atten-p 1.5 --orbit
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock \
  --route 0:31 --route 1:13
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock --panic
```

The default scripted note is MIDI 36 / Ableton C1 / 65.4 Hz. Frequencies are
not transposed; the engine uses standard equal temperament and clamps to
20–200 Hz.

Use this workflow for:

- protocol and handshake changes;
- note/MPE/parameter lifecycle;
- Wave and TW automation semantics;
- reconnect and disconnect cleanup;
- headless callback health; and
- build-hash-independent debugging before DAW integration.

## Interactive workflow without a plugin

Build the GUI and server helper together once:

```bash
cargo build -p haptic-server -p haptic-viewer --release
```

Then launch the application:

```bash
cargo run -p haptic-viewer --release
```

The **Haptic** window starts a managed physical-audio server when the production
endpoint is free. Its top panel identifies the server as managed or external,
shows the socket, provides owned-child start/stop/restart controls, and contains
a bounded server log.

To run the complete application without physical hardware:

```bash
cargo run -p haptic-viewer --release -- \
  --headless --socket /tmp/haptic-vst-test.sock
```

To attach to a server whose lifecycle is managed in another terminal or by a
service:

```bash
cargo run -p haptic-viewer --release -- \
  --connect-only --socket /tmp/haptic-vst-test.sock
```

The viewer's controls are stacked to fit the default 620 px window width.

1. Choose **Wave** or **Travelling Wave (TW)** and start the test note.
2. Note and velocity changes retrigger the held test voice.
3. Wave speed retriggers a held Wave because speed is latched at note-on.
4. TW speed/wavelength mode, spatial scale, and decay update live.
5. Drag on the table or enable orbit to move the source.
6. Left-click a transducer to route it to physical output 1; right-click routes
   it to output 2. Badges show current routing.

The viewer always displays all 32 logical channels. Device routing changes only
what is copied to the available physical outputs.

Note names use Ableton's octave convention: MIDI 60 is C3. C3 is 261.6 Hz
before clamping and therefore produces/displays the 200 Hz ceiling. Hue shows
relative spatial phase, brightness shows local amplitude, the cross is the
effective source, and the ring is the requested position.

The field display is a phase-aligned geometric preview. It does not reproduce
synchronized oscillator phase or Wave delay-line history. A single TW voice's
relative field uses the same closed-form helper as the engine.

## Automated Rust tests

Run all workspace tests:

```bash
cargo test --workspace
```

Before a Rust handoff, also run:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
```

For a substantial callback or protocol change:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Coverage includes:

- length-prefixed framing and exact-version handshakes;
- invalid-command rejection, duplicate identities, and disconnect cleanup;
- voice allocation, stealing, release, panic, and multi-instance isolation;
- Wave delay/tail/smoothing/scatter/reconstruction behaviour;
- TW closed-form field, scale modes, live automation, and lifecycle;
- active-voice frame budget and observer snapshots;
- layout parsing/reload and per-transducer gains;
- 48 kHz output-config preference and monitor routing; and
- framed end-to-end IPC over a real Unix socket.

`cargo test` compiles test targets, but it does not replace a release binary
already running in another terminal. Restart that process after rebuilding.

## Targeted DSP capture

The ignored engine test `orbit_capture_writes_debug_buffers` drives the Wave
model with the viewer's orbit-style MPE stream and writes multichannel samples
for offline analysis. It is intentionally excluded from ordinary test runs.

Use it when changing:

- delay scheduling or capacity;
- source motion/smoothing;
- scatter kernels;
- internal render rate or reconstruction;
- output headroom; or
- Wave envelope/tail semantics.

Run the ignored test deliberately and set its documented environment variables
in `haptic-server/src/engine.rs` for the scenario under investigation. Evaluate
at least:

- non-finite samples;
- peak and clamp incidence;
- stationary noise/spurs;
- orbit-period discontinuities;
- expected Doppler direction and amplitude behaviour; and
- reconstruction images and in-band sidebands.

A reproducible default capture and dependency-free FFT analysis can be run from
the workspace root with:

```bash
HAPTIC_CAPTURE_OUT="$PWD/target/dsp-capture/default" \
  cargo test -p haptic-server --release \
  orbit_capture_writes_debug_buffers -- --ignored --nocapture

rustc --edition 2021 -O tools/analyze_f32_capture.rs \
  -o target/analyze-f32-capture
target/analyze-f32-capture \
  target/dsp-capture/default/orbit_c1_sr48000.f32 48000 32 2 262144
```

The capture harness accepts Wave speed, duration, sample rate, callback size,
orbit period, note, MPE interval, orbit radius, velocity, pressure, and both
distance-decay parameters through the `HAPTIC_CAPTURE_*` variables listed in
its source comment. Use a distinct output directory for every sweep point.
The analyzer reports per-channel time-domain statistics, aggregate band energy,
and strongest spectral bins. Its dB bands are relative to total spectral
energy across all logical channels.

The current default-orbit baseline, physical bounds, callback-cadence sweep,
and scatter-kernel experiments are recorded in
[`docs/wave-orbit-dsp-analysis.md`](docs/wave-orbit-dsp-analysis.md).

Numerical baselines are evidence for a scenario, not universal performance
promises. Record a new baseline beside the relevant design change when the
model intentionally changes.

## Physical-device bring-up

Only use physical mode when hardware behaviour is the test target.

Start with the server's independent channel-cycling tone:

```bash
cargo run -p haptic-server --release -- --test-tone
```

It emits 100 Hz bursts in turn across the physical outputs. This verifies
device selection, channel count/order, and cabling without depending on a
client or MIDI mapping.

The server reports selected device, sample rate, channel count, buffer range,
callback p50/p99/max, frame count, and stream errors. It prefers a supported
48 kHz `f32` mode for the selected channel layout and reports when another rate
is necessary.

Alternatively, launch the Haptic application with `--test-tone`; it will pass
that mode to a managed server and expose its output in the server log.

After channel bring-up, use the application to route selected logical channels
and exercise Wave/TW at conservative levels. Remember that the default layout
gain is 0.5 but explicit `haptic.toml` gains override it.

## VST3/DAW integration

Use this only after headless and scripted tests pass.

Build the bundle:

```bash
cargo xtask bundle haptic-plugin --release
```

Optionally install it on macOS:

```bash
mkdir -p "$HOME/Library/Audio/Plug-Ins/VST3"
ditto target/bundled/haptic-plugin.vst3 \
  "$HOME/Library/Audio/Plug-Ins/VST3/haptic-plugin.vst3"
```

Then:

1. Start the Haptic application. It attaches to an existing server or starts
   one; alternatively run a server independently and use `--connect-only`.
2. Load **Haptic Controller** in the target host.
3. Compare the editor's build hash and protocol version with the bundle under
   test.
4. Play MIDI/MPE and automate the patch parameters.
5. Use the Haptic application, not the plugin, for whole-server field and
   routing state.

The plugin reconnects automatically and replays a coherent configuration after
`HelloAccepted`. Starting the server before the host is convenient but not
required.

A DAW may retain an old dynamic library after the bundle is replaced. If the
displayed hash is stale, fully quit and restart the host. Repeated pre-Hello or
protocol-version errors also indicate that an older client is still loaded.

Plugin log location defaults to:

```text
/Users/jmz/tmp/log/haptic-vst.log
```

Override it with `NIH_LOG` where supported by the logging layer.

## Wire notes for custom clients

- Frames are `u32` little-endian length plus a bincode payload in both
  directions.
- Enum tags are currently bincode declaration-order `u32` values.
- Every connection must send the exact current `Hello` before any command and
  wait for `HelloAccepted`.
- Protocol v3 has only `Wave` and `TravellingWave`; the second legacy stimulus
  slot maps to TW.
- Controllers receive only the acknowledgement and liveness failure.
- Observers receive continuous status and must keep reading.
- Copy the checked-in script's current schema or use `haptic-protocol`; do not
  hardcode an older frame layout.

## Troubleshooting current operation

| Symptom | Likely cause or check |
|---|---|
| `cargo` not found | Activate the rustup environment shown at the top of this document. |
| Application waits for server | Open **server log**. Build `haptic-server` beside the GUI, pass `--server-bin`, or confirm an external process uses the same socket. |
| Plugin remains disconnected | Confirm protocol/build identity and that the server accepted the instance rather than merely accepting the socket. |
| No sound on a stereo fallback | Route the desired logical channels to physical outputs 1 and 2 in the viewer. |
| Server will not bind | Another live process owns the endpoint; stop it or choose an isolated test socket. |
| Observer is removed during a long test | It is not consuming status fast enough or its bounded backlog remained full; inspect the integrated or external server output. |
| Managed server remains after an ordinary GUI exit | It should receive stdin EOF and stop within three seconds; inspect the server log and verify it was started with `--managed-lifetime-stdin`. |
| Output comes from an unexpected device | No 32-channel device matched and the server deliberately selected the system default. |
| VST editor shows an old hash | The DAW still has an older library loaded; replace the bundle and fully restart the host. |
