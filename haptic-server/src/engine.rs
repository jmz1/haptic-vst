use crate::config::TransducerLayout;
#[cfg(test)]
use haptic_protocol::DEFAULT_TEST_NOTE;
use haptic_protocol::{
    distance_gain, effective_wavelength, DistanceDecay, HapticCommand, InstanceConfig, MpeData,
    Parameter, SpatialScaleMode, StimulusType, TravellingWaveConfig, VoiceInfo, DEFAULT_ATTEN_D0_M,
    DEFAULT_ATTEN_EXPONENT, DEFAULT_WAVE_SPEED, MAX_ACTIVE_VOICES, MAX_ATTEN_D0_M,
    MAX_ATTEN_EXPONENT, MAX_WAVELENGTH_M, MAX_WAVE_SPEED, MIN_ATTEN_D0_M, MIN_ATTEN_EXPONENT,
    MIN_WAVELENGTH_M, MIN_WAVE_SPEED,
};

// Constants from requirements
pub const TRANSDUCER_COUNT: usize = 32;
const MAX_WAVE_STIMULI: usize = 8;
const MAX_TRAVELLING_WAVE_STIMULI: usize = 8;
const _: () = assert!(MAX_ACTIVE_VOICES >= MAX_WAVE_STIMULI + MAX_TRAVELLING_WAVE_STIMULI);
// Delay-line capacity in *internal-rate* samples (see RENDER_DECIMATION):
// 16384 samples at 48 kHz / 32 = 1.5 kHz is ~10.9 s of propagation — 8.3 s
// covers the full default table at the 0.25 m/s wave-speed floor, so the
// safety clamp below is unreachable for realistic layouts. Delays beyond
// capacity are clamped, not wrapped.
const MAX_DELAY_SAMPLES: usize = 16384;

/// The wave field is synthesised at the device rate divided by this factor
/// (48 kHz -> 1.5 kHz) and upsampled to the device rate at the output
/// stage. The internal Nyquist (750 Hz at 48 kHz) comfortably covers the
/// 20-200 Hz transducer band, per-sample delay-line work drops 32x, and
/// delay-line capacity in seconds stretches 32x, which is what lets slow
/// wave speeds run without clamping.
pub const RENDER_DECIMATION: usize = 32;

/// Reconstruction filter length per polyphase branch. The interpolator is a
/// windowed-sinc lowpass with cutoff at the internal Nyquist, evaluated
/// polyphase: each device frame is a FIR_TAPS_PER_PHASE-tap dot product
/// against the most recent internal frames. 16 taps x Kaiser beta 10 gives
/// ~100 dB image rejection with the transition band comfortably between
/// the 200 Hz content edge and the first image at internal_rate - 200 Hz.
const FIR_TAPS_PER_PHASE: usize = 16;
const FIR_LEN: usize = FIR_TAPS_PER_PHASE * RENDER_DECIMATION;

/// Bandlimited scatter (deposit) kernel for the delay line. Each emitted
/// sample is splatted across `SPLAT_TAPS` ring cells straddling its fractional
/// arrival index, weighted by a windowed sinc. A naive 2-tap linear splat has
/// a frequency-domain gain that varies with the fractional phase; as the delay
/// sweeps (a moving source) that variation amplitude-modulates the signal into
/// an audible granulation warble (~frac(da/dn)·f_internal). A windowed-sinc
/// deposit has near-flat in-band gain regardless of phase, removing it. The
/// kernel is precomputed at `SPLAT_PHASES` fractional phases and each phase is
/// normalised to unit sum (flat DC, so bunched arrivals still give the correct
/// Doppler amplitude gain — the deposit is bandlimited, not attenuated).
const SPLAT_TAPS: usize = 8;
const SPLAT_HALF: usize = SPLAT_TAPS / 2; // taps cover cells base-(HALF-1)..=base+HALF
const SPLAT_PHASES: usize = 128;
const SPLAT_LEN: usize = SPLAT_TAPS * SPLAT_PHASES;

// Haptic frequency band of the transducers
const MIN_HAPTIC_FREQ: f32 = 20.0;
const MAX_HAPTIC_FREQ: f32 = 200.0;

// One-pole smoothing time constant for incoming MPE dimensions
const MPE_SMOOTHING_TAU: f32 = 0.015; // 15 ms

/// Incoming MPE updates arrive as discrete steps (a controller's send rate,
/// further quantised to audio-block boundaries by the command queue). Each
/// new target is linearly ramped over roughly the measured update spacing,
/// clamped to this range (seconds), before the one-pole above — otherwise
/// the block-rate staircase frequency-modulates the delay lines into an
/// audible sideband comb around the carrier.
const MPE_RAMP_MIN_S: f32 = 0.005;
const MPE_RAMP_MAX_S: f32 = 0.05;

/// Maximum speed of the effective source position, as a fixed fraction of
/// the stimulus's wave speed. Keeps the source comfortably subsonic relative
/// to its own waves. With the scatter-write delay line (see `DelayLine`), an
/// emission's arrival index advances by da/dn = 1 + v_r/c per frame, where
/// v_r is the source's radial velocity toward a transducer. Holding the
/// table-space source speed to 0.5·c bounds da/dn ∈ [0.5, 1.5]: arrivals stay
/// monotonic (no order inversion on approach) and never skip a ring cell (no
/// dropout gaps on recession), so 2-tap linear scatter needs no special-casing.
const SOURCE_SPEED_FRACTION: f32 = 0.5;

/// Capacity of the IPC → audio thread command ring buffer. Sized for a
/// worst-case burst of MPE traffic within one audio callback.
const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Maximum concurrently-registered client instances (plugins + viewer). Small
/// and fixed so the instance→config registry is a linear scan with no
/// allocation on the audio thread.
const MAX_INSTANCES: usize = 16;

/// Per-block snapshot of every active Wave/TW voice, exported to the IPC thread
/// for phase visualisation. Fixed-size (no allocation on the audio thread);
/// `count` entries of `voices` are valid. Each `VoiceInfo` is tagged with its
/// owning `instance_id` so the viewer can filter or sum.
#[derive(Clone, Copy)]
pub struct VoiceSnapshot {
    pub sample_rate: f32,
    pub count: u8,
    pub voices: [VoiceInfo; MAX_ACTIVE_VOICES],
}

/// Map a MIDI note to its standard equal-tempered frequency, then clamp it to
/// the transducers' 20-200 Hz band. There is no octave transposition. MIDI 60
/// (Ableton C3) is 261.6 Hz and therefore saturates at the 200 Hz ceiling.
pub fn note_to_haptic_frequency(note: u8) -> f32 {
    let f = 440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0);
    f.clamp(MIN_HAPTIC_FREQ, MAX_HAPTIC_FREQ)
}

// Core trait - must be Send + Sync for thread safety
pub trait Stimulus: Send + Sync {
    fn process(&mut self, context: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT];
    fn is_active(&self) -> bool;
    fn is_releasing(&self) -> bool;
    fn note_on(&mut self, frequency: f32, velocity: u8, mpe: MpeData);
    fn note_off(&mut self);
    fn mpe_update(&mut self, mpe: MpeData);
    fn reset(&mut self);
    fn set_wave_speed(&mut self, _wave_speed: f32) {
        // Default implementation does nothing (for stimuli that don't use wave speed)
    }
    fn set_distance_decay(&mut self, _decay: DistanceDecay) {}
}

// Static allocation pool
pub struct StimulusPool<T: Stimulus + Default, const N: usize> {
    stimuli: [T; N],
    active_mask: [bool; N],
}

impl<T: Stimulus + Default, const N: usize> StimulusPool<T, N> {
    pub fn new() -> Self {
        Self {
            stimuli: std::array::from_fn(|_| T::default()),
            active_mask: [false; N],
        }
    }

    /// Find a free slot, mark it active and reset its stimulus.
    fn allocate_slot(&mut self) -> Option<usize> {
        for (i, active) in self.active_mask.iter_mut().enumerate() {
            if !*active {
                *active = true;
                self.stimuli[i].reset();
                return Some(i);
            }
        }
        None
    }

    /// Re-arm an occupied slot for voice stealing.
    fn retrigger_slot(&mut self, slot: usize) {
        self.active_mask[slot] = true;
        self.stimuli[slot].reset();
    }

    fn get_mut(&mut self, slot: usize) -> &mut T {
        &mut self.stimuli[slot]
    }

    fn slot_active(&self, slot: usize) -> bool {
        self.active_mask[slot]
    }

    fn slot_releasing(&self, slot: usize) -> bool {
        self.stimuli[slot].is_releasing()
    }

    fn reset_all(&mut self) {
        for (i, active) in self.active_mask.iter_mut().enumerate() {
            *active = false;
            self.stimuli[i].reset();
        }
    }

    pub fn process_all(
        &mut self,
        context: &ProcessContext<'_>,
        output: &mut [f32; TRANSDUCER_COUNT],
    ) {
        for (i, stimulus) in self.stimuli.iter_mut().enumerate() {
            if self.active_mask[i] {
                if !stimulus.is_active() {
                    self.active_mask[i] = false;
                } else {
                    let stimulus_output = stimulus.process(context);
                    for (out, &val) in output.iter_mut().zip(stimulus_output.iter()) {
                        *out += val;
                    }
                }
            }
        }
    }
}

/// Which note owns a pool slot; `seq` orders allocations for voice stealing.
/// Voice identity is `(instance_id, channel, note)` — the `instance_id` keeps
/// notes from concurrent plugin instances that happen to share a MIDI
/// channel/note from colliding.
#[derive(Clone, Copy)]
struct VoiceOwner {
    instance_id: u64,
    channel: u8,
    note: u8,
    seq: u64,
}

/// Pick a slot to steal: oldest owner in release, else oldest owner.
fn steal_candidate<T: Stimulus + Default, const N: usize>(
    pool: &StimulusPool<T, N>,
    owners: &[Option<VoiceOwner>; N],
) -> usize {
    let mut best: Option<(usize, bool, u64)> = None; // (slot, releasing, seq)
    for (i, owner) in owners.iter().enumerate() {
        let seq = owner.map(|o| o.seq).unwrap_or(0);
        let releasing = pool.slot_releasing(i);
        let better = match best {
            None => true,
            Some((_, best_releasing, best_seq)) => {
                (releasing, std::cmp::Reverse(seq)) > (best_releasing, std::cmp::Reverse(best_seq))
            }
        };
        if better {
            best = Some((i, releasing, seq));
        }
    }
    best.map(|(i, _, _)| i).unwrap_or(0)
}

// Main engine with thread-safe command queue
pub struct StimulusEngine {
    wave_pool: StimulusPool<WaveStimulus, MAX_WAVE_STIMULI>,
    travelling_wave_pool: StimulusPool<TravellingWaveStimulus, MAX_TRAVELLING_WAVE_STIMULI>,

    // Note -> slot ownership, parallel to each pool's slots
    wave_owners: [Option<VoiceOwner>; MAX_WAVE_STIMULI],
    travelling_wave_owners: [Option<VoiceOwner>; MAX_TRAVELLING_WAVE_STIMULI],
    next_seq: u64,

    // Per-instance note-type config, keyed by instance_id. Replaces the old
    // single server-global wave speed / stimulus type: a note reads the config
    // of the instance that sent it at note-on, so concurrent controllers never
    // contend. Fixed-capacity linear-scan map — no allocation on the audio
    // thread. `MonitorRoute` remains global (a device concern), below.
    instances: [Option<(u64, InstanceConfig)>; MAX_INSTANCES],

    // Lock-free SPSC command queues, consumer ends (IPC thread holds the
    // command producer, the config watcher holds the layout producer)
    command_queue: rtrb::Consumer<EngineCommand>,
    layout_queue: rtrb::Consumer<TransducerLayout>,

    // Voice snapshots out to the IPC thread (drops when full)
    voice_producer: rtrb::Producer<VoiceSnapshot>,
    last_voice_snapshot_count: u8,

    // Physical output p plays logical channel monitor_routes[p]
    monitor_routes: [u8; TRANSDUCER_COUNT],

    // Transducer configuration (hot-swappable via layout_queue)
    layout: TransducerLayout,

    // Output upsampler state: the engine renders at the device rate divided
    // by RENDER_DECIMATION; device frames are reconstructed by a polyphase
    // windowed-sinc filter over the most recent internal frames (newest at
    // history[history_pos], ring order oldest-ward).
    history: [[f32; TRANSDUCER_COUNT]; FIR_TAPS_PER_PHASE],
    history_pos: usize,
    interp_phase: usize,
    fir: Box<[f32; FIR_LEN]>,

    // Bandlimited scatter kernel for the delay lines (see design_splat_kernel)
    splat_kernel: Box<[f32; SPLAT_LEN]>,
}

pub struct ProcessContext<'a> {
    pub sample_rate: f32,
    pub dt: f32,
    pub transducer_positions: &'a [(f32, f32); TRANSDUCER_COUNT],
    /// (width, length) of the table, for MPE -> source-position mapping.
    pub table_m: (f32, f32),
    /// Bandlimited scatter kernel shared by the voices' delay lines.
    pub splat_kernel: &'a [f32; SPLAT_LEN],
}

