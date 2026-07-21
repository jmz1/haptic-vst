# Code-review remediation plan

*Started 2026-07-21 following a full workspace architecture and Rust review. This is the active implementation plan for hardening the current system before Phase E. `ROADMAP.md` remains the status source of truth; completed milestones here must be reflected there in the same session.*

## Objective

Preserve the current controller/server/viewer architecture and the verified Doppler work while making it safe to extend with more stimulus types. In particular:

- a client must not be able to leave indefinitely sounding voices behind;
- neither the server nor plugin audio callback may allocate, block, lock, or perform routine logging;
- protocol evolution must fail explicitly rather than being interpreted through incompatible bincode enum layouts;
- the low-speed delay model must retain in-flight energy and relative short-delay phase;
- the viewer must distinguish exact server state from geometric approximation;
- current documentation and automated quality gates must describe the code that actually runs.

The 32-channel-device search and default-device fallback remain intentional. Audio configuration should prefer 48 kHz on every selected device when the device exposes it, falling back to another supported rate only when necessary.

## Progress — 2026-07-21

- Milestone 1 core is implemented: protocol versioning, mandatory one-shot
  handshake, duplicate-live-ID rejection, numeric/range validation, retried
  disconnect cleanup, fixed registry reclamation, and command-ring capacity
  reserved for lifecycle traffic. Protocol v2 adds an explicit server
  acknowledgement, so clients report a connection only after registration is
  accepted. Server-issued IDs remain future protocol work.
- Milestone 2 callback work is substantially implemented: the plugin callback
  no longer reads wall time, allocates/formats diagnostic events, or locks
  diagnostic/configuration mutexes; delay-line reset is now generation-based;
  layout hot reload no longer deallocates a `Box` on the audio thread. A formal
  allocation-counting callback harness remains to add.
- Milestone 3 correctness fixes implemented: propagation tails drain after the
  source envelope ends, scatter lookahead preserves relative short delays,
  release begins at the current envelope level, non-MPE velocity is linear,
  and final device-rate samples are hard-clamped after reconstruction.
- Milestone 4 partially implemented: standing voices are included in snapshots,
  an explicit empty snapshot clears observers, and the viewer labels its field
  as a phase-aligned geometric preview rather than exact interference.
- Milestone 5 partially implemented: a second live server is refused, stale
  sockets are removed selectively, idle controllers detect a stopped server,
  failed viewer handshakes retry, observer status writes are bounded and
  resumable across partial writes/backpressure, and selected devices prefer
  48 kHz while retaining the default-device fallback.
  Headless/dummy runs now use a real-time-paced in-memory sink and isolated
  per-process socket namespace, with explicit socket overrides for test clients.
- All workspace Rust was formatted. Format, check, tests, and strict Clippy pass
  after this tranche.

## Milestone 1 — connection and protocol safety

1. Add a protocol version to `Hello` and reject incompatible or missing handshakes.
2. Require `Hello` before any other command and bind identity/role once per connection.
3. Validate all wire values before they enter the audio-thread queue:
   - finite, bounded MPE values;
   - finite wave speed clamped to the supported range;
   - MIDI note/channel/velocity bounds;
   - routing bounds.
4. Add `DisconnectInstance` to the internal engine command set. On socket loss, release or silence every voice owned by that connection and remove its configuration registry entry.
5. Prevent one connection from impersonating another active connection. Treat duplicate IDs as an explicit connection error until server-issued IDs are introduced.
6. Preserve critical lifecycle commands under load. Coalesce or drop MPE/config traffic before `NoteOff`, disconnect cleanup, or panic.
7. Add socket tests for pre-handshake commands, version mismatch, duplicate identity, invalid floats, disconnect cleanup, and queue-pressure behavior.

Exit criterion: killing or disconnecting any client leaves no indefinitely sustained voice, incompatible clients are rejected clearly, and invalid numeric input cannot produce non-finite audio.

## Milestone 2 — callback real-time safety

1. Remove `SystemTime`, `Vec<String>`, `format!`, mutex acquisition, and routine diagnostic logging from `HapticPlugin::process()`.
2. Publish plugin diagnostics through atomics and a bounded fixed-record queue consumed by the editor.
3. Move reconnect configuration publication off the callback or use a non-blocking snapshot mechanism.
4. Make wave-slot reset bounded. Avoid clearing roughly 2 MiB per note allocation and roughly 16 MiB on panic inside one callback.
5. Remove audio-thread deallocation during layout hot reload; use preallocated/copyable state or defer reclamation.
6. Add callback instrumentation/tests for allocation-free note bursts, panic, voice stealing, and 64-frame buffers.

