# Wave

Wave is the propagation-based stimulus in `haptic-server`. It models a
sinusoidal point source moving across the table and schedules each emitted
sample to arrive at every transducer after the appropriate distance-dependent
delay. Source motion changes the spacing of those arrivals, producing Doppler
frequency and amplitude behaviour as an outcome of the propagation model.

This document describes current behaviour and the engineering decisions that
protect it. Travelling Wave, the instantaneous alternative, is documented in
[`travelling-wave.md`](travelling-wave.md).

## What it is

For transducer `i` at position `x_i`, a source at `x_s(t)`, and wave speed `c`,
the physical delay of an emission is:

```text
tau_i(t) = |x_i - x_s(t)| / c
```

For a source signal `s(t)`, a fixed listener receives the sample emitted at
time `t` at arrival time:

```text
a_i(t) = t + tau_i(t)
```

If the source approaches a transducer, consecutive arrivals become closer
together. Frequency rises and energy is concentrated. If it recedes, arrivals
spread apart, frequency falls, and energy is diluted. There is no separate
“Doppler effect” stage in the engine; adding one would double-count behaviour
already present in the arrival schedule.

The current Wave voice combines:

- a standard-MIDI-frequency sinusoidal oscillator clamped to 20–200 Hz;
- attack, sustain, and release envelope state;
- velocity and smoothed MPE pressure amplitude;
- a latest-value MPE target in the XY plane;
- a persistent third-order XY motion controller with bounded jerk,
  acceleration, and wave-speed-relative velocity;
- configurable distance decay; and
- 32 independent fractional arrival buffers, one per transducer.

Wave speed and stimulus type are taken from the owning instance at note-on.
Changing Wave speed while a note is held does not rewrite that voice's existing
propagation history; the viewer test console retriggers held Wave notes when
speed changes to make this latching visible.

## Signal path

```text
MIDI/MPE
   │
   ▼
latest complete controller value
   │
   ├── pressure ramp/smoother ─┐
   └── raw XY target           │
             │                 │
             ▼                 │
  1.5 kHz third-order motion   │
  (bounded jerk/accel/speed)   │
             │                 ▼
oscillator × envelope × velocity/pressure
             │
             ▼
32 bandlimited scatter writes at emission-time distance and gain
             │
             ▼
32 sequential arrival reads
             │
             ▼
voice mixing → layout gains → sinc reconstruction → routing → device
```

The source oscillator advances at the engine's internal render rate. The delay
model carries emitted energy; note-off closes the source envelope but does not
discard arrivals already in flight.

## Scatter writes and sequential reads

For a moving source and fixed listener, delay must be evaluated at **emission
time**. Each emitted sample is therefore scattered into its future fractional
arrival position, and the buffer is read sequentially:

```text
read and clear                            scatter emission n
      ▼                                  near n + tau_i(n)
  ┌──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┐
  │  │  │  │  │  │▁▃▅█▅▃▁│  │  │  │  │
  └──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┘
      sequential time ──────────────────▶
```

The deposit uses a 16-tap Kaiser-windowed sinc splat selected from 1024
fractional phases. Each phase is normalized to unit DC gain. The 64 KiB table
is precomputed on the heap and borrowed directly by the callback. A constant
half-kernel lookahead is included in every scheduled delay so all taps land in
unread cells and nearby transducers retain distinct short delays.

Every delay cell also carries a generation tag. Reset, voice stealing, and
panic advance the generation instead of clearing the large buffers inside the
audio callback. A cell from an older generation behaves as zero when read or
accumulated.

The read pointer advances by exactly one internal sample each frame, consumes
the accumulated arrival, and logically clears that cell. Delays beyond the
available capacity are clamped rather than wrapped; wrapping would lap the read
pointer and turn a long physical delay into a false short one.

## Source motion and causality

The plugin/viewer sends discrete MPE updates, usually quantised to client and
audio-block cadence. Wave treats these as samples of a deliberately
lower-bandwidth spatial control: multiple updates drained at one callback
boundary simply replace the latest XY target and never reset motion state.

Each voice owns persistent two-dimensional position, velocity, acceleration,
and jerk state. At every internal render frame a three-pole critically damped
controller advances that state toward the latest target. Its natural frequency
is 1.5 Hz: nine times the default orbit frequency, retaining about 98% of its
radius while rejecting callback/update images. The controller bounds vector
jerk and acceleration, then limits Euclidean velocity to half the configured
wave speed. Limits apply to vector magnitude rather than independently per
axis, so diagonal motion cannot exceed the causal speed ceiling.

The approximately 0.32-second low-frequency motion lag is intentional. The
requested-position ring remains the latest controller target; the effective
source cross shows the filtered physical source. Pressure continues through
the general MPE amplitude ramp and smoother independently of XY motion.

The final limit is a causality/stability condition for scatter scheduling, not
merely aesthetic smoothing. With radial speed bounded by `0.5*c`, the arrival
index advances within approximately `[0.5, 1.5]` cells per emission. Successive
arrivals remain monotonic and do not leave pathological gaps or reverse order.