// Commands from IPC thread. Every note/parameter command carries the
// `instance_id` of the connection it arrived on (stamped by the IPC layer),
// so per-instance config and (instance_id, channel, note) voice identity work
// without the client tagging each message.
#[derive(Clone)]
pub enum EngineCommand {
    /// Register or replace an instance's note-type config (from `Hello`).
    RegisterInstance {
        instance_id: u64,
        config: InstanceConfig,
    },
    /// A client's socket closed. Release all of its voices and return its
    /// fixed-capacity configuration slot to the registry.
    DisconnectInstance {
        instance_id: u64,
    },
    NoteOn {
        instance_id: u64,
        note: u8,
        velocity: u8,
        channel: u8,
        mpe: MpeData,
    },
    NoteOff {
        instance_id: u64,
        note: u8,
        channel: u8,
    },
    MpeUpdate {
        instance_id: u64,
        channel: u8,
        mpe: MpeData,
    },
    SetParameter {
        instance_id: u64,
        parameter: Parameter,
    },
    Panic,
}

impl EngineCommand {
    /// Convert a wire command into an engine command, stamping it with the
    /// `instance_id` of the connection it arrived on. `Hello` carries its own
    /// id/config; everything else inherits the connection's bound id.
    pub fn from_wire(cmd: HapticCommand, instance_id: u64) -> Self {
        match cmd {
            HapticCommand::Hello {
                instance_id,
                config,
                ..
            } => EngineCommand::RegisterInstance {
                instance_id,
                config,
            },
            HapticCommand::NoteOn {
                note,
                velocity,
                channel,
                mpe,
                ..
            } => EngineCommand::NoteOn {
                instance_id,
                note,
                velocity,
                channel,
                mpe,
            },
            HapticCommand::NoteOff { note, channel, .. } => EngineCommand::NoteOff {
                instance_id,
                note,
                channel,
            },
            HapticCommand::MpeUpdate { channel, mpe, .. } => EngineCommand::MpeUpdate {
                instance_id,
                channel,
                mpe,
            },
            HapticCommand::SetParameter { parameter, .. } => EngineCommand::SetParameter {
                instance_id,
                parameter,
            },
            HapticCommand::Panic => EngineCommand::Panic,
        }
    }
}

impl StimulusEngine {
    /// Returns the engine (owned by the audio callback), the command
    /// producer (owned by the IPC thread), the layout producer (owned by
    /// the config watcher thread), and the voice-snapshot consumer (owned
    /// by the IPC thread). Separate rings keep every path SPSC.
    pub fn new(
        layout: TransducerLayout,
    ) -> (
        Self,
        rtrb::Producer<EngineCommand>,
        rtrb::Producer<TransducerLayout>,
        rtrb::Consumer<VoiceSnapshot>,
    ) {
        let (producer, consumer) = rtrb::RingBuffer::new(COMMAND_QUEUE_CAPACITY);
        let (layout_producer, layout_consumer) = rtrb::RingBuffer::new(4);
        let (voice_producer, voice_consumer) = rtrb::RingBuffer::new(256);

        let engine = Self {
            wave_pool: StimulusPool::new(),
            travelling_wave_pool: StimulusPool::new(),
            wave_owners: [None; MAX_WAVE_STIMULI],
            travelling_wave_owners: [None; MAX_TRAVELLING_WAVE_STIMULI],
            next_seq: 0,
            instances: [None; MAX_INSTANCES],
            command_queue: consumer,
            layout_queue: layout_consumer,
            voice_producer,
            last_voice_snapshot_count: 0,
            monitor_routes: std::array::from_fn(|i| i as u8),
            layout,
            history: [[0.0; TRANSDUCER_COUNT]; FIR_TAPS_PER_PHASE],
            history_pos: 0,
            // Force a render on the very first device frame
            interp_phase: RENDER_DECIMATION - 1,
            fir: design_reconstruction_fir(),
            splat_kernel: design_splat_kernel(),
        };
        (engine, producer, layout_producer, voice_consumer)
    }

    /// Config for `instance_id`, or the default if the instance has not
    /// registered one yet (e.g. a note arrived before its `Hello`).
    fn instance_config(&self, instance_id: u64) -> InstanceConfig {
        self.instances
            .iter()
            .flatten()
            .find(|(id, _)| *id == instance_id)
            .map(|(_, cfg)| *cfg)
            .unwrap_or_default()
    }

    /// Register or replace an instance's config, or (if unregistered) create
    /// it. Returns a mutable handle to the stored config for in-place patching.
    fn instance_config_mut(&mut self, instance_id: u64) -> Option<&mut InstanceConfig> {
        if let Some(slot) = self
            .instances
            .iter()
            .position(|e| matches!(e, Some((id, _)) if *id == instance_id))
        {
            return self.instances[slot].as_mut().map(|(_, cfg)| cfg);
        }
        // Not registered yet: claim the first free slot with a default config.
        if let Some(slot) = self.instances.iter().position(|e| e.is_none()) {
            self.instances[slot] = Some((instance_id, InstanceConfig::default()));
            return self.instances[slot].as_mut().map(|(_, cfg)| cfg);
        }
        None // registry full — drop silently (16 concurrent instances is ample)
    }

    fn register_instance(&mut self, instance_id: u64, config: InstanceConfig) {
        if let Some(cfg) = self.instance_config_mut(instance_id) {
            *cfg = config;
        }
    }

    fn disconnect_instance(&mut self, instance_id: u64) {
        for slot in 0..MAX_WAVE_STIMULI {
            if self.wave_owners[slot].is_some_and(|owner| owner.instance_id == instance_id) {
                self.wave_pool.get_mut(slot).note_off();
            }
        }
        for slot in 0..MAX_TRAVELLING_WAVE_STIMULI {
            if self.travelling_wave_owners[slot]
                .is_some_and(|owner| owner.instance_id == instance_id)
            {
                self.travelling_wave_pool.get_mut(slot).note_off();
            }
        }
        if let Some(slot) = self
            .instances
            .iter()
            .position(|entry| entry.is_some_and(|(id, _)| id == instance_id))
        {
            self.instances[slot] = None;
        }
    }

    fn note_on(&mut self, instance_id: u64, note: u8, velocity: u8, channel: u8, mpe: MpeData) {
        let frequency = note_to_haptic_frequency(note);
        let seq = self.next_seq;
        self.next_seq += 1;
        let owner = VoiceOwner {
            instance_id,
            channel,
            note,
            seq,
        };
        let config = self.instance_config(instance_id);

        match config.stimulus_type {
            StimulusType::Wave => {
                let slot = match self.wave_pool.allocate_slot() {
                    Some(slot) => slot,
                    None => {
                        let slot = steal_candidate(&self.wave_pool, &self.wave_owners);
                        self.wave_pool.retrigger_slot(slot);
                        slot
                    }
                };
                let stim = self.wave_pool.get_mut(slot);
                stim.note_on(frequency, velocity, mpe);
                stim.set_wave_speed(config.wave_speed);
                stim.configure_distance_decay(config.distance_decay);
                self.wave_owners[slot] = Some(owner);
            }
            StimulusType::TravellingWave => {
                let slot = match self.travelling_wave_pool.allocate_slot() {
                    Some(slot) => slot,
                    None => {
                        let slot = steal_candidate(
                            &self.travelling_wave_pool,
                            &self.travelling_wave_owners,
                        );
                        self.travelling_wave_pool.retrigger_slot(slot);
                        slot
                    }
                };
                let stim = self.travelling_wave_pool.get_mut(slot);
                stim.note_on(frequency, velocity, mpe);
                stim.configure(config.travelling_wave, config.distance_decay, true);
                self.travelling_wave_owners[slot] = Some(owner);
            }
        }
    }

    fn note_off(&mut self, instance_id: u64, note: u8, channel: u8) {
        for slot in 0..MAX_WAVE_STIMULI {
            if let Some(owner) = self.wave_owners[slot] {
                if owner.instance_id == instance_id
                    && owner.channel == channel
                    && owner.note == note
                {
                    self.wave_pool.get_mut(slot).note_off();
                }
            }
        }
        for slot in 0..MAX_TRAVELLING_WAVE_STIMULI {
            if let Some(owner) = self.travelling_wave_owners[slot] {
                if owner.instance_id == instance_id
                    && owner.channel == channel
                    && owner.note == note
                {
                    self.travelling_wave_pool.get_mut(slot).note_off();
                }
            }
        }
        // Ownership is retained through the release phase so late MPE
        // updates still reach the voice; it is cleared once inactive.
    }

    fn mpe_update(&mut self, instance_id: u64, channel: u8, mpe: MpeData) {
        for slot in 0..MAX_WAVE_STIMULI {
            if let Some(owner) = self.wave_owners[slot] {
                if owner.instance_id == instance_id && owner.channel == channel {
                    self.wave_pool.get_mut(slot).mpe_update(mpe);
                }
            }
        }
        for slot in 0..MAX_TRAVELLING_WAVE_STIMULI {
            if let Some(owner) = self.travelling_wave_owners[slot] {
                if owner.instance_id == instance_id && owner.channel == channel {
                    self.travelling_wave_pool.get_mut(slot).mpe_update(mpe);
                }
            }
        }
    }

    /// Drop ownership entries for slots whose stimulus finished its release.
    fn reap_finished_voices(&mut self) {
        for slot in 0..MAX_WAVE_STIMULI {
            if self.wave_owners[slot].is_some() && !self.wave_pool.slot_active(slot) {
                self.wave_owners[slot] = None;
            }
        }
        for slot in 0..MAX_TRAVELLING_WAVE_STIMULI {
            if self.travelling_wave_owners[slot].is_some()
                && !self.travelling_wave_pool.slot_active(slot)
            {
                self.travelling_wave_owners[slot] = None;
            }
        }
    }

    fn set_instance_decay(&mut self, instance_id: u64, d0_m: Option<f32>, exponent: Option<f32>) {
        let mut decay = self.instance_config(instance_id).distance_decay;
        if let Some(value) = d0_m {
            decay.d0_m = value;
        }
        if let Some(value) = exponent {
            decay.exponent = value;
        }
        for (slot, owner) in self.wave_owners.iter().enumerate() {
            if owner.is_some_and(|owner| owner.instance_id == instance_id) {
                self.wave_pool.get_mut(slot).set_distance_decay(decay);
            }
        }
        for (slot, owner) in self.travelling_wave_owners.iter().enumerate() {
            if owner.is_some_and(|owner| owner.instance_id == instance_id) {
                self.travelling_wave_pool
                    .get_mut(slot)
                    .set_distance_decay(decay);
            }
        }
    }

