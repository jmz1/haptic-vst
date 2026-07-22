# Travelling Wave

Travelling Wave (`tw`/`TW`) is the instantaneous radial phase-field stimulus.
It has a movable source, wavelength, and distance decay like Wave, but it does
not model propagation history. Every internal frame evaluates the field at the
source's current smoothed position, so spatial automation begins taking effect
immediately and no energy remains in flight.

The only runtime stimulus types are Wave and Travelling Wave. The old in-phase
“Standing” placeholder was removed; its stable second VST enum slot deliberately
migrates to TW for old session automation.

## What it is

For transducer `i` at distance `d_i` from the source:

```text
theta[n + 1] = theta[n] + 2*pi*f*dt

k = 2*pi*f/c          in speed mode
  = 2*pi/lambda       in fixed-wavelength mode

g(d) = (1 + d/d0)^(-p)

y_i[n] = A[n] * pressure[n] * g(d_i) * sin(theta[n] - k*d_i)
```

`theta` is one source oscillator shared across the voice. `k` is the spatial
wavenumber; it determines how phase rotates with distance. `g(d)` is shared
with Wave so the two stimuli can be compared without an accidental attenuation
change.

TW has:

- standard MIDI frequency clamped to 20–200 Hz;
- velocity, pressure, attack, and release behaviour shared with Wave;
- source x/y controlled by smoothed MPE bend and timbre;
- speed or fixed-wavelength spatial-scale modes;
- configurable distance-decay knee `d0` and exponent `p`; and
- live spatial-scale and decay automation for held voices.

It deliberately has no delay lines, scheduled arrivals, Doppler frequency or
amplitude gain, source-speed limit, or propagation tail.

## Speed and wavelength

The two authoring modes describe the same instantaneous phase field from
different directions.

In **Speed** mode:

```text
lambda = c / f
```

The configured speed is constant across notes, so higher notes have shorter
wavelengths and tighter spatial phase rotation.

In **Wavelength** mode:

```text
c_effective = f * lambda
```

The configured wavelength is constant across notes, so different pitches keep
the same spatial pattern while their displayed effective speed changes.

The engine treats wavenumber `k` as canonical live state. Ramping `k`, rather
than switching or interpolating raw speed/wavelength independently, produces a
continuous spatial phase pattern when either control or mode changes.

Shared ranges are defined in `haptic-protocol`:

| Control | Range | Default |
|---|---:|---:|
| speed | 0.25–1000 m/s | 20 m/s |
| wavelength | 0.00125–50 m | 0.2 m |
| decay knee `d0` | 0.01–10 m | 0.5 m |
| decay exponent `p` | 0–4 | 1 |

The broad wavelength range is experimental, not a promise that every layout
can spatially sample it. Wavelengths below roughly twice the local transducer
spacing will alias as a physical sampled field; a future viewer warning should
derive from the accepted layout rather than silently changing the synthesis.

## Control timing

TW distinguishes note selection from live field automation:

- Changing stimulus type affects new notes and retriggers the viewer's test
  note when necessary.
- A note starts at the instance's current scale without an onset glide.
- Speed, wavelength, scale mode, and decay updates apply to active TW voices
  belonging to that instance.
- Wavenumber and decay move toward their targets using bounded
  measured-spacing ramps, avoiding block-rate phase steps.
- Position and pressure retain the shared MPE target interpolation and
  smoothing.
- The effective TW source follows the smoothed position directly. It does not
  use Wave's `0.5*c` arrival-ordering limit.

“Immediate” therefore means the next internal render frame, subject to the
documented ramps and controller smoothing. It does not mean a distance-based
wait, note retrigger, or reset of oscillator phase.

## Distance decay

The shared gain law is:

```text
g(d) = (1 + d/d0)^(-p)
```

At the defaults `d0 = 0.5 m` and `p = 1`, this is exactly
`1 / (1 + 2d)`. Setting `p = 0` produces no distance falloff.

Because TW has no stored history, live decay affects current output directly as
the ramp advances. In Wave, the same parameter applies at emission time and
cannot alter energy already scheduled in its delay lines.

