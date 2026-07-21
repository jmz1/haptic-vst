# Testing the haptic system

How to run and test every part of the system — with or without a DAW/VST
plugin, with or without the 32-channel interface. Build instructions live in
`BUILD.md`; this document is about *driving* the system.

## 0. Prerequisite: cargo on PATH

`rustup` installs to `~/.cargo/bin`, which is not on the shell PATH by
default on this machine. Either prefix commands ad hoc:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

or fix it permanently (recommended):

```bash
echo 'source "$HOME/.cargo/env"' >> ~/.zshrc   # then open a new terminal
```

## 1. The three processes

| Binary | Run with | Role |
|---|---|---|
| `haptic-server` | `cargo run -p haptic-server --release` | Real-time engine. Owns the audio device (32-ch if found, else default/stereo fallback), listens on `/tmp/haptic-vst.sock`. |
| `haptic-viewer` | `cargo run -p haptic-viewer --release` | Phase visualiser **and test console**. Read-only unless you use its controls; auto-reconnects, so start order doesn't matter. |
| VST3 plugin | loaded by a DAW | MIDI/MPE → server commands. Optional — the viewer can drive tests without it. |

Server flags: `--config <path>` (default `./haptic.toml`, hot-reloads on
save), `--test-tone` (100 Hz burst cycling across all outputs, for hardware
bring-up), `--headless`/`--dummy-audio` (48 kHz, 32-channel in-memory sink),
and `--socket <path>` (transport/lock namespace override).

Normal mode owns the production `/tmp/haptic-vst.sock` singleton and opens a
physical device. Headless mode opens no audio hardware and defaults to the
per-process `/tmp/haptic-vst-test-<pid>.sock`, so it cannot block a production
server or another headless run. For a stable dedicated test endpoint:

```bash
cargo run -p haptic-server --release -- \
  --headless --socket /tmp/haptic-vst-test.sock
cargo run -p haptic-viewer --release -- \
  --socket /tmp/haptic-vst-test.sock
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock --orbit
```

`HAPTIC_SOCKET_PATH=/tmp/haptic-vst-test.sock` is supported by all clients,
including `haptic-plugin-standalone`. An occupied explicit endpoint fails fast;
the server never waits for another instance's lock.

## 2. Sequencing a test WITHOUT the plugin (viewer only)

```bash
cargo run -p haptic-server --release        # terminal 1
cargo run -p haptic-viewer --release        # terminal 2
```

In the viewer's bottom panel:

The console is arranged in compact stacked rows and fits the default 620 px
window width; enlarging the viewer should not be necessary to reach controls.

1. **▶ start test note** — plays a wave-stimulus note on MIDI channel 15.
2. **note / velocity / wave speed** sliders — changing one while a note
   sounds retriggers it on release (pitch and wave speed bind at note-on).
3. **Drag on the table** to move the source; the phase pattern re-forms
   around it. **orbit** circles the source automatically (period slider).
4. **Left-click a circle** → that logical channel plays on device output
   1 (L). **Right-click** → output 2 (R). Badges show current routing.
   Default routing is identity (output *n* ← channel *n*).

The engine always renders all 32 logical channels; the visualiser and its
level/phase data are taken *before* the copy to the physical device, so
every cell is live even on a stereo fallback device. Routing only selects
what you *hear*.

Colour code (per-note mode): hue = phase at that transducer relative to
the source oscillator (blue = in phase, hue rotates with phase lag);
brightness = local amplitude. The white cross is the source position.
The header shows note, frequency, wave speed, wavelength, render fps
(120 on a 120 Hz display) and status-message rate.

## 3. Sequencing a test WITHOUT the viewer (scripted / headless)

`tools/test_note.py` speaks the wire protocol directly:

```bash
python3 tools/test_note.py                          # 2 s middle-C note
python3 tools/test_note.py --note 48 --velocity 80 --duration 5
python3 tools/test_note.py --orbit                  # circle the source
python3 tools/test_note.py --wave-speed 100
python3 tools/test_note.py --route 0:31 --route 1:13  # monitor L←31, R←13
python3 tools/test_note.py --panic                  # just silence everything
python3 tools/test_note.py --socket /tmp/haptic-vst-test.sock
```