    fn apply_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::RegisterInstance {
                instance_id,
                config,
            } => {
                self.register_instance(instance_id, config);
            }
            EngineCommand::DisconnectInstance { instance_id } => {
                self.disconnect_instance(instance_id);
            }
            EngineCommand::NoteOn {
                instance_id,
                note,
                velocity,
                channel,
                mpe,
            } => {
                self.note_on(instance_id, note, velocity, channel, mpe);
            }
            EngineCommand::NoteOff {
                instance_id,
                note,
                channel,
            } => {
                self.note_off(instance_id, note, channel);
            }
            EngineCommand::MpeUpdate {
                instance_id,
                channel,
                mpe,
            } => {
                self.mpe_update(instance_id, channel, mpe);
            }
            EngineCommand::SetParameter {
                instance_id,
                parameter,
            } => match parameter {
                // WaveSpeed / StimulusType patch the sender's own instance config.
                Parameter::WaveSpeed(speed) => {
                    if let Some(cfg) = self.instance_config_mut(instance_id) {
                        cfg.wave_speed = speed.clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED);
                        cfg.travelling_wave.wave_speed = cfg.wave_speed;
                    }
                    for (slot, owner) in self.travelling_wave_owners.iter().enumerate() {
                        if owner.is_some_and(|owner| owner.instance_id == instance_id) {
                            self.travelling_wave_pool
                                .get_mut(slot)
                                .set_wave_speed(speed);
                        }
                    }
                }
                Parameter::StimulusType(kind) => {
                    if let Some(cfg) = self.instance_config_mut(instance_id) {
                        cfg.stimulus_type = kind;
                    }
                }
                // MonitorRoute is a server-global device concern.
                Parameter::MonitorRoute { output, source } => {
                    if (output as usize) < TRANSDUCER_COUNT {
                        self.monitor_routes[output as usize] =
                            source.min(TRANSDUCER_COUNT as u8 - 1);
                    }
                }
                Parameter::TravellingWaveScaleMode(mode) => {
                    if let Some(cfg) = self.instance_config_mut(instance_id) {
                        cfg.travelling_wave.scale_mode = mode;
                    }
                    for (slot, owner) in self.travelling_wave_owners.iter().enumerate() {
                        if owner.is_some_and(|owner| owner.instance_id == instance_id) {
                            self.travelling_wave_pool.get_mut(slot).set_scale_mode(mode);
                        }
                    }
                }
                Parameter::TravellingWaveWavelength(wavelength_m) => {
                    let wavelength_m = wavelength_m.clamp(MIN_WAVELENGTH_M, MAX_WAVELENGTH_M);
                    if let Some(cfg) = self.instance_config_mut(instance_id) {
                        cfg.travelling_wave.wavelength_m = wavelength_m;
                    }
                    for (slot, owner) in self.travelling_wave_owners.iter().enumerate() {
                        if owner.is_some_and(|owner| owner.instance_id == instance_id) {
                            self.travelling_wave_pool
                                .get_mut(slot)
                                .set_wavelength(wavelength_m);
                        }
                    }
                }
                Parameter::AttenuationD0(d0_m) => {
                    let d0_m = d0_m.clamp(MIN_ATTEN_D0_M, MAX_ATTEN_D0_M);
                    if let Some(cfg) = self.instance_config_mut(instance_id) {
                        cfg.distance_decay.d0_m = d0_m;
                    }
                    self.set_instance_decay(instance_id, Some(d0_m), None);
                }
                Parameter::AttenuationExponent(exponent) => {
                    let exponent = exponent.clamp(MIN_ATTEN_EXPONENT, MAX_ATTEN_EXPONENT);
                    if let Some(cfg) = self.instance_config_mut(instance_id) {
                        cfg.distance_decay.exponent = exponent;
                    }
                    self.set_instance_decay(instance_id, None, Some(exponent));
                }
            },
            EngineCommand::Panic => {
                self.wave_pool.reset_all();
                self.travelling_wave_pool.reset_all();
                self.wave_owners = [None; MAX_WAVE_STIMULI];
                self.travelling_wave_owners = [None; MAX_TRAVELLING_WAVE_STIMULI];
            }
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.command_queue.pop() {
            self.apply_command(cmd);
        }
        // Hot config reload is a fixed-size Copy through the preallocated ring:
        // no allocation or deallocation occurs on the audio thread.
        while let Ok(layout) = self.layout_queue.pop() {
            self.layout = layout;
        }
    }

    /// Synthesize one frame of all active stimuli into `output`.
    fn render_frame(&mut self, context: &ProcessContext<'_>, output: &mut [f32; TRANSDUCER_COUNT]) {
        output.fill(0.0);
        self.wave_pool.process_all(context, output);
        self.travelling_wave_pool.process_all(context, output);

        // Per-transducer gain, then safety limiting
        for (sample, &gain) in output.iter_mut().zip(self.layout.gains.iter()) {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
    }

    /// Audio-callback entry point: drains pending commands once, then fills
    /// the interleaved `data` buffer at the device rate. The wave field is
    /// rendered at `sample_rate / RENDER_DECIMATION` and linearly upsampled
    /// per device frame; the upsampler state persists across calls, so block
    /// sizes need not be multiples of the decimation factor. Writes the
    /// block RMS of each of the 32 logical transducer outputs into
    /// `levels_out` (computed pre-truncation, so levels are meaningful even
    /// on a stereo fallback device). MUST NOT block or allocate.
    pub fn process_block(
        &mut self,
        data: &mut [f32],
        channels: usize,
        sample_rate: f32,
        levels_out: &mut [f32; TRANSDUCER_COUNT],
    ) {
        self.drain_commands();

        let engine_rate = sample_rate / RENDER_DECIMATION as f32;
        // Copy so the context doesn't hold a borrow of self across render_frame
        // (the kernel is Copy — a 4 KB stack copy once per block, no alloc).
        let positions = self.layout.positions;
        let splat_kernel = *self.splat_kernel;
        let context = ProcessContext {
            sample_rate: engine_rate,
            dt: 1.0 / engine_rate,
            transducer_positions: &positions,
            table_m: self.layout.table_m,
            splat_kernel: &splat_kernel,
        };

        let mut sum_squares = [0.0f32; TRANSDUCER_COUNT];
        let frames = data.len() / channels;
        let routes = self.monitor_routes;
        for frame in data[..frames * channels].chunks_exact_mut(channels) {
            self.interp_phase += 1;
            if self.interp_phase >= RENDER_DECIMATION {
                self.interp_phase = 0;
                let mut cur = [0.0f32; TRANSDUCER_COUNT];
                self.render_frame(&context, &mut cur);
                self.history_pos = (self.history_pos + FIR_TAPS_PER_PHASE - 1) % FIR_TAPS_PER_PHASE;
                self.history[self.history_pos] = cur;
            }
            // Polyphase reconstruction: device frame at phase p is the dot
            // product of branch p of the windowed-sinc filter with the
            // FIR_TAPS_PER_PHASE most recent internal frames
            let n = channels.min(TRANSDUCER_COUNT);
            let mut interp = [0.0f32; TRANSDUCER_COUNT];
            for k in 0..FIR_TAPS_PER_PHASE {
                let coeff = self.fir[k * RENDER_DECIMATION + self.interp_phase];
                let frame_k = &self.history[(self.history_pos + k) % FIR_TAPS_PER_PHASE];
                for (out, &sample) in interp.iter_mut().zip(frame_k.iter()) {
                    *out += coeff * sample;
                }
            }
            for (sum, &sample) in sum_squares.iter_mut().zip(interp.iter()) {
                *sum += sample * sample;
            }
            // Physical outputs play their routed logical channel (identity
            // by default; a stereo device can audition any of the 32)
            for (p, sample) in frame[..n].iter_mut().enumerate() {
                // Reconstruction can overshoot even though the internal-rate
                // mix was bounded. Enforce the hardware limit at the final
                // device-rate output.
                *sample = interp[routes[p] as usize].clamp(-1.0, 1.0);
            }
            for sample in frame[n..].iter_mut() {
                *sample = 0.0;
            }
        }
        // Zero any trailing partial frame rather than leave stale samples
        for sample in data[frames * channels..].iter_mut() {
            *sample = 0.0;
        }

        let inv_frames = 1.0 / frames.max(1) as f32;
        for (level, &sum) in levels_out.iter_mut().zip(sum_squares.iter()) {
            *level = (sum * inv_frames).sqrt();
        }

        self.publish_active_voices(engine_rate);
        self.reap_finished_voices();
    }

    /// Push a snapshot of every active voice for the visualiser, each
    /// tagged with its owning `instance_id`. The viewer reconstructs the
    /// per-transducer field geometrically, so no delay array is sent. No-op
    /// (and no allocation) when nothing is playing.
    fn publish_active_voices(&mut self, sample_rate: f32) {
        let mut voices = [VoiceInfo::default(); MAX_ACTIVE_VOICES];
        let mut count = 0usize;
        for (slot, owner) in self.wave_owners.iter().enumerate() {
            let Some(owner) = owner else { continue };
            if !self.wave_pool.slot_active(slot) || count >= MAX_ACTIVE_VOICES {
                continue;
            }
            let stim = &self.wave_pool.stimuli[slot];
            voices[count] = VoiceInfo {
                instance_id: owner.instance_id,
                seq: owner.seq,
                note: owner.note,
                note_type: StimulusType::Wave,
                frequency: stim.frequency,
                wave_speed: stim.wave_speed,
                scale_mode: SpatialScaleMode::Speed,
                wavelength_m: stim.wave_speed / stim.frequency.max(f32::MIN_POSITIVE),
                atten_d0_m: stim.decay_d0.current,
                atten_exponent: stim.decay_exponent.current,
                source_pos: stim.source_pos,
                requested_pos: stim.requested_pos,
                amplitude: stim.amplitude * stim.env_level * stim.mpe.value.pressure,
            };
            count += 1;
        }
        for (slot, owner) in self.travelling_wave_owners.iter().enumerate() {
            let Some(owner) = owner else { continue };
            if !self.travelling_wave_pool.slot_active(slot) || count >= MAX_ACTIVE_VOICES {
                continue;
            }
            let stim = &self.travelling_wave_pool.stimuli[slot];
            voices[count] = VoiceInfo {
                instance_id: owner.instance_id,
                seq: owner.seq,
                note: owner.note,
                note_type: StimulusType::TravellingWave,
                frequency: stim.frequency,
                wave_speed: stim.frequency * stim.wavelength_m(),
                scale_mode: stim.scale_mode,
                wavelength_m: stim.wavelength_m(),
                atten_d0_m: stim.decay_d0.current,
                atten_exponent: stim.decay_exponent.current,
                source_pos: stim.source_pos,
                requested_pos: stim.source_pos,
                amplitude: stim.amplitude * stim.env_level * stim.mpe.value.pressure,
            };
            count += 1;
        }
        // Publish one explicit empty snapshot when the final voice ends so an
        // observer clears immediately without relying on a stale-data timeout.
        if count == 0 && self.last_voice_snapshot_count == 0 {
            return;
        }
        let count = count as u8;
        if self
            .voice_producer
            .push(VoiceSnapshot {
                sample_rate,
                count,
                voices,
            })
            .is_ok()
        {
            self.last_voice_snapshot_count = count;
        }
    }

    /// Single-frame variant used by tests: renders one *internal-rate*
    /// frame directly at `sample_rate` (no upsampling stage).
    #[cfg(test)]
    pub fn process(&mut self, output: &mut [f32; TRANSDUCER_COUNT], sample_rate: f32) {
        self.drain_commands();
        let positions = self.layout.positions;
        let splat_kernel = *self.splat_kernel;
        let context = ProcessContext {
            sample_rate,
            dt: 1.0 / sample_rate,
            transducer_positions: &positions,
            table_m: self.layout.table_m,
            splat_kernel: &splat_kernel,
        };
        self.render_frame(&context, output);
        self.reap_finished_voices();
    }
}

