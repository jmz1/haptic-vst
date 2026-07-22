# Composition model and syllabary direction

Haptic VST is primarily a tool for composing haptic accompaniment inside
Ableton Live, not a standalone synthesis language. Push supplies performable
MPE gestures; Live records and edits those gestures; VST parameters provide
track-level automation; the server turns several tracks into one 32-channel
physical field.

“Syllabary” is the working name for a future small vocabulary of recognisable
haptic event types. Wave and Travelling Wave are the first concrete vocabulary,
but there is not yet a generic syllable descriptor protocol. This document
keeps the compositional idea and its open questions together without presenting
speculative wire messages as current behaviour.

## The workflow

The expected path is:

```text
Push 3 performance
   │ notes, velocity, bend, pressure, slide
   ▼
Ableton MIDI clip and expression lanes
   │ editing + track automation + optional M4L modulation
   ▼
one Haptic Controller instance per haptic track
   │ per-instance patch + note expression
   ▼
haptic-server
   │ superposition of all active voices
   ▼
32 logical transducer channels
```

This makes the DAW the durable composition surface. A control that exists only
inside a bespoke server UI is difficult to record, edit, arrange, or revisit.
Operational controls such as monitor routing are exceptions; they describe the
system rather than the authored stimulus.

## Four control layers

The Push → Live → VST3 path offers a small set of carriers. Every useful
stimulus parameter should have an intentional home among them.

| Layer | Carrier | Character | DAW representation |
|---|---|---|---|
| Identity | note and plugin instance | selected at note-on | notes and tracks |
| Articulation | strike and release velocity | one-shot | velocity lanes |
| Expression | bend, pressure, timbre/CC74 | continuous per note | MPE expression lanes |
| Patch | stable VST3 parameters | continuous per instance | automation and M4L modulation |

Continuous per-note expression is scarce: bend, pressure, and timbre are the
three available dimensions. Arbitrary per-note CCs do not survive the workflow
as equally editable MPE lanes. Richer controls therefore belong at patch level
unless a stimulus makes a deliberate trade.

The current bindings for both Wave and TW are:

| Input | Meaning |
|---|---|
| note | oscillator frequency, standard MIDI then 20–200 Hz clamp |
| strike velocity | base amplitude |
| bend | source x across the table |
| timbre / CC74 | source y along the table |
| pressure | intensity |
| VST parameters | stimulus type, spatial scale, and distance decay |

Live will conventionally present bend as pitch expression even though these
stimuli spend it on spatial x. That mismatch is currently accepted because
position is more valuable to the instrument than continuous repitching.

## Current multi-instance model

One plugin instance corresponds naturally to one Live track. On connection it
registers an `instance_id` and complete `InstanceConfig`; its notes are owned by
that identity, and its parameter automation updates only that configuration and
the active voices for which a parameter is live.

This gives a composition several independent haptic instruments against one
server without last-writer-wins global settings. The viewer can filter by
instance or show their geometric sum.

The current patch chooses Wave or TW for new notes. The wire `NoteOn` does not
carry an arbitrary syllable identifier; the server looks up the sending
instance's registered stimulus type. This is sufficient for the current
one-track/one-instrument model and should not be generalized until a composition
demonstrates a need to interleave types within one track.

## Existing vocabulary

### Wave: a moving propagated source

Wave feels like a thing moving through the table. Position consumes bend and
timbre, pressure controls intensity, and wave speed is a track patch parameter
latched at note-on. Decay changes affect new emissions while older energy
continues with its emission-time gain.

Its defining character is propagation history: motion produces Doppler and a
released source leaves a delayed tail. See [`wave.md`](wave.md).

### Travelling Wave: an instantaneous radial field

TW uses the same note and MPE bindings but treats phase as an instantaneous
radial pattern. Speed or fixed wavelength and distance decay can change live
without retriggering. There is no Doppler or in-flight energy.