Because every transducer distance derives from the one controlled XY position,
`|d_i/dt| <= |velocity| <= 0.5*c` for all 32 outputs. Per-channel motion
controllers would not preserve a coherent point source or this automatic
radial bound. The before/after frequency-domain evidence is in
[`wave-orbit-dsp-analysis.md`](wave-orbit-dsp-analysis.md).

The viewer shows the requested position as a ring and the effective source as a
cross, joined while the source is catching up.

## Distance decay and headroom

Wave and TW share the radial distance gain:

```text
g(d) = (1 + d / d0)^(-p)
```

Defaults are `d0 = 2 m` and `p = 1`, giving `1 / (1 + d/2)`.
`p = 0` deliberately disables falloff.

For Wave, gain is evaluated at emission time and carried into the arrival
buffer. A live decay change affects new emissions, not energy already in
flight. This differs from TW, whose instantaneous output uses the latest
smoothed decay directly.

Approach at the maximum `0.5*c` source speed can bunch arrivals by as much as
`1 / (1 - 0.5) = 2`. The default per-transducer layout gain is therefore 0.5,
reserving 6 dB for that physically meaningful amplification before final
output bounds. Explicit layout gains can change the policy and must be tested
with maximum-occupancy material.

## Two-rate rendering and reconstruction

Delay lines run at `device_rate / 32`. At a preferred 48 kHz device rate this is
1.5 kHz, comfortably above twice the 200 Hz stimulus ceiling. The lower internal
rate makes long physical delays inexpensive: the 34,000-cell buffers cover
about 22.7 seconds at 1.5 kHz, enough for the default table diagonal at the
0.1 m/s control floor.

Device-rate samples are reconstructed with a 512-tap polyphase
Kaiser-windowed sinc filter, 16 taps per interpolation phase. The filter both
interpolates and suppresses images above the internal Nyquist. Its group delay
is operational latency shared by all channels, not a spatial propagation
difference.

Final samples are bounded after reconstruction. Bounding only internal frames
would not protect against reconstruction overshoot.

## Voice lifetime

A Wave voice has two related lifetimes:

- **source lifetime:** the envelope can still emit new energy; and
- **tail lifetime:** scheduled arrivals may still remain in the buffers.

Note-off begins release from the envelope's current value, including during
attack. Once the source becomes silent, the voice continues sequential reads
until its latest possible arrival has passed. Only then may the slot and owner
be reaped. Panic is intentionally different: it invalidates generations and
ownership immediately.

Disconnect releases the owning instance's voices through the same lifecycle
machinery, so closing a viewer or plugin connection cannot leave a permanent
sustain.

## Output observation

The viewer does not attempt to reproduce the 32 delay-line histories from
voice metadata. The server measures the final summed Wave/TW output after
device-rate reconstruction and safety bounding, then publishes its 32-channel
Hilbert analytic signal with synchronized source-oscillator references.

Consequently moving-source history, Doppler behaviour, interference, release
energy, reconstruction response, and bounding are all present in the displayed
field. Voice position and configuration remain observer metadata only for
reference selection, labels, and source cursors. A selected Wave oscillator
continues advancing until the measured propagation and filter tails are silent.

## Why it works this way

Several completed investigations established the current design:

- A fixed write head with an interpolated read tap evaluates delay at reception
  time. That is a moving-listener model and can make the tap overtake or reverse
  near the write head. Moving-source physics requires emission-time scatter
  writes.
- A two-tap linear scatter kernel has fractional-phase-dependent gain. Sweeping
  delay turns that into audible granulation/warble. The bandlimited normalized
  sinc splat keeps in-band gain nearly constant across phases.
- Delay capacity expressed at the device rate was too short for a 1×2 m table
  at low wave speeds. Internal-rate delay storage turns capacity into a
  physical-time margin large enough for realistic layouts.
- Directly stepping block-rate MPE targets creates an FM sideband comb. A
  persistent, low-bandwidth XY controller must advance on the synthesis clock;
  callback inputs update its destination rather than its state or ramp timing.
- Discarding a voice when its envelope ends loses physically emitted release
  energy. Source and tail activity must be tracked separately.

Representative numerical evidence and the regression tests protecting these
lessons are summarised in [`learnings.md`](learnings.md). The opt-in orbit
capture in the engine is the deeper evidence tool when this path changes.

## Open edges

- The source is a point in a direct, unbounded medium. Physical table
  boundaries, reflections, dispersion, and modes are not represented.
- The internal render rate is derived from the device rate rather than fixed at
  a universal rate with a general resampler. Device selection prefers 48 kHz to
  keep the tested operating point stable.
- Multiple Wave voices sum before the final bound; there is no perceptual
  loudness normalization or automatic polyphonic headroom policy.
- MPE pitch bend is spatial x, so note frequency is latched. Continuous pitch
  would require an explicit future binding rather than an implicit change to
  this stimulus.