## Engine implementation

The server owns a fixed `StimulusPool<TravellingWaveStimulus, 8>` and a parallel
owner table. A TW voice contains only fixed scalar/component state:

- oscillator and envelope;
- MPE target ramp and smoothers;
- current source position;
- current/target wavenumber;
- current/target decay; and
- configuration mode and effective scale.

It performs at most one direct radial phasor evaluation per transducer per
internal frame. At maximum occupancy that is eight voices × 32 logical
transducers. It allocates no delay storage and shares the server's ordinary
voice allocation, stealing, note-off, MPE, disconnect, panic, mixing,
reconstruction, routing, and snapshot lifecycle.

When the envelope becomes inactive, the TW voice can be reaped immediately:
there are no future arrivals to drain.

## Plugin and reconnect behaviour

VST3 parameter IDs remain registered and stable whether or not the currently
selected stimulus uses them. The editor de-emphasizes inactive controls rather
than changing the host parameter list dynamically.

The plugin's reconnect state is a sequence-checked multi-atomic snapshot. It
stores the top-level Wave configuration and nested TW mode/wavelength/decay
configuration coherently and prevents the worker from observing a torn patch,
without adding a callback lock. The current single host-visible Wave Speed
parameter is copied into both Wave speed and TW speed-mode configuration.

The legacy second value of the existing `stim_type` parameter now means
Travelling Wave. This is an intentional semantic migration from a placeholder,
not a retained standing-wave compatibility engine.

## Viewer contract

For a TW voice the viewer uses the same shared relative phasor helper as the
engine:

```text
phasor_i = amplitude * g(d_i) * exp(-j*2*pi*d_i/lambda)
```

It receives the effective interpolated wavelength and decay in `VoiceInfo`, so
it need not reconstruct them from stale configuration. Relative phase and gain
for a single voice can therefore match the model closely.

Snapshots still omit oscillator phase. When several voices are displayed, the
viewer phase-aligns their source oscillators before summing; this shows spatial
relationships and possible interference but not the engine's exact
instantaneous output. The overall display remains labelled as a geometric
preview.

Requested and effective cursors normally coincide after ordinary smoothing,
because TW has no wave-speed-dependent source chase.

## Why it works this way

- A phase field and a propagation simulation answer different compositional
  questions. Giving TW its own name and explicit lack of history avoids
  pretending the two models differ only by an optimization.
- Canonical wavenumber makes speed/wavelength mode changes continuous in the
  quantity that directly controls spatial phase.
- Shared decay keeps comparisons with Wave meaningful and places the control in
  the protocol rather than duplicating constants across engine and UIs.
- Stable inactive VST parameters respect DAW automation identity; a plugin
  cannot safely add arbitrary host parameters after connection.
- Reusing the established fixed voice lifecycle is more valuable than making a
  separate “simple oscillator” path that could forget disconnect, stealing,
  or snapshot behaviour.

## Validation

Current tests cover:

- protocol round trips, validation, and maximum status-frame size;
- default decay equivalence and representative closed-form phasors;
- speed and fixed-wavelength relationships;
- live wavenumber and decay changes without retrigger;
- note-off, stealing, disconnect, panic, and owner reaping;
- finite bounded 32-channel output;
- viewer-compatible relative phase; and
- headless framed-socket use through the scripted client.

The implemented release headless smoke test exercised a maximum-size logical
configuration at 48 kHz without opening a DAW or physical audio device. Future
changes should prefer this path before manual DAW reload testing.

## Open edges

- Short wavelengths can spatially alias against the physical layout.
- Eight simultaneous TW voices add significant worst-case coherent sum; the
  final bound is protection, not a complete mixer headroom policy.
- Oscillator phase is not observer-visible, so exact cross-voice interference
  cannot be displayed.
- Continuous pitch is not currently a TW expression dimension. If introduced,
  speed mode would vary wavelength while wavelength mode would vary effective
  speed; the wavenumber model already supports either relationship.
- Boundaries and reflections are outside this stimulus. A future modal or
  reflected model should be a new, explicitly specified type.
