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
save) and `--test-tone` (100 Hz burst cycling across all outputs, for
hardware bring-up).

## 2. Sequencing a test WITHOUT the plugin (viewer only)

```bash
cargo run -p haptic-server --release        # terminal 1
cargo run -p haptic-viewer --release        # terminal 2
```

In the viewer's bottom panel:

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
```

Protocol notes for writing your own clients:
- Frames are `u32` little-endian length + bincode payload, both directions
  (`haptic-protocol/src/lib.rs` is the schema; enum variant tags are
  `u32` LE in declaration order).
- **Clients must keep reading**: the server broadcasts status (levels
  ~60 Hz, voice state ~90–250 Hz) to *every* client and drops any whose
  socket buffer fills. A send-only script will be disconnected mid-test.

## 4. Sequencing a test WITH the VST plugin

```bash
cargo run -p xtask -- bundle haptic-plugin --release
mkdir -p ~/Library/Audio/Plug-Ins/VST3
cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/
```

1. Start `haptic-server` (and the viewer, if wanted — multiple clients are
   fine and see the same state).
2. Load "Haptic Controller" (VST3 instrument) in the DAW; the GUI shows
   connection status and the live level grid. The plugin connects at load
   time, so **start the server first** (or reload the plugin after).
3. Play MIDI/MPE. Velocity → amplitude; pitch bend → source x (full
   table width); pressure → intensity; CC74/slide → source y (full table
   length). The **Wave Speed** and **Stimulus Type** plugin parameters are
   DAW-automatable and pushed to the server on change.
4. The viewer shows the most recent delay-line voice, whoever started it —
   plugin notes and viewer test notes are the same to the server.

Plugin log: `/Users/jmz/tmp/log/haptic-vst.log` (override with `NIH_LOG`).

There is also a standalone plugin host (`cargo run -p
haptic-plugin-standalone`), but the DAW route is the reliable one (see
BUILD.md note on baseview stability).

## 5. Automated tests

```bash
cargo test --workspace
```

Covers: protocol framing (round-trip, coalesced/fragmented/corrupt/
oversized frames), engine note lifecycle (note-off routing, voice
stealing, MPE smoothing and channel isolation, panic), haptic frequency
mapping, per-transducer gains and layout hot-swap, long-delay correctness
(far transducers stay silent until the wave arrives), monitor routing to
physical outputs, config parsing/validation, and an end-to-end IPC test
over a real Unix socket.

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
| Plugin "Disconnected" | Server started after the plugin loaded — reload the plugin (or restart the DAW's audio engine). |
| No sound on stereo device | Only outputs 1–2 exist; route the channels you want to hear onto them (viewer left/right-click). |
| Server won't bind socket | Stale `/tmp/haptic-vst.sock` after a hard kill: `rm /tmp/haptic-vst.sock`. Clean shutdown is Ctrl-C. |
| Script client disconnected mid-test | It stopped reading status broadcasts — see §3. |
| Sound on AirPods | No 32-channel device found; the server fell back to the default output device. |