/// Zeroth-order modified Bessel function of the first kind (power series),
/// for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let half = x / 2.0;
    for k in 1..=30 {
        let f = half / k as f64;
        term *= f * f;
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

/// Design the polyphase reconstruction filter: a Kaiser-windowed sinc
/// lowpass with cutoff at the internal Nyquist (device_rate / (2 *
/// RENDER_DECIMATION)), scaled by RENDER_DECIMATION so each polyphase
/// branch has unity DC gain. This single filter both interpolates the
/// zero-stuffed internal-rate signal and suppresses its spectral images
/// (~100 dB at Kaiser beta 10). Runs once at engine construction.
fn design_reconstruction_fir() -> Box<[f32; FIR_LEN]> {
    let beta = 10.0f64;
    let denom = bessel_i0(beta);
    let center = (FIR_LEN - 1) as f64 / 2.0;
    let fc = 0.5 / RENDER_DECIMATION as f64; // internal Nyquist, normalised to the device rate
    let mut h = Box::new([0.0f32; FIR_LEN]);
    for (n, tap) in h.iter_mut().enumerate() {
        let t = n as f64 - center;
        let x = std::f64::consts::PI * 2.0 * fc * t;
        let sinc = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
        let r = t / center;
        let window = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / denom;
        *tap = (2.0 * fc * RENDER_DECIMATION as f64 * sinc * window) as f32;
    }
    h
}

/// Design the bandlimited scatter kernel (see `SPLAT_TAPS`): for each of
/// `SPLAT_PHASES` fractional arrival positions, a Kaiser-windowed sinc sampled
/// onto the `SPLAT_TAPS` integer cells straddling that position, normalised so
/// the deposit sums to unity. Cutoff is the internal Nyquist (unit sample
/// spacing → normalised 0.5), i.e. the full internal band. Runs once at
/// engine construction.
fn design_splat_kernel() -> Box<[f32; SPLAT_LEN]> {
    let beta = 9.0f64;
    let denom = bessel_i0(beta);
    let half = SPLAT_HALF as f64;
    let mut k = Box::new([0.0f32; SPLAT_LEN]);
    for p in 0..SPLAT_PHASES {
        let frac = p as f64 / SPLAT_PHASES as f64;
        let mut row = [0.0f64; SPLAT_TAPS];
        let mut sum = 0.0f64;
        for (t, w) in row.iter_mut().enumerate() {
            // Signed distance from the fractional arrival to cell base + (t − (HALF−1))
            let x = (t as f64 - (SPLAT_HALF - 1) as f64) - frac;
            let px = std::f64::consts::PI * x;
            let sinc = if px.abs() < 1e-9 { 1.0 } else { px.sin() / px };
            let r = x / half;
            let window = if r.abs() >= 1.0 {
                0.0
            } else {
                bessel_i0(beta * (1.0 - r * r).sqrt()) / denom
            };
            *w = sinc * window;
            sum += *w;
        }
        for (t, &w) in row.iter().enumerate() {
            k[p * SPLAT_TAPS + t] = (w / sum) as f32; // unit-sum: flat DC, no phase-dependent gain
        }
    }
    k
}

/// One-pole toward `target`; coeff derived from dt and MPE_SMOOTHING_TAU.
fn smooth_mpe(current: &mut MpeData, target: &MpeData, dt: f32) {
    let coeff = dt / (MPE_SMOOTHING_TAU + dt);
    current.pressure += (target.pressure - current.pressure) * coeff;
    current.pitch_bend += (target.pitch_bend - current.pitch_bend) * coeff;
    current.timbre += (target.timbre - current.timbre) * coeff;
}

fn lerp_mpe(a: &MpeData, b: &MpeData, t: f32) -> MpeData {
    MpeData {
        pressure: a.pressure + (b.pressure - a.pressure) * t,
        pitch_bend: a.pitch_bend + (b.pitch_bend - a.pitch_bend) * t,
        timbre: a.timbre + (b.timbre - a.timbre) * t,
    }
}

/// Turns discrete MPE updates into a smooth per-sample trajectory: each new
/// target is reached by a linear ramp over roughly the update spacing
/// (MPE_RAMP_MIN_S..MPE_RAMP_MAX_S), and the ramp output is one-pole
/// smoothed. This makes trajectory smoothness independent of how coarsely a
/// client sends updates (see MPE_RAMP_MIN_S rationale).
#[derive(Default)]
struct MpeInterp {
    /// Smoothed value used for synthesis.
    value: MpeData,
    /// First stage of the two-pole smoother (see step()).
    mid: MpeData,
    from: MpeData,
    target: MpeData,
    /// Seconds into / total length of the current ramp.
    ramp_pos: f32,
    ramp_len: f32,
    /// Seconds since the last update, measuring the client's send spacing.
    since_update: f32,
}

impl MpeInterp {
    /// Jump state straight to `mpe` (note-on: no sweep from stale values).
    fn note_on(&mut self, mpe: MpeData) {
        self.value = mpe;
        self.mid = mpe;
        self.from = mpe;
        self.target = mpe;
        self.ramp_pos = 1.0;
        self.ramp_len = 1.0;
        self.since_update = 0.0;
    }

    fn ramped(&self) -> MpeData {
        let t = if self.ramp_len > 0.0 {
            (self.ramp_pos / self.ramp_len).min(1.0)
        } else {
            1.0
        };
        lerp_mpe(&self.from, &self.target, t)
    }

    fn update(&mut self, mpe: MpeData) {
        self.from = self.ramped();
        self.target = mpe;
        self.ramp_len = self.since_update.clamp(MPE_RAMP_MIN_S, MPE_RAMP_MAX_S);
        self.ramp_pos = 0.0;
        self.since_update = 0.0;
    }

    /// Advance one sample; returns the smoothed value to synthesise with.
    /// Two cascaded one-poles: the ramp leaves velocity kinks at the update
    /// rate, and a single pole passes enough of them to remain audible as
    /// FM sidebands; the second pole buys another ~19 dB there for ~15 ms
    /// of extra position lag.
    fn step(&mut self, dt: f32) -> MpeData {
        self.since_update += dt;
        self.ramp_pos += dt;
        let ramped = self.ramped();
        smooth_mpe(&mut self.mid, &ramped, dt);
        let mid = self.mid;
        smooth_mpe(&mut self.value, &mid, dt);
        self.value
    }
}

#[derive(Clone, Copy, Default)]
struct ScalarRamp {
    current: f32,
    from: f32,
    target: f32,
    ramp_pos: f32,
    ramp_len: f32,
    since_update: f32,
}

impl ScalarRamp {
    fn jump(&mut self, value: f32) {
        self.current = value;
        self.from = value;
        self.target = value;
        self.ramp_pos = 1.0;
        self.ramp_len = 1.0;
        self.since_update = 0.0;
    }

    fn set_target(&mut self, value: f32) {
        self.from = self.current;
        self.target = value;
        self.ramp_len = self.since_update.clamp(MPE_RAMP_MIN_S, MPE_RAMP_MAX_S);
        self.ramp_pos = 0.0;
        self.since_update = 0.0;
    }

    fn step(&mut self, dt: f32) -> f32 {
        self.since_update += dt;
        self.ramp_pos += dt;
        let t = if self.ramp_len > 0.0 {
            (self.ramp_pos / self.ramp_len).min(1.0)
        } else {
            1.0
        };
        self.current = self.from + (self.target - self.from) * t;
        self.current
    }
}

// Delay line for wave propagation
/// A propagation delay line for a *moving source, fixed listener*, using
/// **scatter writes and a sequential read** (interpolating write / fixed
/// read). This is the physically correct arrangement for a moving source:
/// the delay is a function of *emission* time — a sample emitted at frame n,
/// when the source is at xₛ(n), arrives at the fixed transducer at frame
/// a(n) = n + τ(n). We therefore deposit each emitted sample into its arrival
/// slot (linearly scattered across the two neighbouring cells, accumulating)
/// and read the buffer sequentially at the current frame.
///
/// The alternative — a fixed write head with an interpolated read tap —
/// evaluates τ at *reception* time (the moving-listener model) and lets a
/// fast-approaching source drive the read tap into the write head, reading
/// the line backwards. The scatter model has no such failure: a fast approach
/// merely bunches writes (the physically correct energy/Doppler concentration,
/// giving amplitude gain as well as the frequency shift). With `source_pos`
/// held to ≤ SOURCE_SPEED_FRACTION·c (0.5·c), the arrival index advances by
/// da/dn = 1 + v_r/c ∈ [0.5, 1.5] per frame, so arrivals never invert on
/// approach and never skip a cell on recession.
///
/// Each emission is deposited with a bandlimited windowed-sinc kernel (see
/// `SPLAT_TAPS`), not a 2-tap linear splat: the linear splat's gain varies
/// with the fractional arrival phase, and as the delay sweeps that variation
/// amplitude-modulates the output into an audible granulation warble. The sinc
/// deposit has near-flat in-band gain at every phase, so the only amplitude
/// variation left is the genuine Doppler bunching gain.
struct DelayLine {
    buffer: Box<[f32; MAX_DELAY_SAMPLES]>, // Move large buffer to heap
    /// Logical-clear generation for every cell. Reset advances one integer
    /// instead of clearing 64 KiB in the audio callback; a cell from an older
    /// generation reads as zero and is initialized lazily on first touch.
    epochs: Box<[u32; MAX_DELAY_SAMPLES]>,
    epoch: u32,
    /// Clock / read pointer, advances exactly 1 per frame.
    pos: f32,
    size: usize,
}

impl DelayLine {
    fn new() -> Self {
        Self {
            buffer: Box::new([0.0; MAX_DELAY_SAMPLES]), // Allocate on heap
            epochs: Box::new([0; MAX_DELAY_SAMPLES]),
            epoch: 1,
            pos: 0.0,
            size: MAX_DELAY_SAMPLES,
        }
    }

    /// Scatter `input` (bandlimited, `kernel`) into its arrival slot
    /// `delay_samples` ahead of the read pointer, then read and consume the
    /// sample arriving at the current frame.
    fn write_and_read(&mut self, input: f32, delay_samples: f32, kernel: &[f32; SPLAT_LEN]) -> f32 {
        // A constant lookahead keeps the whole SPLAT_TAPS-wide kernel strictly
        // ahead of the integer read pointer. The upper bound keeps the longest
        // propagation delayed at the capacity limit, never wrapped past it.
        // Add a constant kernel lookahead to every physical delay. Clamping all
        // short delays to SPLAT_HALF would erase their relative phase; adding
        // the same latency preserves delay differences while keeping the whole
        // deposit ahead of the read pointer.
        let delay_samples =
            (delay_samples.max(0.0) + SPLAT_HALF as f32).min((self.size - SPLAT_TAPS - 2) as f32);

        // Bandlimited scatter: deposit `input` across SPLAT_TAPS cells straddling
        // its fractional arrival index, weighted by the windowed-sinc kernel for
        // this fractional phase (unit-sum → flat gain, no granulation ripple).
        // Accumulates, so bunched arrivals (an approaching source) superpose
        // into the correct Doppler amplitude gain.
        let arrival = self.pos + delay_samples;
        let base = arrival.floor() as usize;
        let frac = arrival - arrival.floor();
        let phase = ((frac * SPLAT_PHASES as f32) as usize).min(SPLAT_PHASES - 1);
        let koff = phase * SPLAT_TAPS;
        for t in 0..SPLAT_TAPS {
            let cell = (base + t + self.size - (SPLAT_HALF - 1)) % self.size;
            if self.epochs[cell] != self.epoch {
                self.buffer[cell] = 0.0;
                self.epochs[cell] = self.epoch;
            }
            self.buffer[cell] += input * kernel[koff + t];
        }

        // Sequential read: the sample scheduled to arrive at this frame. Zero
        // the slot after reading so the ring cell is clean for its next lap.
        let read_idx = self.pos as usize % self.size;
        let output = if self.epochs[read_idx] == self.epoch {
            self.buffer[read_idx]
        } else {
            0.0
        };
        self.buffer[read_idx] = 0.0;
        self.epochs[read_idx] = self.epoch;

        // Advance the clock (integer-valued, wraps at capacity)
        self.pos = (self.pos + 1.0) % self.size as f32;

        output
    }

    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            // Unreachable in normal operation (over four billion resets of one
            // slot), but retain correct behavior across integer wrap.
            self.epochs.as_mut().fill(0);
            self.epoch = 1;
        }
        self.pos = 0.0;
    }
}

impl Default for DelayLine {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct WaveStimulus {
    delay_lines: [DelayLine; TRANSDUCER_COUNT],

    // Source state
    frequency: f32,
    phase: f32,
    amplitude: f32,
    /// Effective source position: chases `requested_pos` at no more than
    /// SOURCE_SPEED_FRACTION of the wave speed.
    source_pos: (f32, f32),
    /// Position requested by (smoothed) MPE.
    requested_pos: (f32, f32),
    /// Snap `source_pos` to the requested position on the next process
    /// call (set at note-on so a new note doesn't sweep in from wherever
    /// the slot's previous voice left off).
    snap_position: bool,
    wave_speed: f32, // Individual wave speed for this stimulus
    decay_d0: ScalarRamp,
    decay_exponent: ScalarRamp,

    // Envelope
    env_state: EnvelopeState,
    env_level: f32,
    env_time: f32,
    release_start_level: f32,

    /// Frames until the latest possible scattered arrival has been consumed.
    /// The source envelope may be idle while physically emitted energy remains
    /// in flight in the per-transducer delay lines.
    tail_frames_remaining: usize,

    // Discrete controller updates -> smooth per-sample MPE trajectory
    mpe: MpeInterp,
}

#[derive(Default, PartialEq)]
enum EnvelopeState {
    #[default]
    Idle,
    Attack,
    Sustain,
    Release,
}

#[inline]
fn step_envelope(
    state: &mut EnvelopeState,
    level: &mut f32,
    time: &mut f32,
    release_start_level: f32,
    dt: f32,
) {
    match state {
        EnvelopeState::Idle => {}
        EnvelopeState::Attack => {
            *time += dt;
            *level = (*time * 10.0).min(1.0);
            if *level >= 1.0 {
                *state = EnvelopeState::Sustain;
            }
        }
        EnvelopeState::Sustain => *level = 1.0,
        EnvelopeState::Release => {
            *time += dt;
            *level = release_start_level * (1.0 - *time * 2.0).max(0.0);
            if *level <= 0.0 {
                *state = EnvelopeState::Idle;
            }
        }
    }
}

#[inline]
fn begin_release(
    state: &mut EnvelopeState,
    level: f32,
    time: &mut f32,
    release_start_level: &mut f32,
) {
    if *state != EnvelopeState::Idle {
        *state = EnvelopeState::Release;
        *time = 0.0;
        *release_start_level = level;
    }
}

#[inline]
fn mpe_source_position(mpe: MpeData, table_m: (f32, f32)) -> (f32, f32) {
    (
        (0.5 + 0.5 * mpe.pitch_bend.clamp(-1.0, 1.0)) * table_m.0,
        mpe.timbre.clamp(0.0, 1.0) * table_m.1,
    )
}

#[inline]
fn advance_oscillator_phase(phase: &mut f32, frequency: f32, dt: f32) {
    *phase += frequency * dt;
    if *phase >= 1.0 {
        *phase -= phase.floor();
    }
}

impl Stimulus for WaveStimulus {
    fn process(&mut self, ctx: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT] {
        let mut output = [0.0; TRANSDUCER_COUNT];

        if self.env_state == EnvelopeState::Idle && self.tail_frames_remaining == 0 {
            return output;
        }
        step_envelope(
            &mut self.env_state,
            &mut self.env_level,
            &mut self.env_time,
            self.release_start_level,
            ctx.dt,
        );

        // Ramp + smooth MPE toward the latest controller values
        let mpe = self.mpe.step(ctx.dt);
        let decay = DistanceDecay {
            d0_m: self.decay_d0.step(ctx.dt),
            exponent: self.decay_exponent.step(ctx.dt),
        };

        // Requested source position from MPE, spanning the whole table:
        // pitch bend -1..1 -> x across the width, timbre 0..1 -> y along
        // the length (bend 0 / timbre 0.5 is the table centre)
        self.requested_pos = mpe_source_position(mpe, ctx.table_m);

        // The effective source chases the requested position at no more than
        // SOURCE_SPEED_FRACTION of the wave speed, keeping arrival indices in
        // the scatter-write delay lines monotonic and gap-free (see DelayLine)
        if self.snap_position {
            self.snap_position = false;
            self.source_pos = self.requested_pos;
        } else {
            let dx = self.requested_pos.0 - self.source_pos.0;
            let dy = self.requested_pos.1 - self.source_pos.1;
            let dist = (dx * dx + dy * dy).sqrt();
            let max_step = SOURCE_SPEED_FRACTION * self.wave_speed * ctx.dt;
            if dist <= max_step {
                self.source_pos = self.requested_pos;
            } else {
                let scale = max_step / dist;
                self.source_pos.0 += dx * scale;
                self.source_pos.1 += dy * scale;
            }
        }

        // Generate source signal
        let source = (self.phase * 2.0 * std::f32::consts::PI).sin()
            * self.amplitude
            * self.env_level
            * mpe.pressure;
        let source_active = self.env_state != EnvelopeState::Idle;
        let mut latest_arrival_frames = 0usize;

        // Process through delay lines
        for (i, &transducer_pos) in ctx.transducer_positions.iter().enumerate() {
            let dx = transducer_pos.0 - self.source_pos.0;
            let dy = transducer_pos.1 - self.source_pos.1;
            let distance = (dx * dx + dy * dy).sqrt();

            let delay_time = distance / self.wave_speed.max(MIN_WAVE_SPEED); // per-stimulus wave speed, floor avoids div by zero
            let delay_samples = delay_time * ctx.sample_rate;
            latest_arrival_frames = latest_arrival_frames.max(
                (delay_samples.ceil().max(0.0) as usize + SPLAT_TAPS + SPLAT_HALF)
                    .min(MAX_DELAY_SAMPLES - 1),
            );

            // Distance attenuation is applied at emission (each wavefront
            // carries its own emission-time attenuation into the delay line);
            // the sequential read is then raw. Doppler amplitude gain from
            // bunched arrivals rides on top of this geometric spreading loss.
            let emitted = source * distance_gain(distance, decay);
            output[i] =
                self.delay_lines[i].write_and_read(emitted, delay_samples, ctx.splat_kernel);
        }

        if source_active {
            self.tail_frames_remaining = latest_arrival_frames;
        } else {
            self.tail_frames_remaining = self.tail_frames_remaining.saturating_sub(1);
        }

        // Update phase
        advance_oscillator_phase(&mut self.phase, self.frequency, ctx.dt);

        output
    }

