# Wave orbit DSP analysis

This records the reproducible baseline and first remediation for the Wave test
console's default moving source, measured on 2026-07-22. It separates intended
moving-source physics from control-rate and fractional-delay artefacts. The XY
motion controller described below is now production behaviour; alternative
scatter-kernel sizes remain temporary diagnostic builds and were reverted after
capture.

The original reported artefact was real and had two independent software
causes:

1. The 8 ms viewer trajectory and a 512-frame, 48 kHz audio callback interact
   badly. Multiple MPE targets may be drained before any synthesis sample is
   rendered, resetting the interpolator and creating images of the Doppler
   sweep inside the 20–200 Hz operating band.
2. The current 8-tap, 128-phase scatter kernel adds a separate narrow spur near
   449 Hz. More taps suppress that spur; more fractional phases suppress a
   different family near and above the 750 Hz internal Nyquist.

There is also a deterministic start transient: a note starts at the table
centre, then the first orbit target is 0.35 m away. The effective source spends
time chasing that discontinuity. The new acceleration/jerk limits make the
chase smooth, but pre-positioning an already-enabled orbit remains preferable.

## Reproduction and measurement

The capture uses the viewer test-tone defaults:

| Parameter | Value |
|---|---:|
| stimulus | Wave |
| note | MIDI 36 / Ableton C1 / 65.406 Hz |
| velocity / pressure | 100 / 1.0 |
| wave speed `c` | 1.0 m/s |
| table | 1.0 × 2.0 m, default 4 × 8 layout |
| orbit centre / radius | (0.5, 1.0) m / 0.35 m |
| orbit period | 6.0 s |
| decay | `d0 = 0.5 m`, `p = 1` |
| device / internal rate | 48 kHz / 1.5 kHz |
| callback / nominal MPE interval | 512 frames / 8 ms |

The ignored engine capture drove the real `StimulusEngine` into a dummy
32-channel output buffer. The default file contains 623,616 interleaved
little-endian `f32` frames (12.992 seconds, 79,822,848 bytes). It had SHA-256
`89c85e47266fd4f3e75aaac3e7004ee0907ac3381860531723f2c5073b3ab180`
in this run. Raw captures live under ignored `target/` directories and are not
repository artefacts.

The dependency-free analyzer in `tools/analyze_f32_capture.rs` used a
262,144-sample Hann-window FFT per channel, beginning at 2 seconds unless a
result explicitly includes onset. Resolution was 0.183105 Hz. Band powers are
the sum across all 32 channel spectra and are reported in dB relative to total
captured spectral power. These are energy ratios, not the level of one selected
FFT bin.

The default capture contained no NaN, infinity, denormal, or clamped sample.
Its largest sample was 0.624607 on logical channel 22 at 1.119729 seconds,
during the initial source chase. After 2 seconds the largest channel peak was
0.519846. Steady per-channel RMS ranged from approximately 0.097 to 0.169.
Hard limiting is therefore not responsible for the observed spectrum.

## Intended spectrum

For emission time `t`, transducer distance `d(t)`, and wave speed `c`, arrival
time is:

```text
a(t) = t + d(t) / c
```

The arrival-time Jacobian gives the received instantaneous frequency and the
ideal bunching gain:

```text
f_received = f0 / (1 + d'(t) / c)
bunching gain = 1 / (1 + d'(t) / c)
```

The default orbit's tangential speed and centripetal acceleration are:

```text
v = 2*pi*0.35/6 = 0.366519 m/s
a = v^2/0.35     = 0.383818 m/s^2
```

Across the actual 32 transducer coordinates, radial velocity reaches both
`-0.366519` and `+0.366519 m/s`. The physically expected steady frequency
range is consequently 47.864–103.249 Hz. Distance spans 0.045285–1.301972 m,
corresponding to 45 ms–1.302 s of physical delay and default distance gains of
approximately 0.917–0.277. None of these delays approaches the 10.9-second
delay-line capacity.

