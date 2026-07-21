# The Delay-Line Doppler Source

*Design documentation for the wave-propagation stimulus at the heart of `haptic-server` (`haptic-server/src/engine.rs`, `WaveStimulus`). High-level companion to `ARCHITECTURE.md`; current status lives in `ROADMAP.md`.*

## 1. The idea in one paragraph

Each active note is modelled as a **point source** moving on the surface of the table, emitting a sinusoid in the 20–200 Hz haptic band. Every transducer receives that sinusoid **delayed by its physical distance to the source divided by the wave speed**, via a per-transducer fractional delay line. Because the delays are recomputed from the source position on every rendered frame, a *moving* source continuously changes each delay — and a time-varying delay is exactly a phase modulation whose derivative is a frequency shift. **Doppler is therefore an emergent property of the delay lines, not a feature that is computed.** There is no explicit Doppler code anywhere in the engine, and none should be added: the delay model produces the physically correct shift, including the correct asymmetry between approach and recession, for free.

```
        table (1 m × 2 m default, 4×8 cell-centred grid)
   ┌──────────────────────────────────────────────┐
   │   o      o      o      o      o      o       │   o  transducer i at xᵢ
   │                          ~~~                 │
   │   o      o      o     ~ ✚ →~   o      o      │   ✚  source at xₛ(t), moving
   │                          ~~~                 │      with velocity v
   │   o      o      o      o      o      o       │
   │        ↑ approaching: delay τᵢ shrinking     │   each o hears s(t − τᵢ(t))
   │          → frequency raised (Doppler)        │   τᵢ(t) = |xᵢ − xₛ(t)| / c
   └──────────────────────────────────────────────┘
```

Formally, transducer *i* outputs `s(t − τᵢ(t))` with `τᵢ(t) = |xᵢ − xₛ(t)| / c`. For a sinusoid `s(t) = sin(2πf₀t)` the instantaneous frequency at the transducer is

```
fᵢ(t) = f₀ · (1 − (d/dt)|xᵢ − xₛ(t)| / c)
```

— the classical Doppler formula, with `c` the configurable wave speed (default 20 m/s, range 0.25–1000 m/s). Wave speeds this low make the effect *strong*: at c = 1 m/s a source orbiting at a few tens of cm/s produces deep, audible (and palpable) pitch trajectories.

## 2. Signal path

```mermaid
flowchart LR
    subgraph clients["Clients (plugin / viewer)"]
        MPE["MPE events<br/>note on/off, bend,<br/>pressure, timbre"]
    end
    MPE -->|"Unix socket, framed bincode"| IPC["IPC thread"]
    IPC -->|"rtrb SPSC ring"| DRAIN["drain_commands()<br/>once per audio callback"]

    subgraph voice["WaveStimulus (per voice, ×8 pool)"]
        SMOOTH["MPE interpolator<br/>ramp + 2× one-pole"]
        POS["source position<br/>velocity-limited chase"]
        OSC["sine oscillator<br/>× envelope × pressure"]
        DL["32 fractional<br/>delay lines"]
        ATT["distance<br/>attenuation"]
        SMOOTH --> POS --> DL
        OSC --> DL --> ATT
    end
    DRAIN --> voice

    ATT -->|"mix voices, per-transducer<br/>gain, ±1 clamp"| RENDER["internal frame<br/>@ 1.5 kHz"]
    RENDER --> FIR["polyphase sinc<br/>reconstruction ×32"]
    FIR -->|"monitor routing"| DEV["cpal device<br/>@ 48 kHz"]
    RENDER -.->|"VoiceSnapshot ring"| VIEW["haptic-viewer<br/>phase visualiser"]
```

Everything inside the audio callback is allocation- and lock-free: the engine is *owned* by the callback, commands and layout hot-reloads arrive on `rtrb` SPSC rings, and voice snapshots leave on a third ring (dropped when full).

## 3. The delay line — scatter writes, sequential read

Each `WaveStimulus` owns 32 independent delay lines — one per transducer — because each transducer sits at a different distance and needs its own tap. A delay line is a heap-allocated ring buffer (`Box<[f32; 16384]>`).