Its defining character is a wave-like spatial relationship that can be reshaped
directly by automation. See [`travelling-wave.md`](travelling-wave.md).

## What a useful future syllable should contain

A syllable is not merely another Rust type. It is a small composition-facing
contract:

- a synthesis/spatial model with a perceptible character;
- an intentional allocation of note, articulation, and the three MPE
  expression dimensions;
- a stable set of patch parameters that a DAW can automate;
- explicit per-parameter apply semantics: note-on, live, or release;
- bounded voice lifecycle and observer representation; and
- a test or capture that connects the model to its claimed behaviour.

New vocabulary should be driven by a desired haptic gesture, not by a desire to
exercise a generic protocol. Candidate research names include:

- **spatial sweep:** an authored gesture translating along a path;
- **impact:** a localized one-shot or percussive excitation;
- **modal/reflected field:** an explicitly defined boundary or resonance model;
- **coupled network:** a textural system whose local oscillators exchange
  energy.

These names are prompts, not planned runtime variants. In particular, a future
modal field would not restore the removed in-phase Standing placeholder.

## Reactive input

The first reactive path should remain DAW-native: an Envelope Follower or other
Max for Live device analyses an audio track and modulates a stable VST
parameter. That signal is visible in the arrangement, can be edited, and
requires no new server modulation protocol.

A dedicated low-rate modulation stream would earn its complexity only if
experiments show a need for per-voice reactive depth, lower latency than host
automation provides, or a source that cannot live naturally in the DAW. Even
then, routes should stay small and explicit rather than turning the server into
a general modulation-matrix engine.

## Stable VST parameters constrain dynamic vocabulary

VST3 hosts expect a plugin's parameter identities to remain stable. The server
cannot advertise an arbitrary new stimulus and expect an already instantiated
plugin to grow a new set of automatable host parameters safely.

Before a generic descriptor system, choose among realistic strategies:

- a stable superset containing parameters for the supported vocabulary;
- a fixed number of generic parameter slots with server-provided labels;
- separate plugin classes for materially different instruments; or
- a deliberately fixed built-in vocabulary released with coordinated plugin
  and server versions.

The current plugin uses the first strategy on a small scale: TW parameters stay
registered when Wave is selected and are merely de-emphasized in the editor.

## Protocol direction

The enabling foundation is already present:

- exact-version `Hello`/`HelloAccepted`;
- client roles and per-instance configuration;
- voice identity keyed by instance, channel, and note;
- observer snapshots tagged by instance and stimulus type; and
- shared numeric bounds and parameter validation.

Before opening the protocol to dynamic syllables or third-party clients, it
still needs explicit stable numeric identities and capability negotiation.
Bincode enum order plus exact-version rejection is safe for coordinated builds,
but it is not a durable extension scheme.

Descriptor design should follow implemented vocabulary. A useful descriptor
might eventually describe parameter name, unit, bounds, curve, carrier layer,
and apply timing, but encoding that schema before the next real stimulus exists
would freeze guesses into the protocol.

## Open edges

- Should a track remain one stimulus type, or is per-note type selection
  genuinely useful in Live?
- Should expression bindings ever be reconfigurable, or is a clear fixed
  binding per stimulus more legible in recorded clips?
- Is release velocity useful enough to carry through the plugin, protocol, and
  envelope lifecycle?
- Does Push/Live need an explicit convention communicating that bend is spatial
  rather than pitch?
- Which next haptic character is sufficiently distinct from Wave and TW to
  justify a new syllable?
- What parameter strategy best preserves stable DAW automation if the
  vocabulary grows?

## Non-goals for now

- encoding stimulus choice through note ranges or velocity bands;
- adding OSC, network transport, or MIDI 2.0 discovery without a concrete
  workflow need;
- implementing a generic server modulation matrix;
- promising dynamic host parameters based on a connected server; or
- treating tentative vocabulary names as roadmap commitments.