The broad, periodically moving band between about 48 and 103 Hz is therefore
intentional Doppler modulation, not an artefact. Its orbit-period sidebands are
spaced by `1/6 Hz`. At the source speed limiter (`0.5*c`), the absolute bounds
for this note become 43.604–130.813 Hz. Energy outside the applicable bounds,
and mirrored copies of the whole sweep, are implementation artefacts.

## Control/callback interaction

At 48 kHz, a 512-frame callback occurs every 10.6667 ms, or 93.75 times per
second. A nominal 8 ms source stream is 125 updates per second. The callback
therefore receives a repeating mixture of one and two MPE updates. Commands are
all drained before rendering begins. When two updates are present,
`MpeInterp::update()` is called twice with no intervening `step()`:

- the intermediate target is never rendered;
- the second call sees zero elapsed synthesis time and selects the 5 ms minimum
  ramp; and
- target step and ramp duration acquire a periodic callback-rate pattern.

That pattern images the 48–103 Hz sweep around the 93.75 Hz callback rate. The
most obvious upper image occupies roughly 142–197 Hz, and a corresponding
folded image appears below about 46 Hz. The strongest aggregate upper-image bin
in the default capture was 151.245 Hz.

The original ramp-plus-chase implementation measured:

| Original scenario | `<45 Hz` | `110–200 Hz` | `200–750 Hz` | `750–2000 Hz` |
|---|---:|---:|---:|---:|
| default: 512 frames, 8 ms | -45.7 dB | -29.9 dB | -37.0 dB | -64.7 dB |
| 120 Hz-like client, 8.333 ms | -39.2 dB | -25.8 dB | -37.0 dB | -64.6 dB |
| 60 Hz-like client, 16.667 ms | -30.9 dB | -19.2 dB | -36.4 dB | -64.7 dB |
| 256 frames, 8 ms | -79.9 dB | -41.8 dB | -37.0 dB | -64.6 dB |
| 64 frames, 8 ms | -83.8 dB | -66.1 dB | -37.7 dB | -64.6 dB |
| 512 frames, callback-aligned 10.667 ms | -83.7 dB | -66.0 dB | -37.7 dB | -64.7 dB |
| stationary source | -125.6 dB | -140.9 dB | -140.0 dB | -108.2 dB |

For the steady default orbit, `<45 Hz` and `110–200 Hz` are both outside the
physical 47.864–103.249 Hz range, so they are useful control-image indicators.
The 60 and 120 Hz rows model the viewer's vsync-dependent update cadence; real
frame-time jitter can make the sequence less regular still.

Two controls make the cause particularly clear. A stationary source is clean,
and changing only the callback/MPE relationship removes about 36 dB of upper
in-band image energy without changing the delay algorithm. A regular 32 ms
update on exactly three 512-frame callbacks also measured -64.4 dB in the
110–200 Hz band, whereas an inadequately slow 64 ms stream measured -18.2 dB.
Both cadence regularity and a sufficient trajectory rate matter.

### Implemented XY motion controller

Wave no longer derives XY position from the measured-spacing MPE ramp. Each
voice now retains the latest complete XY target and owns persistent position,
velocity, acceleration, and jerk state. Multiple MPE commands drained at one
callback overwrite only that target; they cannot reset controller state.

The controller advances at every 1.5 kHz internal frame. Its unconstrained
response has three coincident real poles at 1.5 Hz:

```text
x''' + 3*omega*x'' + 3*omega^2*x' + omega^3*x = omega^3*target
omega = 2*pi*1.5 rad/s
```

Jerk and acceleration are bounded as vectors. Euclidean speed retains the
existing `0.5*c` ceiling, so one coherent source position automatically bounds
all transducer radial velocities. The 1.5 Hz bandwidth is nine times the
six-second orbit rate. In the linear approximation it retains about 98% of the
orbit radius and adds approximately 0.32 seconds of low-frequency lag; capture
measured an effective steady speed near 0.360 m/s versus the requested
0.3665 m/s.

With the current 8-tap/128-phase scatter unchanged, the same analysis measured:

| Current scenario | `<45 Hz` | `110–200 Hz` | `200–750 Hz` | `750–2000 Hz` |
|---|---:|---:|---:|---:|
| 512 frames, 8 ms | -84.4 dB | -65.9 dB | -38.3 dB | -64.7 dB |
| 512 frames, 120 Hz-like client | -80.6 dB | -65.4 dB | -38.3 dB | -64.8 dB |
| 512 frames, 60 Hz-like client | -72.5 dB | -61.2 dB | -38.3 dB | -64.8 dB |
| 64 frames, 8 ms | -84.6 dB | -65.9 dB | -38.3 dB | -64.7 dB |
| stationary source | -125.6 dB | -140.9 dB | -140.0 dB | -108.2 dB |

The former callback image improved by 35.2 dB in the default 110–200 Hz band,
and 512- versus 64-frame steady results now differ by only 0.02 dB there. The
60 Hz-like input is the worst measured cadence and remains below the -60 dB
acceptance threshold. A deterministic unit test separately limits RMS and peak
XY trajectory differences between 64- and 512-frame partitions.

The unchanged approximately 449 Hz scatter spur now dominates the remaining
software artefact. This is why the `200–750 Hz` figures do not improve with the
motion controller and why scatter remediation remains a separate task.

## Start transient

The test console retains `(0.5, 1.0)` as its resting source. If orbit is enabled
when the note starts, the note-on snaps the effective source to that centre;
the next UI update requests approximately `(0.85, 1.0)`. This is not a point on
a continuous six-second trajectory from the note-on position.

In the original implementation the speed limiter turned the 0.35 m
discontinuity into a maximum-speed chase. Analysis from time zero measured a
0.624607 peak at 1.12 seconds and much more energy outside the steady orbit
band. With the current XY controller, analysis from time zero measured a
0.490402 peak, -72.9 dB below 45 Hz, and -65.8 dB in 110–200 Hz. The remaining
centre-to-orbit convergence is a smooth, visible motion lag rather than a
spectral transient. Pre-positioning or easing the test orbit may still be a
better interaction, but is no longer required to protect the DSP path.

## Scatter and reconstruction

Once the control image was isolated with a 64-frame callback, a narrow
448.792 Hz spur remained. It follows moving delay and disappears for the
stationary source. Temporary kernel builds produced:

| Scatter kernel | `110–200 Hz` | `200–750 Hz` | `750–2000 Hz` |
|---|---:|---:|---:|
| 8 taps, 128 phases (current) | -66.1 dB | -37.7 dB | -64.6 dB |
| 16 taps, 128 phases | -66.1 dB | -49.7 dB | -64.4 dB |
| 32 taps, 128 phases | -66.1 dB | -49.4 dB | -63.8 dB |
| 8 taps, 1024 phases | -82.7 dB | -37.9 dB | -82.7 dB |
| 16 taps, 1024 phases | -82.9 dB | -67.7 dB | -82.4 dB |

The tap and phase changes address different errors. Sixteen taps materially
reduce the sub-Nyquist resampling image; 32 taps provide no further gain with
the current full-band Kaiser design. Increasing phase resolution removes the
phase-quantisation family near and above the 750 Hz internal Nyquist. Combined,
16 taps and 1024 phases give the best captured result.

The device-rate reconstruction FIR is not the primary source. The stationary
65.4 Hz control leaves its first 1.5 kHz-rate images near 1435 and 1565 Hz at
roughly -121 and -108 dB in the selected-channel peak list. At 96 kHz device
rate the internal rate doubles to 3 kHz and the moving-source scatter spur
moves upward (the strongest broad aggregate component was near 851 Hz), while
the callback-duration-dependent control image remains. This distinguishes
internal scatter error from device-rate FIR imaging.

The 16-tap/1024-phase experiment is not a drop-in constant change without a
small implementation review. It doubles scatter deposits and expands the
kernel from 4 KiB to 64 KiB. `process_block()` currently copies the kernel into
callback stack storage to simplify borrowing; that copy and stack footprint
should be removed before adopting the larger table.

## Parameter sensitivity