Protocol notes for writing your own clients:
- Frames are `u32` little-endian length + bincode payload, both directions
  (`haptic-protocol/src/lib.rs` is the schema; enum variant tags are
  `u32` LE in declaration order).
- Every connection must first send the exact-version `Hello` handshake. The
  checked-in script does this; copy its current schema rather than hardcoding an
  older enum layout.
- Controllers receive one `HelloAccepted` frame and must wait for it before
  reporting themselves connected; they receive no continuous status stream.
  Observers receive the acknowledgement plus levels (~60 Hz), active voices,
  layout, and routing, and must keep reading or they will be disconnected when
  their socket buffer fills.

## 4. Sequencing a test WITH the VST plugin

```bash
cargo run -p xtask -- bundle haptic-plugin --release
mkdir -p ~/Library/Audio/Plug-Ins/VST3
cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/
```

1. Start `haptic-server` (and the viewer, if wanted — multiple clients are
   fine and see the same state).
2. Load "Haptic Controller" (VST3 instrument) in the DAW. Its GUI shows
   connection and outgoing-MIDI diagnostics; whole-system visualisation is in
   `haptic-viewer`. The plugin reconnects automatically, so start order does not
   matter.
3. Play MIDI/MPE. Velocity → amplitude; pitch bend → source x (full
   table width); pressure → intensity; CC74/slide → source y (full table
   length). The **Wave Speed** and **Stimulus Type** plugin parameters are
   DAW-automatable and pushed to the server on change.
4. The viewer shows every active wave and standing voice, with per-instance
   filtering. Its phase display is a phase-aligned geometric preview; it does
   not reconstruct synchronized oscillator phase or delay-line history.

Plugin log: `/Users/jmz/tmp/log/haptic-vst.log` (override with `NIH_LOG`).

There is also a standalone plugin host (`cargo run -p
haptic-plugin-standalone`), but the DAW route is the reliable one (see
BUILD.md note on baseview stability).

To point the standalone client at an isolated headless server:

```bash
HAPTIC_SOCKET_PATH=/tmp/haptic-vst-test.sock \
  cargo run -p haptic-plugin-standalone --release -- --midi-input "DEVICE NAME"
```

## 5. Automated tests

```bash
cargo test --workspace
```

Covers: protocol framing and versioned handshakes, validation and disconnect
cleanup, engine note/voice lifecycle, propagation tails, short-delay phase,
MPE smoothing, reconstruction bounds, 48 kHz device-config preference,
per-transducer gains/layout reload, monitor routing, configuration parsing, and
end-to-end framed IPC over a real Unix socket.

Note: `cargo test` does **not** rebuild the `haptic-server` binary you run
manually — run `cargo build --release` before live tests, or you'll be
testing stale code.

## 6. Health checks

- Server prints a line every 5 s: callback count, frames, p50/p99/max
  callback time, stream errors. At 48 kHz the budget is ~10.7 ms per
  512-frame callback; release builds sit around p50 ≈ 2 ms.
- `--test-tone` for hardware bring-up: identifies each physical channel in
  turn without needing any client.
- Viewer header fps counter confirms the render rate on 120 Hz displays.

## 7. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `cargo not found` | See §0. |
| Viewer says "waiting for server" | Server not running, or it crashed — check its terminal. The viewer retries every 0.5 s. |
| Plugin "Disconnected" | The client retries automatically; verify the server is running and its protocol version matches. |
| Repeated `unexpected end of file` followed by `command before Hello` | The host has loaded a plugin predating the versioned handshake. Compare the editor's build hash with the log, rebuild the bundle, and fully restart the DAW so it unloads the old dynamic library. |
| No sound on stereo device | Only outputs 1–2 exist; route the channels you want to hear onto them (viewer left/right-click). |
| Server won't bind socket | Another live server owns `/tmp/haptic-vst.sock`; stop it first. A proven-stale socket is removed automatically. |
| Observer client disconnected mid-test | It stopped reading status broadcasts — see §3. |
| Viewer test note becomes a fixed sustained tone after a socket error | This was caused by observer write-side removal bypassing instance cleanup. Current builds buffer temporary backpressure and release viewer-owned voices on every terminal disconnect; rebuild/restart the server if this appears. |
| Sound on AirPods | No 32-channel device found; the server fell back to the default output device. |