The arrangement matters. For a **moving source and fixed listener**, the delay is a function of *emission* time: a sample emitted at frame *n*, when the source is at `xₛ(n)`, arrives at the fixed transducer at frame `a(n) = n + τᵢ(n)`. The line therefore uses **interpolating writes and a fixed (sequential) read** — each emitted sample is *scattered* into its fractional arrival slot, and a read pointer advancing exactly one cell per frame consumes whatever has arrived at the current frame, zeroing the slot behind it:

```
   read pointer (advances 1/frame,               scatter-write: emission n
   consumes & zeroes this slot)                  splatted (windowed sinc)
           ▼                                      around a(n) = n + τᵢ(n)
  ┌──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┐        ▲
  │  │  │  │▒▒│  │  │  │▁▃▅█▅▃▁│  │  │  │  │  │   SPLAT_TAPS-wide kernel
  └──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┴──┘   accumulated (+=); denser
     read side ←───── τᵢ·rate ─────→ write side    when the source approaches
```

This is the physically correct model, and it is *not* what a single fractional **read** tap behind a fixed write head does — that evaluates τ at *reception* time (the moving-*listener* model) and, on a fast approach, drives the read tap into the write head, reading the line backwards. Scatter-writes have no such failure: an approaching source simply bunches successive arrivals — the physically correct concentration of energy, which yields a Doppler **amplitude** gain as well as the frequency shift (verified in capture: channel RMS swings ~2.4× over an orbit). A receding source thins the arrivals out.

**The deposit kernel must be bandlimited.** The natural first cut — splatting each emission linearly across the two cells straddling `a(n)` — sounds bad: the 2-tap kernel's gain depends on the fractional arrival phase, so as the delay sweeps (a moving source) that phase-dependent gain amplitude-modulates the output into an audible granulation warble (measured at ~−20 dB of spurs on a spreading source — a receding note). The fix is a **windowed-sinc deposit** (`SPLAT_TAPS = 8`, Kaiser β = 9, precomputed at 128 fractional phases, each phase normalised to unit sum): its in-band gain is flat to < 0.1 dB across all phases, so the only amplitude variation left is the genuine Doppler bunching. This drops the granulation spurs by ~20 dB in both directions (spreading −20 → −41 dB, bunching −35 → −56 dB) while a stationary source stays at the −138 dB f32 floor. This is the standard cost of push-resampling: read-side (pull) interpolation reconstructs a smooth output for free, whereas write-side (push) needs a proper anti-imaging kernel — which the two-rate render's 32× cost headroom easily affords.

Per frame each line does one `SPLAT_TAPS`-wide scatter-write and one sequential read. A constant `SPLAT_HALF` lookahead (~2.7 ms at 1.5 kHz) is **added to every physical delay** so the whole kernel lands in unread cells without collapsing distinct short delays onto one floor. Distance attenuation `1 / (1 + 2d)` is applied *at the write* (each wavefront carries its own emission-time spreading loss into the line); the read is raw. Delays beyond capacity are **clamped, never wrapped** — a wrap would lap the read pointer and collapse the delay, which is precisely the latent bug the 1×2 m table exposed (see §6).

Note-off closes the source envelope but does not immediately deactivate a wave voice. The voice continues sequential reads until the latest possible scattered arrival has passed, preserving the physically emitted release tail at low wave speeds. Delay-line reset uses per-cell generations, so voice allocation, stealing, and panic logically clear pending samples in constant time rather than zeroing megabytes inside one callback.

**Output headroom.** The Doppler bunching gain is real level: an advancing source at the 0.5·c speed limit concentrates arrivals by up to 1/(1 − 0.5) = 2×. Stacked on a unity source at a coincident transducer (near-field `1/(1+2d) → 1`) that would rail the ±1 output clamp, so the default per-transducer gain is trimmed to 0.5 (−6 dB, `DEFAULT_TRANSDUCER_GAIN`). An explicit layout `gain` overrides it.

## 4. Keeping the model stable: three protective layers

The physics is one line; almost all of the engineering is making a *time-varying* delay behave. Three independent mechanisms, at three timescales, keep the delay trajectories smooth and causal.

