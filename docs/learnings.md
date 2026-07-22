# Technical learnings

This is a selective record of lessons from solved problems that should shape
future work. It is not a changelog or issue archive. Each entry remains only
while its rule could prevent a different future failure; exact implementation
history is available in Git and regression tests.

## A connection is established only after application-level acceptance

Opening a Unix socket does not mean the server has accepted the client's
protocol version, role, identity, or configuration. Treating transport connect
as success caused incompatible or rejected clients to increment reconnect state
and send commands before registration.

**Carry forward:** connection state begins at `HelloAccepted`, not `connect(2)`.
Every protocol that binds identity should acknowledge that binding explicitly.
Controller state replay and note traffic follow acceptance.

**Protected by:** version mismatch, command-before-Hello, duplicate identity,
and handshake acknowledgement socket tests.

## Disconnect is part of voice lifecycle

A socket can disappear through read failure, terminal write failure, sustained
backpressure, process exit, or server shutdown. Cleanup attached to only one of
those paths leaves owned notes sounding.

**Carry forward:** every terminal client-removal path must converge on the same
retried `DisconnectInstance` operation. Lifecycle commands need capacity even
when ordinary control traffic is flooding the queue.

**Protected by:** disconnect/release integration tests and observer write-side
cleanup coverage.

## Nonblocking output needs resumable frames

`WouldBlock` is a normal condition on a nonblocking observer socket. Dropping
the client immediately makes transient pressure look like failure; retrying a
whole frame after a partial write corrupts framing.

**Carry forward:** keep a bounded per-client output buffer and a cursor into the
current frame. Resume after partial writes, drop or coalesce noncritical status
under a documented backlog policy, and disconnect only on terminal failure or
sustained inability to make progress.

Observers must also keep reading while connected; status is a continuous part
of the protocol rather than optional decoration.

## Moving-source delay is scheduled at emission time

A fixed write head with a variable interpolated read tap calculates delay at
reception time. That corresponds to a moving listener and can make a rapidly
approaching read tap overtake or reverse at the write head.

**Carry forward:** a moving source and fixed transducers require scatter writes
to fractional arrival positions `n + tau(n)` and sequential reads. Doppler
frequency and amplitude then emerge from arrival spacing; do not add a separate
Doppler multiplier.

**Protected by:** the scatter delay-line pitch/amplitude regression and the
opt-in moving-source orbit capture. See [`wave.md`](wave.md).

## Push resampling needs a bandlimited deposit

Two-tap linear scatter has gain that changes with fractional arrival phase. A
sweeping delay turns that periodic gain error into granulation and warble even
when its interpolation error appears small in a static sample test.

**Carry forward:** write-side/push resampling needs a phase-normalized
bandlimited deposit kernel. Test gain across fractional phases and relevant
in-band frequencies, not only DC or stationary delays.

The current eight-tap, 128-phase Kaiser-windowed sinc splat reduced measured
motion spurs by roughly 20 dB compared with the linear deposit; stationary
output remained near the f32 analysis floor.

## Capacity must be expressed in physical time

A delay buffer that looked large in samples covered only about 341 ms at 48
kHz. The 1×2 m table and low wave speeds exceeded that margin; clamped delay
then collapsed Doppler behaviour at predictable orbit positions.

**Carry forward:** derive delay capacity from maximum physical distance,
minimum wave speed, kernel lookahead, render rate, and release needs. Report or
test that time margin. The two-rate engine makes 16,384 cells cover about 10.9 s
at the preferred 1.5 kHz internal rate.

## Block-rate control steps become signal-rate artifacts

Viewer orbit positions and DAW control updates arrive at discrete cadences.
Applying targets directly at callback boundaries produced an FM sideband comb
at the update rate and made sound depend on buffer size/client cadence.

**Carry forward:** ramp targets over measured arrival spacing, then apply the
minimum additional smoothing needed by the model. Spatial phase parameters
should ramp in their canonical domain—wavenumber for TW—rather than stepping
derived controls.

**Protected by:** MPE cadence/smoothing tests and orbit capture analysis.

## Source lifetime and propagation-tail lifetime are different

Ending a Wave voice when its source envelope reaches zero discards energy that
was already emitted but has not arrived at distant transducers.

**Carry forward:** propagation stimuli track whether the source can emit and
whether scheduled energy remains separately. Note-off closes the source; the
slot is reaped only after the tail bound. Instantaneous models such as TW do not
inherit a fake tail merely for lifecycle uniformity.

## Safety bounds belong after reconstruction

An internal-rate sample can be within bounds while a reconstruction filter
overshoots between it and the device. Bounding only before the filter does not
guarantee bounded hardware output.

**Carry forward:** reserve model/mixer headroom and apply the final safety bound
after all reconstruction. Measure clamp incidence so protection does not become
an unnoticed normalizer.

The default transducer gain of 0.5 reserves headroom for up to 2× Wave arrival
bunching at the `0.5*c` source-speed limit.

## Logical output is independent of monitor hardware

Falling back to a stereo device once caused understandable confusion about
whether only two transducers were being simulated.

**Carry forward:** synthesis, levels, layout, and viewer state remain 32 logical
channels. Monitor routing is a final mapping from each available device output
to a selected logical channel. Device discovery must not resize the model.

## Observer geometry is not delayed audio truth

Current voice snapshots do not include synchronized oscillator phases or Wave
delay-line contents. Present source position and wavelength are insufficient to
reconstruct exact moving-source output or in-flight release energy.

**Carry forward:** label the viewer as a phase-aligned geometric preview. Claim
exactness only for quantities actually shared with the engine, such as a single
TW voice's relative phasor and decay.

## Build identity must outlive bundle timestamps

A VST3 bundle directory can retain an old wrapper timestamp, and a DAW can keep
an already loaded dynamic library resident after rebuilding the bundle. File
mtime alone cannot tell which code is running.

**Carry forward:** expose a deterministic content hash and protocol version in
the editor and plugin log. Use DAW-free headless/socket tests for most debugging;
reload the host only when validating actual host integration.

## Test processes need isolated ownership namespaces

Headless tests originally contended with the production server's singleton
socket even though they did not need physical hardware.

**Carry forward:** production keeps a fail-fast singleton endpoint; headless
runs default to a per-process endpoint and accept an explicit stable socket for
multi-process tests. An occupied endpoint is an error, never something to wait
on or unlink blindly.

## One application does not require one process

Launching the viewer and server separately imposed operational friction, but
putting egui and the real-time engine in one address space would remove useful
fault isolation without eliminating a measured bottleneck.

**Carry forward:** unify user-facing startup and lifecycle while retaining the
server process boundary. The GUI may attach to an external server or supervise
one child, but both cases use the same observer protocol. A managed child must
receive a parent-death signal that survives GUI crashes; the current inherited
stdin pipe turns EOF into normal server shutdown. Never terminate a server the
GUI did not start.

## MIDI naming and frequency are separate conventions

Ableton labels MIDI 60 as C3, while the equal-tempered frequency is still
261.6 Hz. Earlier hidden octave transposition made displayed notes and audible
test pitches disagree.

**Carry forward:** use Ableton octave names in the UI, standard MIDI frequency
without transposition in the engine, and an explicit 20–200 Hz clamp. Choose
test-note defaults directly for their desired frequency; MIDI 36 / Ableton C1
preserves the useful 65.4 Hz default without a hidden mapping.