The controlling dimensionless quantities are `v/c` for Doppler/time warping
and `2*pi*f0*delta_x/c` for propagation-phase error caused by a position step.
At the default 8 ms interval, the source moves 2.93 mm, which is about 1.20
radians (69 degrees) of worst-case propagation phase at 65.4 Hz and `c=1`.
A 60 Hz UI step is about 6.11 mm or 2.51 radians (144 degrees). Losing or
collapsing one such target is not a small perturbation.

The original ramp-plus-chase parameter sweep measured:

| Original change from default | `110–200 Hz` | `200–750 Hz` | Interpretation |
|---|---:|---:|---|
| wave speed 2 m/s | -39.2 dB | -49.9 dB | halves `v/c` and phase sensitivity |
| wave speed 5 m/s | -58.0 dB | -49.9 dB | control image becomes small |
| wave speed 20 m/s | -64.3 dB | -50.1 dB | close to a stationary-delay regime |
| orbit period 12 s | -40.5 dB | -49.7 dB | halves source speed |
| orbit radius 0.10 m | -55.0 dB | -50.0 dB | reduces speed and phase excursion |
| decay exponent 0 | -30.2 dB | -37.7 dB | nearly unchanged: decay AM is not the cause |

A three-second orbit requests 0.733 m/s, beyond the `0.5*c` limit. The source
then follows a capped chase rather than the requested circle; its 110–131 Hz
content is partly intended, but energy above 131 Hz and the -28.9 dB
`200–750 Hz` band still expose control/scatter images.

Pitch changes both the valid Doppler band and sensitivity to position error.
MIDI 48 / C2 has `f0 = 130.8 Hz`; even the default steady `v/c` can raise it to
about 206.5 Hz. The engine clamps the source oscillator to 200 Hz but does not
and should not silently remove propagation-created Doppler content. If the
physical system must remain below 200 Hz, Wave note validation needs a
Doppler-aware policy; clamping only `f0` is insufficient. Under the `0.5*c`
motion limit, `f0 <= 100 Hz` is the conservative guarantee.

The default wavelength is only `c/f0 = 15.29 mm`, while adjacent transducers
are 250 mm apart (16.35 wavelengths). This does not create temporal aliasing in
the independent 32-channel renderer, but it means the table is extremely
sparsely sampled spatially. The viewer cannot imply a smoothly sampled field,
and real-table modes, transducer bandwidth, mounting, phase response, and
mechanical coupling may dominate what is felt. Those require microphone,
accelerometer, or electrical loopback capture; this software-only analysis
cannot characterize them.

## Recommended remediation and acceptance tests

Remaining work should proceed in this order:

1. Evaluate the 16-tap/1024-phase scatter configuration after removing the
   callback stack copy. Measure callback cost at maximum Wave polyphony before
   adopting it.
2. Pre-position an already-enabled test orbit before note-on and ease orbit
   entry when toggled during a held note. This is a UI gesture issue rather
   than a reason to generate general motion server-side.
3. Expand the deterministic spectral regression checks. Include
   stationary, onset, steady orbit, 60/120 Hz client cadence, 64–512 and
   variable callback partitions, 44.1/48/96 kHz device rates, speed/radius/
   period extremes, and Doppler-safe versus Doppler-over-band notes.

For the default steady orbit, the first software acceptance target is
aggregate energy below -60 dB relative to total outside 45–110 Hz, with no
single mirrored sweep visible above that floor. The XY controller now meets the
target for the control-sensitive lower and upper bands across tested callback
and client cadences. The diagnostic 16-tap/1024-phase build reached -82.9 dB in
110–200 Hz, -67.7 dB in 200–750 Hz, and -82.4 dB in 750–2000 Hz when control
delivery was regular, showing the remaining scatter target is also attainable.
Onset should be evaluated against its wider 43.6–130.8 Hz cap-bound range.

Raw FFT ratios should remain diagnostic evidence rather than a universal
psychoacoustic or haptic threshold. Final acceptance also needs a physical
32-channel capture at conservative level, because mechanical resonances can
amplify a software spur that appears modest electrically.