    fn is_active(&self) -> bool {
        self.env_state != EnvelopeState::Idle || self.tail_frames_remaining > 0
    }

    fn is_releasing(&self) -> bool {
        self.env_state == EnvelopeState::Release
            || (self.env_state == EnvelopeState::Idle && self.tail_frames_remaining > 0)
    }

    fn note_on(&mut self, frequency: f32, velocity: u8, mpe: MpeData) {
        self.frequency = frequency;
        self.amplitude = velocity as f32 / 127.0;
        self.mpe.note_on(mpe);
        self.snap_position = true;
        self.env_state = EnvelopeState::Attack;
        self.env_time = 0.0;
        self.release_start_level = 0.0;
        self.tail_frames_remaining = 0;
        self.wave_speed = DEFAULT_WAVE_SPEED; // Overridden by set_wave_speed after note_on
        self.decay_d0.jump(DEFAULT_ATTEN_D0_M);
        self.decay_exponent.jump(DEFAULT_ATTEN_EXPONENT);
    }

    fn note_off(&mut self) {
        begin_release(
            &mut self.env_state,
            self.env_level,
            &mut self.env_time,
            &mut self.release_start_level,
        );
    }

    fn mpe_update(&mut self, mpe: MpeData) {
        self.mpe.update(mpe);
    }

    fn reset(&mut self) {
        for line in &mut self.delay_lines {
            line.reset();
        }
        self.phase = 0.0;
        self.snap_position = true;
        self.mpe = MpeInterp::default();
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
        self.release_start_level = 0.0;
        self.tail_frames_remaining = 0;
        self.wave_speed = DEFAULT_WAVE_SPEED;
        self.decay_d0.jump(DEFAULT_ATTEN_D0_M);
        self.decay_exponent.jump(DEFAULT_ATTEN_EXPONENT);
    }

    fn set_wave_speed(&mut self, wave_speed: f32) {
        self.wave_speed = wave_speed;
    }

    fn set_distance_decay(&mut self, decay: DistanceDecay) {
        self.decay_d0.set_target(decay.d0_m);
        self.decay_exponent.set_target(decay.exponent);
    }
}

impl WaveStimulus {
    fn configure_distance_decay(&mut self, decay: DistanceDecay) {
        self.decay_d0.jump(decay.d0_m);
        self.decay_exponent.jump(decay.exponent);
    }
}

/// Instantaneous radial travelling wave. Unlike `WaveStimulus`, this owns no
/// delay lines: position and spatial-scale changes alter the field on the next
/// internal render frame without propagation history, Doppler, or a tail.
#[derive(Default)]
pub struct TravellingWaveStimulus {
    frequency: f32,
    phase: f32,
    amplitude: f32,
    source_pos: (f32, f32),
    scale_mode: SpatialScaleMode,
    wave_speed: f32,
    configured_wavelength_m: f32,
    wavenumber: ScalarRamp,
    decay_d0: ScalarRamp,
    decay_exponent: ScalarRamp,
    env_state: EnvelopeState,
    env_level: f32,
    env_time: f32,
    release_start_level: f32,
    mpe: MpeInterp,
}

impl TravellingWaveStimulus {
    fn target_wavenumber(&self) -> f32 {
        let wavelength = effective_wavelength(
            self.frequency,
            self.wave_speed,
            self.scale_mode,
            self.configured_wavelength_m,
        );
        std::f32::consts::TAU / wavelength.max(MIN_WAVELENGTH_M)
    }

    fn configure(&mut self, config: TravellingWaveConfig, decay: DistanceDecay, immediate: bool) {
        self.scale_mode = config.scale_mode;
        self.wave_speed = config.wave_speed.clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED);
        self.configured_wavelength_m = config
            .wavelength_m
            .clamp(MIN_WAVELENGTH_M, MAX_WAVELENGTH_M);
        let k = self.target_wavenumber();
        if immediate {
            self.wavenumber.jump(k);
            self.decay_d0.jump(decay.d0_m);
            self.decay_exponent.jump(decay.exponent);
        } else {
            self.wavenumber.set_target(k);
            self.decay_d0.set_target(decay.d0_m);
            self.decay_exponent.set_target(decay.exponent);
        }
    }

    fn set_scale_mode(&mut self, mode: SpatialScaleMode) {
        self.scale_mode = mode;
        self.wavenumber.set_target(self.target_wavenumber());
    }

    fn set_wavelength(&mut self, wavelength_m: f32) {
        self.configured_wavelength_m = wavelength_m.clamp(MIN_WAVELENGTH_M, MAX_WAVELENGTH_M);
        if self.scale_mode == SpatialScaleMode::Wavelength {
            self.wavenumber.set_target(self.target_wavenumber());
        }
    }

    fn wavelength_m(&self) -> f32 {
        std::f32::consts::TAU / self.wavenumber.current.max(f32::MIN_POSITIVE)
    }
}

impl Stimulus for TravellingWaveStimulus {
    fn process(&mut self, ctx: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT] {
        let mut output = [0.0; TRANSDUCER_COUNT];

        if self.env_state == EnvelopeState::Idle {
            return output;
        }
        step_envelope(
            &mut self.env_state,
            &mut self.env_level,
            &mut self.env_time,
            self.release_start_level,
            ctx.dt,
        );

        let mpe = self.mpe.step(ctx.dt);
        self.source_pos = mpe_source_position(mpe, ctx.table_m);
        let k = self.wavenumber.step(ctx.dt);
        let decay = DistanceDecay {
            d0_m: self.decay_d0.step(ctx.dt),
            exponent: self.decay_exponent.step(ctx.dt),
        };

        let gain = self.amplitude * self.env_level * mpe.pressure;
        let theta = self.phase * std::f32::consts::TAU;
        for (sample, &pos) in output.iter_mut().zip(ctx.transducer_positions.iter()) {
            let dx = pos.0 - self.source_pos.0;
            let dy = pos.1 - self.source_pos.1;
            let distance = (dx * dx + dy * dy).sqrt();
            *sample = gain * distance_gain(distance, decay) * (theta - k * distance).sin();
        }

        advance_oscillator_phase(&mut self.phase, self.frequency, ctx.dt);

        output
    }

    fn is_active(&self) -> bool {
        self.env_state != EnvelopeState::Idle
    }

    fn is_releasing(&self) -> bool {
        self.env_state == EnvelopeState::Release
    }

    fn note_on(&mut self, frequency: f32, velocity: u8, mpe: MpeData) {
        self.frequency = frequency;
        self.amplitude = velocity as f32 / 127.0;
        self.mpe.note_on(mpe);
        self.wave_speed = DEFAULT_WAVE_SPEED;
        self.configured_wavelength_m = haptic_protocol::DEFAULT_WAVELENGTH_M;
        self.scale_mode = SpatialScaleMode::Speed;
        self.wavenumber.jump(self.target_wavenumber());
        self.decay_d0.jump(DEFAULT_ATTEN_D0_M);
        self.decay_exponent.jump(DEFAULT_ATTEN_EXPONENT);
        self.env_state = EnvelopeState::Attack;
        self.env_time = 0.0;
        self.release_start_level = 0.0;
    }

    fn note_off(&mut self) {
        begin_release(
            &mut self.env_state,
            self.env_level,
            &mut self.env_time,
            &mut self.release_start_level,
        );
    }

    fn mpe_update(&mut self, mpe: MpeData) {
        self.mpe.update(mpe);
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
        self.release_start_level = 0.0;
        self.mpe = MpeInterp::default();
        self.source_pos = (0.0, 0.0);
        self.wave_speed = DEFAULT_WAVE_SPEED;
        self.configured_wavelength_m = haptic_protocol::DEFAULT_WAVELENGTH_M;
        self.scale_mode = SpatialScaleMode::Speed;
        self.wavenumber.jump(1.0);
        self.decay_d0.jump(DEFAULT_ATTEN_D0_M);
        self.decay_exponent.jump(DEFAULT_ATTEN_EXPONENT);
    }