```mermaid
flowchart TD
    A["Discrete MPE updates<br/>(client send rate, quantised to<br/>audio-block boundaries)"]
    B["<b>1 · MPE interpolator</b><br/>linear ramp over the measured update<br/>spacing (5–50 ms), then two cascaded<br/>15 ms one-poles"]
    C["<b>2 · Source velocity limit</b><br/>effective position chases the request at<br/>≤ 0.5 × wave speed; snaps only at note-on"]
    D["<b>3 · Delay clamp (backstop)</b><br/>τ clamped to buffer capacity —<br/>unreachable for realistic layouts"]
    E["Smooth, causal τᵢ(t)<br/>→ clean Doppler"]
    A --> B --> C --> D --> E
```

**Layer 1 — MPE interpolation** (`MpeInterp`). Controller updates arrive as discrete steps, further staircased to audio-block boundaries by the command queue. Feeding steps straight into the delay computation frequency-modulates the lines at the update rate, spraying an FM sideband comb around the carrier. Each new target is instead ramped linearly over roughly the *measured* arrival spacing (clamped 5–50 ms) — making smoothness independent of the client's send cadence — and the ramp output passes through two cascaded 15 ms one-poles (the second pole buys ~19 dB of extra sideband suppression for ~15 ms of position lag). Verified by buffer capture: in-band artefacts fell from −41.6 to −53 dB at c = 1 m/s.

