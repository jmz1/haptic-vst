# Haptic VST

Haptic VST is a research instrument for composing spatial vibration across a
32-transducer table. It turns MIDI and MPE performance into stimuli in the
roughly 20–200 Hz haptic range, while keeping the composition workflow inside a
DAW and the real-time multichannel engine in a standalone server.

The project is interested in the felt character of spatial vibration: moving
sources, changing phase relationships, interference, propagation, and the way
these can become compositional materials. Its motivating context is research
into haptic phenomenology, nervous-system regulation, health, and wellbeing.
Those aims are a direction for exploration rather than claims established by
the software.

## How the system is arranged

- **Haptic Controller** is a VST3 plugin. It receives MIDI/MPE and owns the
  automatable stimulus configuration for one DAW track.
- **Haptic** (`haptic-viewer`) is the primary interactive application. It
  observes and visualises the whole server, provides routing and test controls,
  and attaches to an existing server or starts and supervises one automatically.
- **haptic-server** remains a separate real-time process owning the engine and
  audio device. It always renders 32 logical channels, even when monitoring
  through a smaller device, and can still run independently or headlessly.
- **haptic-protocol** defines their versioned, framed Unix-socket protocol.

There are currently two stimulus types:

- **Wave** models a moving source using propagation delay lines. Motion creates
  Doppler pitch and amplitude behaviour through the delay model.
- **Travelling Wave (TW)** evaluates an instantaneous radial phasor. It has
  spatial wavelength and distance decay but no propagation history or Doppler.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the current system in detail.

## Start here

- [ROADMAP.md](ROADMAP.md) — current baseline, active priorities, and future
  research paths.
- [BUILD.md](BUILD.md) — build and bundle the workspace and VST3 plugin.
- [TESTING.md](TESTING.md) — run the unified application, standalone server,
  scripted client, headless tests, and DAW workflow.
- [docs/wave.md](docs/wave.md) and
  [docs/travelling-wave.md](docs/travelling-wave.md) — stimulus models and the
  engineering decisions behind them.
- [docs/composition.md](docs/composition.md) — the Ableton/MPE composition model
  and the emerging syllabary direction.
- [docs/learnings.md](docs/learnings.md) — technical lessons worth carrying
  forward from problems already solved.

The earlier architecture proposals in [docs/planning](docs/planning/README.md)
are frozen historical sources. They explain where the project came from, but
they do not describe current behaviour or priorities.

## Working principles

- Audio callbacks must not allocate, block, lock, perform I/O, or routinely
  log. Work on the real-time path must be fixed or explicitly bounded.
- The engine remains 32 logical channels regardless of the physical monitoring
  device; routing is the final copy to hardware.
- MIDI/MPE expresses the performance. VST parameters configure the stimulus
  emitted by one plugin instance.
- Protocol changes are coordinated across every client and protected by an
  exact-version handshake.
- Documentation describes the code that runs now. Completed issue narratives
  are removed after their durable lessons have been placed beside the relevant
  design or regression test.