    fn set_wave_speed(&mut self, wave_speed: f32) {
        self.wave_speed = wave_speed.clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED);
        if self.scale_mode == SpatialScaleMode::Speed {
            self.wavenumber.set_target(self.target_wavenumber());
        }
    }

    fn set_distance_decay(&mut self, decay: DistanceDecay) {
        self.decay_d0.set_target(decay.d0_m);
        self.decay_exponent.set_target(decay.exponent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 48000.0;

    fn full_mpe() -> MpeData {
        MpeData {
            pressure: 1.0,
            pitch_bend: 0.0,
            timbre: 0.5,
        }
    }

    fn run_samples(engine: &mut StimulusEngine, n: usize) -> f32 {
        let mut peak = 0.0f32;
        let mut output = [0.0f32; TRANSDUCER_COUNT];
        for _ in 0..n {
            engine.process(&mut output, SAMPLE_RATE);
            for &s in output.iter() {
                peak = peak.max(s.abs());
            }
        }
        peak
    }

    fn send(producer: &mut rtrb::Producer<EngineCommand>, cmd: EngineCommand) {
        producer.push(cmd).unwrap();
    }

    fn active_wave_voices(engine: &StimulusEngine) -> usize {
        engine.wave_owners.iter().flatten().count()
    }

    fn active_travelling_wave_voices(engine: &StimulusEngine) -> usize {
        engine.travelling_wave_owners.iter().flatten().count()
    }

    #[test]
    fn frequency_mapping_targets_haptic_band() {
        // Standard equal-tempered frequencies are unchanged inside the band.
        let c0 = note_to_haptic_frequency(24);
        let c1 = note_to_haptic_frequency(36);
        assert!((c0 - 32.703).abs() < 0.01);
        assert!((c1 / c0 - 2.0).abs() < 1e-3);
        assert!((note_to_haptic_frequency(DEFAULT_TEST_NOTE) - 65.406).abs() < 0.01);
        // MIDI 60 / Ableton C3 is above the haptic band and saturates.
        assert_eq!(note_to_haptic_frequency(60), MAX_HAPTIC_FREQ);
        // Extremes clamp to the transducer band
        assert_eq!(note_to_haptic_frequency(0), 20.0);
        assert_eq!(note_to_haptic_frequency(127), 200.0);
    }

    #[test]
    fn note_on_produces_output_and_note_off_releases() {
        let (mut engine, mut producer, _layout_producer, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );

        // 200ms: through the attack, into sustain
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert!(peak > 0.0, "expected audible output after note on");
        assert_eq!(active_wave_voices(&engine), 1);

        send(
            &mut producer,
            EngineCommand::NoteOff {
                instance_id: 0,
                note: 60,
                channel: 1,
            },
        );
        // 700ms: past the 500ms release
        run_samples(&mut engine, (0.7 * SAMPLE_RATE) as usize);
        assert_eq!(
            active_wave_voices(&engine),
            0,
            "voice should be reaped after release"
        );

        let residual = run_samples(&mut engine, 1024);
        assert_eq!(residual, 0.0, "released voice should be silent");
    }

    #[test]
    fn note_off_only_affects_matching_note_and_channel() {
        let (mut engine, mut producer, _layout_producer, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 64,
                velocity: 100,
                channel: 2,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 256);
        assert_eq!(active_wave_voices(&engine), 2);

        // Wrong channel: no effect
        send(
            &mut producer,
            EngineCommand::NoteOff {
                instance_id: 0,
                note: 60,
                channel: 2,
            },
        );
        run_samples(&mut engine, (0.7 * SAMPLE_RATE) as usize);
        assert_eq!(active_wave_voices(&engine), 2);

        send(
            &mut producer,
            EngineCommand::NoteOff {
                instance_id: 0,
                note: 60,
                channel: 1,
            },
        );
        run_samples(&mut engine, (0.7 * SAMPLE_RATE) as usize);
        assert_eq!(active_wave_voices(&engine), 1);
    }

    #[test]
    fn voice_stealing_prefers_releasing_then_oldest() {
        let (mut engine, mut producer, _layout_producer, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        for note in 0..MAX_WAVE_STIMULI as u8 {
            send(
                &mut producer,
                EngineCommand::NoteOn {
                    instance_id: 0,
                    note: 40 + note,
                    velocity: 100,
                    channel: note,
                    mpe: full_mpe(),
                },
            );
        }
        run_samples(&mut engine, 64);
        assert_eq!(active_wave_voices(&engine), MAX_WAVE_STIMULI);

        // Release note 42 (channel 2), then allocate over capacity
        send(
            &mut producer,
            EngineCommand::NoteOff {
                instance_id: 0,
                note: 42,
                channel: 2,
            },
        );
        run_samples(&mut engine, 64); // still releasing, still owned
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 90,
                velocity: 100,
                channel: 10,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 64);

        assert_eq!(active_wave_voices(&engine), MAX_WAVE_STIMULI);
        let notes: Vec<u8> = engine
            .wave_owners
            .iter()
            .flatten()
            .map(|o| o.note)
            .collect();
        assert!(notes.contains(&90), "new note should have a voice");
        assert!(
            !notes.contains(&42),
            "releasing voice should have been stolen"
        );

        // Pool still full, nothing releasing: the oldest (note 40) is stolen
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 91,
                velocity: 100,
                channel: 11,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 64);
        let notes: Vec<u8> = engine
            .wave_owners
            .iter()
            .flatten()
            .map(|o| o.note)
            .collect();
        assert!(notes.contains(&91));
        assert!(!notes.contains(&40), "oldest voice should have been stolen");
    }

    #[test]
    fn mpe_update_reaches_owning_stimulus_smoothly() {
        let (mut engine, mut producer, _layout_producer, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        let mut quiet = full_mpe();
        quiet.pressure = 0.0;
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 3,
                mpe: quiet,
            },
        );

        // Past the attack with zero pressure: silent
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert_eq!(peak, 0.0, "zero pressure should be silent");

        // Press: output should fade in via smoothing. The window must cover
        // wave propagation to the nearest transducer (~0.13 m at 100 m/s
        // with the default cell-centred layout ≈ 1.3 ms).
        send(
            &mut producer,
            EngineCommand::MpeUpdate {
                instance_id: 0,
                channel: 3,
                mpe: full_mpe(),
            },
        );
        let early = run_samples(&mut engine, (0.01 * SAMPLE_RATE) as usize); // 10 ms
        let later = run_samples(&mut engine, (0.1 * SAMPLE_RATE) as usize);
        assert!(early > 0.0, "pressure should start taking effect");
        assert!(later > early, "smoothed pressure should keep rising");

        // Update on a different channel must not affect this voice
        let mut half = full_mpe();
        half.pressure = 0.5;
        send(
            &mut producer,
            EngineCommand::MpeUpdate {
                instance_id: 0,
                channel: 5,
                mpe: half,
            },
        );
        run_samples(&mut engine, (0.1 * SAMPLE_RATE) as usize);
        let target = engine.wave_pool.stimuli[0].mpe.target;
        assert_eq!(
            target.pressure, 1.0,
            "other channel's update must not leak in"
        );
    }

    #[test]
    fn source_position_is_velocity_limited() {
        // Note-on at the table origin, then request a jump to the far
        // corner: the effective source must snap at note-on, then travel
        // at no more than SOURCE_SPEED_FRACTION x wave speed, eventually
        // converging on the requested position.
        let origin = MpeData {
            pressure: 1.0,
            pitch_bend: -1.0,
            timbre: 0.0,
        };
        let far = MpeData {
            pressure: 1.0,
            pitch_bend: 1.0,
            timbre: 1.0,
        };
        let (mut engine, mut producer, _lp, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: origin,
            },
        );
        run_samples(&mut engine, 64);
        let start = engine.wave_pool.stimuli[0].source_pos;
        assert!(
            start.0.abs() < 1e-3 && start.1.abs() < 1e-3,
            "note-on snaps to the requested position"
        );

        send(
            &mut producer,
            EngineCommand::MpeUpdate {
                instance_id: 0,
                channel: 1,
                mpe: far,
            },
        );
        let window_s = 0.05;
        run_samples(&mut engine, (window_s * SAMPLE_RATE) as usize);
        let pos = engine.wave_pool.stimuli[0].source_pos;
        let travelled = (pos.0 * pos.0 + pos.1 * pos.1).sqrt();
        let limit = SOURCE_SPEED_FRACTION * DEFAULT_WAVE_SPEED * window_s;
        assert!(
            travelled <= limit * 1.01,
            "moved {travelled} m, limit {limit} m"
        );
        // The requested position is ~2.24 m away, well beyond the limit, so
        // a limited source should be pinned at (close to) full speed
        assert!(
            travelled >= limit * 0.9,
            "moved only {travelled} m of the allowed {limit} m"
        );
        // The requested target is most of the way to the far corner (the
        // ramp + two-pole MPE smoothing takes ~100 ms to fully converge)
        let req = engine.wave_pool.stimuli[0].requested_pos;
        assert!(
            req.0 > 0.75 && req.1 > 1.5,
            "requested position should be well on its way, at {req:?}"
        );

        // Given enough time, the effective source converges on the target
        run_samples(&mut engine, (0.3 * SAMPLE_RATE) as usize);
        let pos = engine.wave_pool.stimuli[0].source_pos;
        assert!(
            (pos.0 - 1.0).abs() < 1e-2 && (pos.1 - 2.0).abs() < 1e-2,
            "source should converge, at {pos:?}"
        );
    }

    #[test]
    fn set_parameter_patches_instance_config_and_applies_at_note_on() {
        let (mut engine, mut producer, _layout_producer, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 7,
                parameter: Parameter::StimulusType(StimulusType::TravellingWave),
            },
        );
        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 7,
                parameter: Parameter::WaveSpeed(250.0),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 7,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 64);

        // The instance's config drove note allocation into the TW pool.
        assert_eq!(active_wave_voices(&engine), 0);
        assert_eq!(engine.travelling_wave_owners.iter().flatten().count(), 1);
        assert_eq!(engine.instance_config(7).wave_speed, 250.0);

        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 7,
                parameter: Parameter::StimulusType(StimulusType::Wave),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 7,
                note: 62,
                velocity: 100,
                channel: 2,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 64);
        assert_eq!(active_wave_voices(&engine), 1);
        assert_eq!(
            engine.wave_pool.stimuli[0].wave_speed, 250.0,
            "instance wave speed should apply at note on"
        );
    }

    #[test]
    fn instances_have_independent_config_and_voice_identity() {
        // Two instances register different wave speeds; the same (channel,
        // note) from each must be an independent voice using its own config,
        // and a note-off from one must not silence the other.
        let (mut engine, mut producer, _lp, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::RegisterInstance {
                instance_id: 100,
                config: InstanceConfig {
                    stimulus_type: StimulusType::Wave,
                    wave_speed: 5.0,
                    ..InstanceConfig::default()
                },
            },
        );
        send(
            &mut producer,
            EngineCommand::RegisterInstance {
                instance_id: 200,
                config: InstanceConfig {
                    stimulus_type: StimulusType::Wave,
                    wave_speed: 300.0,
                    ..InstanceConfig::default()
                },
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 100,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 200,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 64);
        assert_eq!(
            active_wave_voices(&engine),
            2,
            "same (channel, note) from two instances are two voices"
        );

        // Each voice captured its own instance's wave speed.
        let speeds: Vec<f32> = (0..MAX_WAVE_STIMULI)
            .filter(|&s| engine.wave_owners[s].is_some())
            .map(|s| engine.wave_pool.stimuli[s].wave_speed)
            .collect();
        assert!(
            speeds.contains(&5.0) && speeds.contains(&300.0),
            "got {speeds:?}"
        );

        // Note-off from instance 100 releases only its voice.
        send(
            &mut producer,
            EngineCommand::NoteOff {
                instance_id: 100,
                note: 60,
                channel: 1,
            },
        );
        run_samples(&mut engine, 64);
        let releasing: Vec<bool> = (0..MAX_WAVE_STIMULI)
            .filter(|&s| engine.wave_owners[s].is_some())
            .map(|s| engine.wave_pool.slot_releasing(s))
            .collect();
        assert!(
            releasing.contains(&true) && releasing.contains(&false),
            "exactly one voice releasing: {releasing:?}"
        );
    }

    #[test]
    fn scatter_lookahead_preserves_relative_short_delays() {
        let kernel = design_splat_kernel();
        let mut zero_delay = DelayLine::new();
        let mut two_sample_delay = DelayLine::new();
        let mut zero_out = [0.0f32; 12];
        let mut two_out = [0.0f32; 12];
        for n in 0..zero_out.len() {
            let input = if n == 0 { 1.0 } else { 0.0 };
            zero_out[n] = zero_delay.write_and_read(input, 0.0, &kernel);
            two_out[n] = two_sample_delay.write_and_read(input, 2.0, &kernel);
        }
        let zero_peak = zero_out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        let two_peak = two_out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert_eq!(two_peak - zero_peak, 2);
    }

    #[test]
    fn delay_line_reset_logically_discards_pending_samples() {
        let kernel = design_splat_kernel();
        let mut line = DelayLine::new();
        line.write_and_read(1.0, 20.0, &kernel);
        let old_epoch = line.epoch;
        line.reset();
        assert_ne!(line.epoch, old_epoch);
        for _ in 0..40 {
            assert_eq!(line.write_and_read(0.0, 20.0, &kernel), 0.0);
        }
    }

    #[test]
    fn wave_voice_drains_propagation_tail_after_envelope_release() {
        let kernel = design_splat_kernel();
        let positions = [(1.0, 0.0); TRANSDUCER_COUNT];
        let context = ProcessContext {
            sample_rate: 1_500.0,
            dt: 1.0 / 1_500.0,
            transducer_positions: &positions,
            table_m: (1.0, 2.0),
            splat_kernel: &kernel,
        };
        let mut stimulus = WaveStimulus::default();
        stimulus.note_on(
            40.0,
            127,
            MpeData {
                pressure: 1.0,
                pitch_bend: -1.0,
                timbre: 0.0,
            },
        );
        stimulus.set_wave_speed(1.0);
        for _ in 0..75 {
            stimulus.process(&context);
        }
        stimulus.note_off();
        for _ in 0..900 {
            stimulus.process(&context);
        }
        assert!(stimulus.env_state == EnvelopeState::Idle);
        assert!(
            stimulus.is_active(),
            "in-flight arrivals must keep the voice alive"
        );

        let mut late_peak = 0.0f32;
        for _ in 0..1_200 {
            late_peak = late_peak.max(stimulus.process(&context)[0].abs());
        }
        assert!(
            late_peak > 1e-3,
            "expected delayed release energy, peak={late_peak}"
        );
    }

    #[test]
    fn note_off_during_attack_releases_from_current_level() {
        let kernel = design_splat_kernel();
        let positions = [(0.0, 0.0); TRANSDUCER_COUNT];
        let context = ProcessContext {
            sample_rate: 1_000.0,
            dt: 0.001,
            transducer_positions: &positions,
            table_m: (1.0, 2.0),
            splat_kernel: &kernel,
        };
        let mut stimulus = TravellingWaveStimulus::default();
        stimulus.note_on(40.0, 127, full_mpe());
        for _ in 0..50 {
            stimulus.process(&context);
        }
        let before = stimulus.env_level;
        stimulus.note_off();
        stimulus.process(&context);
        assert!(stimulus.env_level <= before);
        assert!(stimulus.env_level > before * 0.99);
    }

    #[test]
    fn travelling_wave_matches_closed_form_radial_field() {
        let kernel = design_splat_kernel();
        let positions = TransducerLayout::default().positions;
        let context = ProcessContext {
            sample_rate: 1_500.0,
            dt: 1.0 / 1_500.0,
            transducer_positions: &positions,
            table_m: (1.0, 2.0),
            splat_kernel: &kernel,
        };
        let mut stimulus = TravellingWaveStimulus::default();
        stimulus.note_on(100.0, 127, full_mpe());
        stimulus.configure(
            TravellingWaveConfig {
                scale_mode: SpatialScaleMode::Wavelength,
                wave_speed: 20.0,
                wavelength_m: 0.5,
            },
            DistanceDecay::default(),
            true,
        );
        stimulus.env_state = EnvelopeState::Sustain;
        stimulus.phase = 0.25;
        let output = stimulus.process(&context);
        for (sample, &(x, y)) in output.iter().zip(positions.iter()) {
            let dx = x - 0.5;
            let dy = y - 1.0;
            let distance = (dx * dx + dy * dy).sqrt();
            let expected = distance_gain(distance, DistanceDecay::default())
                * (std::f32::consts::FRAC_PI_2 - std::f32::consts::TAU * distance / 0.5).sin();
            assert!((sample - expected).abs() < 1e-5, "{sample} != {expected}");
        }
    }

    #[test]
    fn fixed_wavelength_field_is_frequency_independent_at_equal_phase() {
        let kernel = design_splat_kernel();
        let positions = [(0.75, 1.0); TRANSDUCER_COUNT];
        let context = ProcessContext {
            sample_rate: 1_500.0,
            dt: 1.0 / 1_500.0,
            transducer_positions: &positions,
            table_m: (1.0, 2.0),
            splat_kernel: &kernel,
        };
        let make = |frequency| {
            let mut stimulus = TravellingWaveStimulus::default();
            stimulus.note_on(frequency, 127, full_mpe());
            stimulus.configure(
                TravellingWaveConfig {
                    scale_mode: SpatialScaleMode::Wavelength,
                    wave_speed: 20.0,
                    wavelength_m: 0.4,
                },
                DistanceDecay::default(),
                true,
            );
            stimulus.env_state = EnvelopeState::Sustain;
            stimulus.phase = 0.125;
            stimulus
        };
        let mut low = make(50.0);
        let mut high = make(150.0);
        assert!((low.process(&context)[0] - high.process(&context)[0]).abs() < 1e-6);
        assert!((low.frequency * low.wavelength_m() - 20.0).abs() < 1e-5);
        assert!((high.frequency * high.wavelength_m() - 60.0).abs() < 1e-5);
    }

    #[test]
    fn travelling_wave_scale_changes_ramp_without_retrigger_or_tail() {
        let kernel = design_splat_kernel();
        let positions = [(0.75, 1.0); TRANSDUCER_COUNT];
        let context = ProcessContext {
            sample_rate: 1_500.0,
            dt: 1.0 / 1_500.0,
            transducer_positions: &positions,
            table_m: (1.0, 2.0),
            splat_kernel: &kernel,
        };
        let mut stimulus = TravellingWaveStimulus::default();
        stimulus.note_on(100.0, 127, full_mpe());
        stimulus.configure(
            TravellingWaveConfig::default(),
            DistanceDecay::default(),
            true,
        );
        for _ in 0..100 {
            stimulus.process(&context);
        }
        let old_k = stimulus.wavenumber.current;
        stimulus.set_wave_speed(10.0);
        assert_eq!(stimulus.wavenumber.current, old_k);
        for _ in 0..10 {
            stimulus.process(&context);
        }
        assert!(stimulus.wavenumber.current > old_k);
        assert!(stimulus.wavenumber.current < stimulus.wavenumber.target);

        stimulus.note_off();
        for _ in 0..800 {
            stimulus.process(&context);
        }
        assert!(
            !stimulus.is_active(),
            "TW release must not retain a propagation tail"
        );
    }

    #[test]
    fn travelling_wave_source_follows_smoothed_mpe_without_velocity_chase() {
        let kernel = design_splat_kernel();
        let positions = [(0.0, 0.0); TRANSDUCER_COUNT];
        let context = ProcessContext {
            sample_rate: 1_500.0,
            dt: 1.0 / 1_500.0,
            transducer_positions: &positions,
            table_m: (1.0, 2.0),
            splat_kernel: &kernel,
        };
        let mut stimulus = TravellingWaveStimulus::default();
        stimulus.note_on(100.0, 127, full_mpe());
        stimulus.configure(
            TravellingWaveConfig::default(),
            DistanceDecay::default(),
            true,
        );
        stimulus.process(&context);
        stimulus.mpe_update(MpeData {
            pressure: 1.0,
            pitch_bend: 1.0,
            timbre: 1.0,
        });
        stimulus.process(&context);
        let smoothed = stimulus.mpe.value;
        let expected = (0.5 + 0.5 * smoothed.pitch_bend, 2.0 * smoothed.timbre);
        assert!((stimulus.source_pos.0 - expected.0).abs() < 1e-6);
        assert!((stimulus.source_pos.1 - expected.1).abs() < 1e-6);
    }

    #[test]
    fn travelling_wave_pool_is_bounded_steals_and_disconnects_cleanly() {
        let (mut engine, mut producer, _, _) = StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::RegisterInstance {
                instance_id: 77,
                config: InstanceConfig {
                    stimulus_type: StimulusType::TravellingWave,
                    ..InstanceConfig::default()
                },
            },
        );
        for note in 40..=48 {
            send(
                &mut producer,
                EngineCommand::NoteOn {
                    instance_id: 77,
                    note,
                    velocity: 100,
                    channel: note - 40,
                    mpe: full_mpe(),
                },
            );
        }
        let peak = run_samples(&mut engine, 512);
        assert!(peak.is_finite() && peak <= 1.0);
        assert_eq!(
            active_travelling_wave_voices(&engine),
            MAX_TRAVELLING_WAVE_STIMULI
        );
        let notes: Vec<u8> = engine
            .travelling_wave_owners
            .iter()
            .flatten()
            .map(|owner| owner.note)
            .collect();
        assert!(notes.contains(&48));
        assert!(!notes.contains(&40), "oldest TW voice should be stolen");

        send(
            &mut producer,
            EngineCommand::DisconnectInstance { instance_id: 77 },
        );
        run_samples(&mut engine, 1);
        assert!(engine
            .travelling_wave_pool
            .stimuli
            .iter()
            .filter(|stimulus| stimulus.is_active())
            .all(|stimulus| stimulus.is_releasing()));
        run_samples(&mut engine, SAMPLE_RATE as usize);
        assert_eq!(active_travelling_wave_voices(&engine), 0);
    }

    #[test]
    fn disconnect_releases_owned_voices_and_frees_instance_slot() {
        let (mut engine, mut producer, _, _) = StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::RegisterInstance {
                instance_id: 123,
                config: InstanceConfig::default(),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 123,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 1);
        assert_eq!(active_wave_voices(&engine), 1);
        assert!(engine.instances.iter().flatten().any(|(id, _)| *id == 123));

        send(
            &mut producer,
            EngineCommand::DisconnectInstance { instance_id: 123 },
        );
        run_samples(&mut engine, 1);
        assert!(engine.wave_pool.stimuli[0].is_releasing());
        assert!(!engine.instances.iter().flatten().any(|(id, _)| *id == 123));

        run_samples(&mut engine, SAMPLE_RATE as usize);
        assert_eq!(active_wave_voices(&engine), 0);
    }

    #[test]
    fn long_propagation_delays_are_delayed_not_wrapped() {
        // Full negative bend + zero timbre puts the source at the table
        // origin (0, 0); the far corner transducer (ch 31 at (0.875,
        // 1.875)) is ~2.07 m away -> ~103 ms at the default 20 m/s =
        // ~4966 samples. The old 4800-sample delay line overflowed here
        // and read the just-written sample instead (zero delay).
        let mpe = MpeData {
            pressure: 1.0,
            pitch_bend: -1.0,
            timbre: 0.0,
        };
        let (mut engine, mut producer, _lp, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe,
            },
        );

        let mut output = [0.0f32; TRANSDUCER_COUNT];
        let mut near_peak = 0.0f32;
        let mut far_early_peak = 0.0f32;
        let mut far_late_peak = 0.0f32;
        for n in 0..(0.3 * SAMPLE_RATE) as usize {
            engine.process(&mut output, SAMPLE_RATE);
            near_peak = near_peak.max(output[0].abs());
            if n < 4800 {
                far_early_peak = far_early_peak.max(output[31].abs());
            } else {
                far_late_peak = far_late_peak.max(output[31].abs());
            }
        }
        assert!(near_peak > 0.0, "near transducer should sound quickly");
        assert_eq!(
            far_early_peak, 0.0,
            "far corner must stay silent until the wave arrives (~108 ms)"
        );
        assert!(
            far_late_peak > 0.0,
            "far corner should sound once the wave arrives"
        );
    }

    #[test]
    fn scatter_delay_line_shifts_pitch_with_source_motion() {
        // The scatter-write / sequential-read delay line must produce Doppler
        // directly from a changing delay: a shrinking delay (approaching
        // source) raises the observed frequency, a growing delay (receding)
        // lowers it, and neither direction produces read-backwards garbage or
        // dropout gaps at up to 0.5 samples/sample of delay change (the 0.5·c
        // source-speed bound). We drive one line with a fixed-frequency sine
        // while ramping the delay, and compare zero-crossing rates.
        let fs = 1500.0_f32; // internal render rate
        let f0 = 100.0_f32;
        let d_rate = 0.3_f32; // |dδ/dn| samples per sample, < 0.5
        let n = 1500usize; // one second of internal frames
                           // Start high enough that the delay stays comfortably positive across
                           // the full shrinking sweep (500 − 0.3·1500 = 50 samples at the end).
        let d_start = 500.0_f32;

        // Returns (zero_crossings, peak_abs, rms) of the delay-line output for
        // a delay that ramps at `slope` samples/sample from `d_start`.
        let kernel = design_splat_kernel();
        let run = |slope: f32| -> (usize, f32, f32) {
            let mut line = DelayLine::new();
            let mut prev = 0.0f32;
            let mut crossings = 0usize;
            let mut peak = 0.0f32;
            let mut sumsq = 0.0f64;
            let mut counted = 0usize;
            for i in 0..n {
                let phase = 2.0 * std::f32::consts::PI * f0 * i as f32 / fs;
                let input = phase.sin();
                let delay = d_start + slope * i as f32;
                let out = line.write_and_read(input, delay, &kernel);
                // Skip the initial fill (until the read pointer reaches the
                // first scattered arrivals ~d_start frames in).
                if i > d_start as usize + 50 {
                    if prev <= 0.0 && out > 0.0 {
                        crossings += 1;
                    }
                    peak = peak.max(out.abs());
                    sumsq += (out as f64) * (out as f64);
                    counted += 1;
                    prev = out;
                }
            }
            let rms = (sumsq / counted.max(1) as f64).sqrt() as f32;
            (crossings, peak, rms)
        };

        let (approach, approach_peak, approach_rms) = run(-d_rate); // delay shrinking
        let (stationary, _, stationary_rms) = run(0.0);
        let (recede, _recede_peak, recede_rms) = run(d_rate); // delay growing

        // Doppler direction: approaching raises pitch, receding lowers it.
        assert!(
            approach > stationary && stationary > recede,
            "expected approach {approach} > stationary {stationary} > recede {recede} zero-crossings"
        );

        // The shift tracks the classic f0·(1 − dδ/dn): at dδ/dn = ∓0.3 the
        // observed frequency is ≈ 130 Hz / 70 Hz, a clear ordered separation
        // over the counted window (the ordering above pins the direction).

        // No read-backwards garbage on approach: bunching gives bounded gain
        // (~1/(1−0.3) ≈ 1.43), not an explosion.
        assert!(
            approach_peak < 2.0,
            "approach peak {approach_peak} should stay bounded"
        );
        assert!(
            approach_rms > stationary_rms,
            "approaching source should be louder (Doppler gain)"
        );

        // No dropout gaps on recession: the signal thins but stays live.
        assert!(
            recede_rms > 0.2 * stationary_rms,
            "receding output collapsed: rms {recede_rms}"
        );
    }

    #[test]
    fn splat_kernel_has_unit_dc_and_flat_in_band_gain_across_phases() {
        // The scatter kernel's defining property (the one that removes the
        // 2-tap linear splat's granulation warble): every fractional phase has
        // unit DC gain AND near-flat in-band gain, so sweeping the fractional
        // arrival position introduces no amplitude ripple.
        let k = design_splat_kernel();
        let w_norm = 2.0 * std::f64::consts::PI * 200.0 / 1500.0; // 200 Hz at the 1.5 kHz internal rate
        let mut min_g = f64::MAX;
        let mut max_g = f64::MIN;
        for p in 0..SPLAT_PHASES {
            let row = &k[p * SPLAT_TAPS..(p + 1) * SPLAT_TAPS];
            let dc: f64 = row.iter().map(|&x| x as f64).sum();
            assert!((dc - 1.0).abs() < 1e-4, "phase {p} DC gain {dc} != 1");
            // Magnitude response at the in-band test frequency
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (t, &c) in row.iter().enumerate() {
                let d = t as f64 - (SPLAT_HALF - 1) as f64;
                re += c as f64 * (w_norm * d).cos();
                im -= c as f64 * (w_norm * d).sin();
            }
            let g = (re * re + im * im).sqrt();
            min_g = min_g.min(g);
            max_g = max_g.max(g);
        }
        // Peak-to-peak in-band gain variation across all phases < 0.1 dB
        let ripple_db = 20.0 * (max_g / min_g).log10();
        assert!(ripple_db < 0.1, "in-band gain ripple across phases is {ripple_db:.3} dB (min {min_g:.4}, max {max_g:.4})");
    }

    #[test]
    fn layout_gains_apply_and_hot_swap_takes_effect() {
        let muted = TransducerLayout {
            gains: [0.0; TRANSDUCER_COUNT],
            ..TransducerLayout::default()
        };
        let (mut engine, mut producer, mut layout_producer, _voices) = StimulusEngine::new(muted);

        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert_eq!(peak, 0.0, "zero gains must mute all output");

        // Hot-swap to unity gains: same note keeps sounding, now audible
        layout_producer.push(TransducerLayout::default()).unwrap();
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert!(peak > 0.0, "restored gains must un-mute the running voice");
    }

    #[test]
    fn monitor_routing_selects_logical_channel_for_physical_output() {
        // Same geometry as the delay test: source at origin, ch0 near
        // (~12 ms away), ch31 far (~103 ms away). Route physical L <- 31
        // and R <- 0 on a stereo "device": L must stay silent early while
        // R sounds, proving outputs follow the routing.
        let mpe = MpeData {
            pressure: 1.0,
            pitch_bend: -1.0,
            timbre: 0.0,
        };
        let (mut engine, mut producer, _lp, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 0,
                parameter: Parameter::MonitorRoute {
                    output: 0,
                    source: 31,
                },
            },
        );
        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 0,
                parameter: Parameter::MonitorRoute {
                    output: 1,
                    source: 0,
                },
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe,
            },
        );

        // 60 ms stereo block: past ch0's delay, well before ch31's
        let frames = (0.06 * SAMPLE_RATE) as usize;
        let mut data = vec![0.0f32; frames * 2];
        let mut levels = [0.0f32; TRANSDUCER_COUNT];
        engine.process_block(&mut data, 2, SAMPLE_RATE, &mut levels);

        let left_peak = data.iter().step_by(2).fold(0.0f32, |m, &s| m.max(s.abs()));
        let right_peak = data
            .iter()
            .skip(1)
            .step_by(2)
            .fold(0.0f32, |m, &s| m.max(s.abs()));
        assert_eq!(
            left_peak, 0.0,
            "L is routed to the far channel; its wave hasn't arrived yet"
        );
        assert!(
            right_peak > 0.0,
            "R is routed to the near channel and should sound"
        );
        assert!(levels[0] > 0.0, "logical levels stay pre-routing");
    }

    #[test]
    fn active_voice_snapshots_include_travelling_wave_stimuli() {
        let (mut engine, mut producer, _, mut snapshots) =
            StimulusEngine::new(TransducerLayout::default());
        send(
            &mut producer,
            EngineCommand::RegisterInstance {
                instance_id: 55,
                config: InstanceConfig {
                    stimulus_type: StimulusType::TravellingWave,
                    wave_speed: 20.0,
                    ..InstanceConfig::default()
                },
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 55,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: full_mpe(),
            },
        );
        let mut data = [0.0f32; 32 * 32];
        let mut levels = [0.0f32; TRANSDUCER_COUNT];
        engine.process_block(&mut data, 32, SAMPLE_RATE, &mut levels);

        let snapshot = snapshots.pop().unwrap();
        assert_eq!(snapshot.count, 1);
        assert_eq!(snapshot.voices[0].instance_id, 55);
        assert_eq!(snapshot.voices[0].note_type, StimulusType::TravellingWave);
    }

    #[test]
    fn reconstruction_fir_has_unity_dc_gain_and_strong_image_rejection() {
        let h = design_reconstruction_fir();
        // Every polyphase branch must pass DC at unity, or the output would
        // carry amplitude ripple at the internal rate
        for p in 0..RENDER_DECIMATION {
            let sum: f32 = (0..FIR_TAPS_PER_PHASE)
                .map(|k| h[k * RENDER_DECIMATION + p])
                .sum();
            assert!((sum - 1.0).abs() < 0.01, "phase {p} DC gain {sum}");
        }
        // Frequency response at the first image of a 200 Hz tone
        // (internal_rate - 200 Hz): must be deep in the stopband
        let (sr, f_content) = (48000.0f64, 200.0f64);
        let f_image = sr / RENDER_DECIMATION as f64 - f_content;
        let respond = |freq: f64| -> f64 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (n, &tap) in h.iter().enumerate() {
                let w = 2.0 * std::f64::consts::PI * freq * n as f64 / sr;
                re += tap as f64 * w.cos();
                im -= tap as f64 * w.sin();
            }
            (re * re + im * im).sqrt() / RENDER_DECIMATION as f64
        };
        let passband_db = 20.0 * respond(f_content).log10();
        let image_db = 20.0 * respond(f_image).log10();
        assert!(
            passband_db.abs() < 0.1,
            "200 Hz passband gain {passband_db} dB"
        );
        assert!(image_db < -90.0, "first-image rejection only {image_db} dB");
    }

    #[test]
    fn upsampled_output_is_independent_of_block_chunking() {
        let make = || {
            let (engine, mut producer, _lp, _voices) =
                StimulusEngine::new(TransducerLayout::default());
            send(
                &mut producer,
                EngineCommand::NoteOn {
                    instance_id: 0,
                    note: 60,
                    velocity: 100,
                    channel: 1,
                    mpe: full_mpe(),
                },
            );
            (engine, producer)
        };
        let frames = 4800; // 100 ms at the device rate
        let mut levels = [0.0f32; TRANSDUCER_COUNT];

        let (mut a, _keep_a) = make();
        let mut whole = vec![0.0f32; frames * 2];
        a.process_block(&mut whole, 2, SAMPLE_RATE, &mut levels);

        let (mut b, _keep_b) = make();
        let mut chunked = vec![0.0f32; frames * 2];
        let mut off = 0;
        for &size in [512usize, 100, 60, 7, 1024, 300].iter().cycle() {
            if off >= frames {
                break;
            }
            let n = size.min(frames - off);
            b.process_block(
                &mut chunked[off * 2..(off + n) * 2],
                2,
                SAMPLE_RATE,
                &mut levels,
            );
            off += n;
        }
        assert!(
            whole.iter().any(|&s| s != 0.0),
            "note should be audible in the window"
        );
        assert_eq!(
            whole, chunked,
            "upsampler state must carry across arbitrary block boundaries"
        );
    }

    #[test]
    fn reconstructed_device_output_is_hard_bounded() {
        let layout = TransducerLayout {
            gains: [100.0; TRANSDUCER_COUNT],
            ..TransducerLayout::default()
        };
        let (mut engine, mut producer, _, _) = StimulusEngine::new(layout);
        for note in 60..68 {
            send(
                &mut producer,
                EngineCommand::NoteOn {
                    instance_id: 0,
                    note,
                    velocity: 127,
                    channel: note - 60,
                    mpe: full_mpe(),
                },
            );
        }
        let mut data = vec![0.0f32; 4_800 * TRANSDUCER_COUNT];
        let mut levels = [0.0f32; TRANSDUCER_COUNT];
        engine.process_block(&mut data, TRANSDUCER_COUNT, SAMPLE_RATE, &mut levels);
        assert!(data.iter().all(|sample| sample.is_finite()));
        assert!(data.iter().all(|sample| sample.abs() <= 1.0));
    }

    /// Debug harness, not a regression test: drives the engine exactly like
    /// the audio callback against a dummy 32-channel device while replaying
    /// the viewer's orbit command stream, and captures the raw 32-channel
    /// output to disk for offline analysis. Run explicitly:
    ///
    ///   HAPTIC_CAPTURE_OUT=/path/dir cargo test -p haptic-server --release \
    ///       orbit_capture -- --ignored --nocapture
    ///
    /// Optional env: HAPTIC_CAPTURE_WAVE_SPEED (1.0), HAPTIC_CAPTURE_SECS
    /// (13), HAPTIC_CAPTURE_SR (48000), HAPTIC_CAPTURE_BLOCK (512),
    /// HAPTIC_CAPTURE_ORBIT_PERIOD (6), HAPTIC_CAPTURE_NOTE (36 / Ableton C1),
    /// HAPTIC_CAPTURE_MPE_MS (8).
    #[test]
    #[ignore]
    fn orbit_capture_writes_debug_buffers() {
        use std::io::Write as _;

        let env_f32 = |key: &str, default: f32| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let out_dir =
            std::env::var("HAPTIC_CAPTURE_OUT").unwrap_or_else(|_| "target/orbit-capture".into());
        let wave_speed = env_f32("HAPTIC_CAPTURE_WAVE_SPEED", 1.0);
        let secs = env_f32("HAPTIC_CAPTURE_SECS", 13.0);
        let sample_rate = env_f32("HAPTIC_CAPTURE_SR", 48000.0);
        let block = env_f32("HAPTIC_CAPTURE_BLOCK", 512.0) as usize;
        let orbit_period = env_f32("HAPTIC_CAPTURE_ORBIT_PERIOD", 6.0);
        let note = env_f32("HAPTIC_CAPTURE_NOTE", DEFAULT_TEST_NOTE as f32) as u8;
        let mpe_interval = env_f32("HAPTIC_CAPTURE_MPE_MS", 8.0) as f64 / 1000.0;

        let layout = TransducerLayout::default();
        let (width, length) = layout.table_m;
        let (mut engine, mut producer, _lp, _voices) = StimulusEngine::new(layout);

        // Mimic the viewer's start(): wave speed first, then note-on at the
        // resting source position (table centre), then orbit MPE updates
        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 0,
                parameter: Parameter::WaveSpeed(wave_speed),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note,
                velocity: 100,
                channel: 15,
                mpe: MpeData {
                    pressure: 1.0,
                    pitch_bend: 0.0,
                    timbre: 0.5,
                },
            },
        );

        std::fs::create_dir_all(&out_dir).unwrap();
        let path = format!("{out_dir}/orbit_c{wave_speed}_sr{sample_rate}.f32");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());

        let mut next_mpe = 0.0f64;
        let mut orbit_phase = 0.0f32;
        let radius = 0.35 * width.min(length);
        let mut data = vec![0.0f32; block * TRANSDUCER_COUNT];
        let mut levels = [0.0f32; TRANSDUCER_COUNT];
        let total_blocks = (secs as f64 * sample_rate as f64 / block as f64) as usize;
        for b in 0..total_blocks {
            let t = b as f64 * block as f64 / sample_rate as f64;
            while t >= next_mpe {
                orbit_phase += std::f32::consts::TAU * mpe_interval as f32 / orbit_period;
                let sx = 0.5 * width + radius * orbit_phase.cos();
                let sy = 0.5 * length + radius * orbit_phase.sin();
                send(
                    &mut producer,
                    EngineCommand::MpeUpdate {
                        instance_id: 0,
                        channel: 15,
                        mpe: MpeData {
                            pressure: 1.0,
                            pitch_bend: (2.0 * sx / width - 1.0).clamp(-1.0, 1.0),
                            timbre: (sy / length).clamp(0.0, 1.0),
                        },
                    },
                );
                next_mpe += mpe_interval;
            }
            engine.process_block(&mut data, TRANSDUCER_COUNT, sample_rate, &mut levels);
            let bytes: Vec<u8> = data.iter().flat_map(|s| s.to_le_bytes()).collect();
            file.write_all(&bytes).unwrap();
        }
        file.flush().unwrap();
        eprintln!(
            "captured {} blocks ({} s) of 32ch f32le to {}",
            total_blocks,
            total_blocks * block / sample_rate as usize,
            path
        );
    }

    #[test]
    fn panic_silences_everything() {
        let (mut engine, mut producer, _layout_producer, _voices) =
            StimulusEngine::new(TransducerLayout::default());
        for note in 60..64u8 {
            send(
                &mut producer,
                EngineCommand::NoteOn {
                    instance_id: 0,
                    note,
                    velocity: 100,
                    channel: note - 60,
                    mpe: full_mpe(),
                },
            );
        }
        run_samples(&mut engine, 256);
        send(
            &mut producer,
            EngineCommand::SetParameter {
                instance_id: 0,
                parameter: Parameter::StimulusType(StimulusType::TravellingWave),
            },
        );
        send(
            &mut producer,
            EngineCommand::NoteOn {
                instance_id: 0,
                note: 72,
                velocity: 100,
                channel: 8,
                mpe: full_mpe(),
            },
        );
        run_samples(&mut engine, 256);
        assert_eq!(active_travelling_wave_voices(&engine), 1);
        send(&mut producer, EngineCommand::Panic);
        let peak = run_samples(&mut engine, 256);
        assert_eq!(peak, 0.0);
        assert_eq!(active_wave_voices(&engine), 0);
        assert_eq!(active_travelling_wave_voices(&engine), 0);
    }
}