Exit criterion: both audio callbacks have auditable, bounded work and contain no allocation, deallocation, locks, blocking calls, or formatting/logging.

## Milestone 3 — propagation and output correctness

1. Separate source-envelope activity from delay-tail activity. Continue consuming scheduled arrivals after note-off until the last possible emitted wavefront has passed.
2. Preserve relative short delays by adding a constant scatter-kernel lookahead instead of clamping all sub-kernel delays to one value.
3. Make release begin at the current envelope level so note-off during attack is continuous.
4. Decide and document the non-MPE velocity/pressure rule; default to linear velocity amplitude rather than unintentionally squaring velocity.
5. Put the final hardware safety bound after reconstruction, or prove and reserve sufficient reconstruction headroom.
6. Make delay capacity independent of unexpectedly high device rates, either by preferring 48 kHz or by using a fixed internal render rate with a general resampler.
7. Add regression tests for delayed release tails, short-delay phase separation, attack-time note-off, finite output, and final output bounds.

Exit criterion: the implemented propagation semantics match the design document across onset, motion, release, and reconstruction.

## Milestone 4 — truthful observation and state delivery

1. Include every stimulus type in active-voice/system-state reporting.
2. Stop describing geometric reconstruction as exact interference unless snapshots include enough synchronized phase/history information to make it exact.
3. Choose one explicit viewer contract:
   - approximate geometric field, visibly labelled; or
   - exact per-transducer complex/level snapshots from the engine.
4. Make routing and layout broadcasts derive from accepted engine state rather than optimistic IPC-thread mirrors.
5. Replace split best-effort layout queues with one versioned state publication so engine and observer cannot diverge.
6. Send an explicit empty active-voice snapshot rather than relying only on viewer timeouts.

Exit criterion: the viewer never presents an approximation as server truth and its configuration/routing display matches accepted engine state.

## Milestone 5 — transport scalability and operational robustness

1. Give each client a bounded read/decode budget per IPC iteration so one sender cannot starve other clients or status publication.
2. Decode incrementally while reading and replace front-of-`Vec` drains with a cursor/compaction strategy.
3. Make observer writes explicitly buffered; handle partial non-blocking writes without corrupting frames. ✅
4. Refuse a second live server instead of unconditionally unlinking its socket. Remove only a proven-stale socket.
5. Add configurable socket paths/server profiles if concurrent tables become a requirement. ✅ *(socket paths and isolated headless profiles implemented)*
6. Keep default-device fallback, but prefer a supported 48 kHz `f32` configuration and report clearly when another rate/format is selected.

Exit criterion: transport work is bounded per client, partial I/O is correct, and process startup cannot create a split-brain socket endpoint.

## Milestone 6 — structure for Phase E and the syllabary

1. Split the server monolith into voice lifecycle, commands, stimulus implementations, delay DSP, reconstruction, routing, and snapshots.
2. Extract the duplicated envelope and controller smoothing into reusable components.
3. Move shared numeric limits/schema metadata into the protocol or a dedicated shared model crate so plugin, viewer, and server cannot drift.
4. Define stable protocol discriminants and negotiated capabilities before adding syllable descriptors. Raise or redesign the 4 KiB frame budget if descriptors require it.
5. Reconcile dynamic server descriptors with VST3's stable host-parameter IDs. Choose a stable parameter superset, fixed generic slots, or separate plugin classes rather than attempting to add host parameters after connection.
6. Define unified voice/status capacity across wave, standing-wave, sweep, and future stimulus types.
7. Prototype standing-wave/phasor models offline before admitting them to the real-time engine.

Exit criterion: adding a stimulus does not require extending one monolithic file, changing unstable wire enum ordering, or inventing new lifecycle semantics.

## Continuous quality gates

Each completed milestone must:

- update `ROADMAP.md` and the affected current documentation;
- add tests at the layer where the defect was found;
- pass `cargo fmt --all -- --check`;
- pass `cargo check --workspace`;
- pass `cargo test --workspace`;
- pass `cargo clippy --workspace --all-targets --all-features -- -D warnings`;
- avoid editing `docs/planning/`;
- avoid physical hardware, GUI, DAW installation, or bundle installation unless explicitly requested.

The ignored orbit-capture harness remains the offline DSP evidence tool. It should be run deliberately when a propagation/reconstruction milestone changes sound, not as part of every unit-test invocation.