**Layer 2 — subsonic source** (`SOURCE_SPEED_FRACTION = 0.5`). With scatter-writes, an emission's arrival index advances by `da/dn = 1 + v_r/c` per frame, where `v_r` is the source's radial velocity toward a transducer. If the source outran its waves (`v_r → −c` on approach) `da/dn` would fall to zero and successive arrivals would invert; run the other way and they would skip cells, leaving dropout gaps. Holding the table-space source speed to 0.5 × c bounds `da/dn ∈ [0.5, 1.5]`, so arrivals stay strictly monotonic *and* gap-free — a plain 2-tap accumulate needs no special-casing (the earlier read-tap model needed this limit for a harder reason: to stop the read tap overtaking the write head; 0.8 × c sufficed there, 0.5 × c is the conservative bound the scatter model wants). The effective position therefore chases the MPE-requested position at no more than 0.5 × c, snapping directly to the request at note-on only (a new note must not sweep in from wherever the stolen slot's previous voice sat). The viewer visualises this pair: a ring at the requested position, a cross at the effective source, a tether while it catches up.

**Layer 3 — capacity clamp.** A last-resort clamp of τ to the buffer length. When it engages, Doppler silently dies for that transducer (a clamped delay is a constant delay), so the design goal is to make it unreachable — which is what the rate architecture below achieves. The wave-speed floor (0.25 m/s) is chosen so a full-table propagation still fits.

## 5. The two-rate architecture

The haptic band ends at 200 Hz, but the device runs at 48 kHz. Rendering the wave field at the device rate wastes 32× the delay-line work *and* — the real killer — limits delay capacity in seconds. The engine instead renders at the device rate ÷ 32 (`RENDER_DECIMATION`), i.e. **1.5 kHz**, whose 750 Hz Nyquist comfortably covers the band:

```mermaid
flowchart LR
    R["Wave-field render<br/>@ 1.5 kHz<br/>(all voices, delay lines,<br/>gains, clamp)"]
    H["16-frame history ring<br/>(per transducer)"]
    P["Polyphase windowed-sinc FIR<br/>512 taps · 32 phases · 16 taps/phase<br/>Kaiser β = 10, cutoff 750 Hz"]
    O["Device frames @ 48 kHz<br/>one 16-tap dot product<br/>per frame per channel"]
    R --> H --> P --> O
```

Consequences:

- **Delay capacity stretches 32×**: 16384 internal frames ≈ 10.9 s of propagation. 8.3 s covers the full default table even at the 0.25 m/s wave-speed floor, so the §4 clamp never engages for realistic layouts.
- **Per-frame delay-line cost drops 32×.**
- **Reconstruction is a single filter doing two jobs**: the polyphase sinc both interpolates the internal-rate signal up to 48 kHz and suppresses its spectral images. Each device frame costs one 16-tap dot product per channel; each branch has unity DC gain (no amplitude ripple at the internal rate); first-image rejection is > 90 dB by unit test, −107 dB by capture (the f32 noise floor). Group delay is ~5.3 ms — irrelevant at haptic timescales.
- The upsampler state persists across callbacks, so device block sizes need not divide the decimation factor (regression-tested: chunked and whole renders are bit-identical).

The earlier linear-interpolation upsampler was replaced after its images (~−50 dB above 1 kHz) proved audible on monitors; the delay-line and Doppler behaviour was unchanged by the swap, only the reconstruction quality.

## 6. Failure modes that shaped the design

Each protective mechanism above earned its place by a concrete, captured failure:

| Symptom | Root cause | Fix |
|---|---|---|
| Far-corner transducer sounded *immediately* (no propagation delay) on the 1×2 m table | Worst-case delay (diagonal ÷ 20 m/s ≈ 112 ms) exceeded the then-100 ms buffer; the overflow path collapsed the delay to ~zero | Bigger buffers, clamp-not-wrap semantics (§3) |
| Pitch *steps* at the orbit period; Doppler vanishing over arcs of the orbit | Delay hitting the capacity clamp — a clamped delay is Doppler-free | Two-rate render stretching capacity to ~10.9 s (§5) |
| Sideband comb around the carrier at the audio-block/MPE-send rate | Stepped position targets frequency-modulating the delay lines | Measured-spacing ramp + double one-pole (§4, layer 1) |
| Garbled output on fast MPE jumps | Source moving supersonically: under the old read-tap line the read tap overtook the write head (line read backwards) | Scatter-write line (§3) removes the overtake failure entirely; 0.5 × c source velocity limit keeps scatter arrivals monotonic and gap-free (§4, layer 2) |
| Audible high-frequency images on monitors | Linear-interp upsampler images at ~−50 dB | Polyphase Kaiser-sinc reconstruction, images at −107 dB (§5) |
| High-frequency warble on a moving source (worst at low c / tight orbits) | 2-tap linear scatter kernel: its gain varies with the fractional arrival phase, amplitude-modulating the output as the delay sweeps (granulation spurs ~−20 dB) | Bandlimited windowed-sinc deposit, flat gain across phases, spurs −20 dB lower (§3) |
| Popping / clipping bursts as the source passes close to a transducer | Doppler bunching gain (new, real level) stacking on the near-field `1/(1+2d)` gain and railing the ±1 clamp | Default per-transducer gain trimmed to 0.5 for 2× bunching headroom (§3) |

The debugging instrument for all of these is the headless capture harness (`orbit_capture_writes_debug_buffers` in `engine.rs`): it drives `process_block` with the viewer's exact orbit command stream against a dummy 32-channel sink and writes raw 32-channel f32 output for offline spectral analysis. Design-level claims above (image rejection, sideband levels, pitch-jump counts, granulation-spur reduction) were verified against those captures, and the load-bearing properties are pinned by unit tests (scatter-line Doppler direction and amplitude gain, splat-kernel flat in-band gain, delay-not-wrap, velocity limit, per-branch DC gain and image rejection, block-size invariance).

## 7. Boundaries of the model

Deliberate simplifications, so future work doesn't mistake them for oversights:

- **Direct path only.** No reflections at table edges, no standing-wave structure, no dispersion (all frequencies travel at the same c). Real standing-wave spatial structure is the Phase E research track; `StandingWaveStimulus` is currently an in-phase placeholder, not a wave model.
- **2D geometry, isotropic medium.** Transducer positions and the source live on a plane; attenuation is the ad-hoc `1/(1 + 2d)`, not a fitted physical law.
- **Per-voice independence.** Voices superpose linearly; there is no inter-voice interaction. Each of the up-to-8 wave voices carries its own 32 delay lines and its own wave speed (captured at note-on from the `WaveSpeed` parameter).
- **Viewer is an approximation.** Snapshots carry every active wave and standing voice, but not synchronized oscillator phase or delay-line history. The viewer therefore renders a clearly labelled, phase-aligned geometric preview rather than claiming exact multi-voice output interference.
