use haptic_protocol::{HapticCommand, MpeData, Parameter, StimulusType};
use crate::config::TransducerLayout;

// Constants from requirements
pub const TRANSDUCER_COUNT: usize = 32;
const MAX_WAVE_STIMULI: usize = 8;
const MAX_STANDING_STIMULI: usize = 4;
// Sized for worst-case propagation delay: table diagonal (~2.24 m for the
// default 1m x 2m layout, plus MPE source excursion) at the minimum wave
// speed (20 m/s) is ~115 ms; 16384 samples covers ~341 ms at 48 kHz and
// ~171 ms at 96 kHz. Delays beyond capacity are clamped, not wrapped.
const MAX_DELAY_SAMPLES: usize = 16384;

// Haptic frequency band of the transducers
const MIN_HAPTIC_FREQ: f32 = 20.0;
const MAX_HAPTIC_FREQ: f32 = 200.0;

// One-pole smoothing time constant for incoming MPE dimensions
const MPE_SMOOTHING_TAU: f32 = 0.015; // 15 ms

const DEFAULT_WAVE_SPEED: f32 = 20.0; // m/s

/// Capacity of the IPC → audio thread command ring buffer. Sized for a
/// worst-case burst of MPE traffic within one audio callback.
const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Per-block snapshot of the most recently started active wave voice,
/// exported to the IPC thread for phase visualisation.
#[derive(Clone, Copy)]
pub struct VoiceSnapshot {
    pub seq: u64,
    pub note: u8,
    pub frequency: f32,
    pub wave_speed: f32,
    pub source_pos: (f32, f32),
    /// Current perceived source level: velocity amplitude x envelope x
    /// smoothed MPE pressure.
    pub amplitude: f32,
    pub sample_rate: f32,
    /// The propagation delays the delay lines are using this block.
    pub delay_samples: [f32; TRANSDUCER_COUNT],
}

/// Map a MIDI note onto the transducers' haptic band. Equal-temperament
/// ratios are preserved but transposed two octaves down so middle C (60)
/// lands at ~65.4 Hz, then clamped to the 20-200 Hz band. Kept as a free
/// function so a config value can later select between mappings.
pub fn note_to_haptic_frequency(note: u8) -> f32 {
    let f = 440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0) / 4.0;
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

    pub fn process_all(&mut self, context: &ProcessContext<'_>, output: &mut [f32; TRANSDUCER_COUNT]) {
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
#[derive(Clone, Copy)]
struct VoiceOwner {
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
    standing_pool: StimulusPool<StandingWaveStimulus, MAX_STANDING_STIMULI>,

    // Note -> slot ownership, parallel to each pool's slots
    wave_owners: [Option<VoiceOwner>; MAX_WAVE_STIMULI],
    standing_owners: [Option<VoiceOwner>; MAX_STANDING_STIMULI],
    next_seq: u64,

    // Plugin-controlled parameters (applied to notes at note-on)
    wave_speed: f32,
    stimulus_type: StimulusType,

    // Lock-free SPSC command queues, consumer ends (IPC thread holds the
    // command producer, the config watcher holds the layout producer)
    command_queue: rtrb::Consumer<EngineCommand>,
    layout_queue: rtrb::Consumer<Box<TransducerLayout>>,

    // Voice snapshots out to the IPC thread (drops when full)
    voice_producer: rtrb::Producer<VoiceSnapshot>,

    // Transducer configuration (hot-swappable via layout_queue)
    layout: TransducerLayout,
}

pub struct ProcessContext<'a> {
    pub sample_rate: f32,
    pub dt: f32,
    pub transducer_positions: &'a [(f32, f32); TRANSDUCER_COUNT],
}

// Commands from IPC thread
#[derive(Clone)]
pub enum EngineCommand {
    NoteOn { note: u8, velocity: u8, channel: u8, mpe: MpeData },
    NoteOff { note: u8, channel: u8 },
    MpeUpdate { channel: u8, mpe: MpeData },
    SetParameter { parameter: Parameter },
    Panic,
}

impl From<HapticCommand> for EngineCommand {
    fn from(cmd: HapticCommand) -> Self {
        match cmd {
            HapticCommand::NoteOn { note, velocity, channel, mpe, .. } => {
                EngineCommand::NoteOn { note, velocity, channel, mpe }
            }
            HapticCommand::NoteOff { note, channel, .. } => {
                EngineCommand::NoteOff { note, channel }
            }
            HapticCommand::MpeUpdate { channel, mpe, .. } => {
                EngineCommand::MpeUpdate { channel, mpe }
            }
            HapticCommand::SetParameter { parameter, .. } => {
                EngineCommand::SetParameter { parameter }
            }
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
        rtrb::Producer<Box<TransducerLayout>>,
        rtrb::Consumer<VoiceSnapshot>,
    ) {
        let (producer, consumer) = rtrb::RingBuffer::new(COMMAND_QUEUE_CAPACITY);
        let (layout_producer, layout_consumer) = rtrb::RingBuffer::new(4);
        let (voice_producer, voice_consumer) = rtrb::RingBuffer::new(256);

        let engine = Self {
            wave_pool: StimulusPool::new(),
            standing_pool: StimulusPool::new(),
            wave_owners: [None; MAX_WAVE_STIMULI],
            standing_owners: [None; MAX_STANDING_STIMULI],
            next_seq: 0,
            wave_speed: DEFAULT_WAVE_SPEED,
            stimulus_type: StimulusType::Wave,
            command_queue: consumer,
            layout_queue: layout_consumer,
            voice_producer,
            layout,
        };
        (engine, producer, layout_producer, voice_consumer)
    }

    fn note_on(&mut self, note: u8, velocity: u8, channel: u8, mpe: MpeData) {
        let frequency = note_to_haptic_frequency(note);
        let seq = self.next_seq;
        self.next_seq += 1;
        let owner = VoiceOwner { channel, note, seq };

        match self.stimulus_type {
            StimulusType::Wave => {
                let slot = match self.wave_pool.allocate_slot() {
                    Some(slot) => slot,
                    None => {
                        let slot = steal_candidate(&self.wave_pool, &self.wave_owners);
                        self.wave_pool.retrigger_slot(slot);
                        slot
                    }
                };
                let wave_speed = self.wave_speed;
                let stim = self.wave_pool.get_mut(slot);
                stim.note_on(frequency, velocity, mpe);
                stim.set_wave_speed(wave_speed);
                self.wave_owners[slot] = Some(owner);
            }
            StimulusType::Standing => {
                let slot = match self.standing_pool.allocate_slot() {
                    Some(slot) => slot,
                    None => {
                        let slot = steal_candidate(&self.standing_pool, &self.standing_owners);
                        self.standing_pool.retrigger_slot(slot);
                        slot
                    }
                };
                self.standing_pool.get_mut(slot).note_on(frequency, velocity, mpe);
                self.standing_owners[slot] = Some(owner);
            }
        }
    }

    fn note_off(&mut self, note: u8, channel: u8) {
        for slot in 0..MAX_WAVE_STIMULI {
            if let Some(owner) = self.wave_owners[slot] {
                if owner.channel == channel && owner.note == note {
                    self.wave_pool.get_mut(slot).note_off();
                }
            }
        }
        for slot in 0..MAX_STANDING_STIMULI {
            if let Some(owner) = self.standing_owners[slot] {
                if owner.channel == channel && owner.note == note {
                    self.standing_pool.get_mut(slot).note_off();
                }
            }
        }
        // Ownership is retained through the release phase so late MPE
        // updates still reach the voice; it is cleared once inactive.
    }

    fn mpe_update(&mut self, channel: u8, mpe: MpeData) {
        for slot in 0..MAX_WAVE_STIMULI {
            if let Some(owner) = self.wave_owners[slot] {
                if owner.channel == channel {
                    self.wave_pool.get_mut(slot).mpe_update(mpe);
                }
            }
        }
        for slot in 0..MAX_STANDING_STIMULI {
            if let Some(owner) = self.standing_owners[slot] {
                if owner.channel == channel {
                    self.standing_pool.get_mut(slot).mpe_update(mpe);
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
        for slot in 0..MAX_STANDING_STIMULI {
            if self.standing_owners[slot].is_some() && !self.standing_pool.slot_active(slot) {
                self.standing_owners[slot] = None;
            }
        }
    }

    fn apply_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::NoteOn { note, velocity, channel, mpe } => {
                self.note_on(note, velocity, channel, mpe);
            }
            EngineCommand::NoteOff { note, channel } => {
                self.note_off(note, channel);
            }
            EngineCommand::MpeUpdate { channel, mpe } => {
                self.mpe_update(channel, mpe);
            }
            EngineCommand::SetParameter { parameter } => match parameter {
                Parameter::WaveSpeed(speed) => {
                    self.wave_speed = speed.clamp(1.0, 1000.0);
                }
                Parameter::StimulusType(kind) => {
                    self.stimulus_type = kind;
                }
            },
            EngineCommand::Panic => {
                self.wave_pool.reset_all();
                self.standing_pool.reset_all();
                self.wave_owners = [None; MAX_WAVE_STIMULI];
                self.standing_owners = [None; MAX_STANDING_STIMULI];
            }
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.command_queue.pop() {
            self.apply_command(cmd);
        }
        // Hot config reload: copy the new layout in place. Dropping the Box
        // deallocates on the audio thread, but only on the rare occasion a
        // human edits the config file - accepted.
        while let Ok(layout) = self.layout_queue.pop() {
            self.layout = *layout;
        }
    }

    /// Synthesize one frame of all active stimuli into `output`.
    fn render_frame(&mut self, context: &ProcessContext<'_>, output: &mut [f32; TRANSDUCER_COUNT]) {
        output.fill(0.0);
        self.wave_pool.process_all(context, output);
        self.standing_pool.process_all(context, output);

        // Per-transducer gain, then safety limiting
        for (sample, &gain) in output.iter_mut().zip(self.layout.gains.iter()) {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
    }

    /// Audio-callback entry point: drains pending commands once, then fills
    /// the interleaved `data` buffer. Writes the block RMS of each of the 32
    /// logical transducer outputs into `levels_out` (computed pre-truncation,
    /// so levels are meaningful even on a stereo fallback device).
    /// MUST NOT block or allocate.
    pub fn process_block(
        &mut self,
        data: &mut [f32],
        channels: usize,
        sample_rate: f32,
        levels_out: &mut [f32; TRANSDUCER_COUNT],
    ) {
        self.drain_commands();

        // Copy so the context doesn't hold a borrow of self across render_frame
        let positions = self.layout.positions;
        let context = ProcessContext {
            sample_rate,
            dt: 1.0 / sample_rate,
            transducer_positions: &positions,
        };

        let mut output = [0.0f32; TRANSDUCER_COUNT];
        let mut sum_squares = [0.0f32; TRANSDUCER_COUNT];
        let frames = data.len() / channels;
        for frame in data[..frames * channels].chunks_exact_mut(channels) {
            self.render_frame(&context, &mut output);
            for (sum, &sample) in sum_squares.iter_mut().zip(output.iter()) {
                *sum += sample * sample;
            }
            let n = channels.min(TRANSDUCER_COUNT);
            frame[..n].copy_from_slice(&output[..n]);
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

        self.publish_latest_voice(sample_rate);
        self.reap_finished_voices();
    }

    /// Push a snapshot of the most recently started active wave voice for
    /// the visualiser. No-op (and no allocation) when nothing is playing.
    fn publish_latest_voice(&mut self, sample_rate: f32) {
        let mut latest: Option<(usize, VoiceOwner)> = None;
        for (slot, owner) in self.wave_owners.iter().enumerate() {
            if let Some(o) = owner {
                if self.wave_pool.slot_active(slot)
                    && latest.map_or(true, |(_, best)| o.seq > best.seq)
                {
                    latest = Some((slot, *o));
                }
            }
        }
        let Some((slot, owner)) = latest else { return };
        let stim = &self.wave_pool.stimuli[slot];

        let mut delay_samples = [0.0f32; TRANSDUCER_COUNT];
        for (delay, &(tx, ty)) in delay_samples.iter_mut().zip(self.layout.positions.iter()) {
            let dx = tx - stim.source_pos.0;
            let dy = ty - stim.source_pos.1;
            let distance = (dx * dx + dy * dy).sqrt();
            *delay = distance / stim.wave_speed.max(1.0) * sample_rate;
        }

        let _ = self.voice_producer.push(VoiceSnapshot {
            seq: owner.seq,
            note: owner.note,
            frequency: stim.frequency,
            wave_speed: stim.wave_speed,
            source_pos: stim.source_pos,
            amplitude: stim.amplitude * stim.env_level * stim.mpe.pressure,
            sample_rate,
            delay_samples,
        });
    }

    /// Single-frame variant used by tests.
    #[cfg(test)]
    pub fn process(&mut self, output: &mut [f32; TRANSDUCER_COUNT], sample_rate: f32) {
        self.drain_commands();
        let positions = self.layout.positions;
        let context = ProcessContext {
            sample_rate,
            dt: 1.0 / sample_rate,
            transducer_positions: &positions,
        };
        self.render_frame(&context, output);
        self.reap_finished_voices();
    }

}

/// One-pole toward `target`; coeff derived from dt and MPE_SMOOTHING_TAU.
fn smooth_mpe(current: &mut MpeData, target: &MpeData, dt: f32) {
    let coeff = dt / (MPE_SMOOTHING_TAU + dt);
    current.pressure += (target.pressure - current.pressure) * coeff;
    current.pitch_bend += (target.pitch_bend - current.pitch_bend) * coeff;
    current.timbre += (target.timbre - current.timbre) * coeff;
}

// Delay line for wave propagation
#[derive(Clone)]
struct DelayLine {
    buffer: Box<[f32; MAX_DELAY_SAMPLES]>, // Move large buffer to heap
    write_pos: f32,
    size: usize,
}

impl DelayLine {
    fn new() -> Self {
        Self {
            buffer: Box::new([0.0; MAX_DELAY_SAMPLES]), // Allocate on heap
            write_pos: 0.0,
            size: MAX_DELAY_SAMPLES,
        }
    }

    // Fractional delay with linear interpolation
    fn write_and_read(&mut self, input: f32, delay_samples: f32) -> f32 {
        // Write current input
        let write_idx = self.write_pos as usize;
        self.buffer[write_idx] = input;

        // A delay beyond capacity would make read_pos negative even after
        // the wrap correction; the negative-to-usize cast then reads index 0
        // (i.e. the sample just written - no delay at all). Clamp instead.
        let delay_samples = delay_samples.clamp(0.0, (self.size - 2) as f32);

        // Read with fractional delay
        let read_pos = self.write_pos - delay_samples;
        let read_pos = if read_pos < 0.0 {
            read_pos + self.size as f32
        } else {
            read_pos
        };

        let idx0 = read_pos.floor() as usize % self.size;
        let idx1 = (idx0 + 1) % self.size;
        let frac = read_pos.fract();

        let output = self.buffer[idx0] * (1.0 - frac) + self.buffer[idx1] * frac;

        // Advance write position
        self.write_pos = (self.write_pos + 1.0) % self.size as f32;

        output
    }

    fn reset(&mut self) {
        self.buffer.as_mut().fill(0.0);
        self.write_pos = 0.0;
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
    source_pos: (f32, f32),
    wave_speed: f32, // Individual wave speed for this stimulus

    // Envelope
    env_state: EnvelopeState,
    env_level: f32,
    env_time: f32,

    // MPE: `mpe` is the smoothed value used for synthesis, `mpe_target`
    // is the most recent raw update from the controller
    mpe: MpeData,
    mpe_target: MpeData,
}

#[derive(Default, PartialEq)]
enum EnvelopeState {
    #[default]
    Idle,
    Attack,
    Sustain,
    Release,
}

impl Stimulus for WaveStimulus {
    fn process(&mut self, ctx: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT] {
        let mut output = [0.0; TRANSDUCER_COUNT];

        // Update envelope
        match self.env_state {
            EnvelopeState::Idle => return output,
            EnvelopeState::Attack => {
                self.env_time += ctx.dt;
                self.env_level = (self.env_time * 10.0).min(1.0); // 100ms attack
                if self.env_level >= 1.0 {
                    self.env_state = EnvelopeState::Sustain;
                }
            }
            EnvelopeState::Sustain => {
                self.env_level = 1.0;
            }
            EnvelopeState::Release => {
                self.env_time += ctx.dt;
                self.env_level = (1.0 - self.env_time * 2.0).max(0.0); // 500ms release
                if self.env_level <= 0.0 {
                    self.env_state = EnvelopeState::Idle;
                }
            }
        }

        // Smooth MPE toward the latest controller values
        smooth_mpe(&mut self.mpe, &self.mpe_target, ctx.dt);

        // Update source position from MPE
        self.source_pos.0 = self.mpe.pitch_bend * 0.2; // ±20cm
        self.source_pos.1 = self.mpe.timbre * 0.2;

        // Generate source signal
        let source = (self.phase * 2.0 * std::f32::consts::PI).sin()
                    * self.amplitude * self.env_level * self.mpe.pressure;

        // Process through delay lines
        for (i, &transducer_pos) in ctx.transducer_positions.iter().enumerate() {
            let dx = transducer_pos.0 - self.source_pos.0;
            let dy = transducer_pos.1 - self.source_pos.1;
            let distance = (dx * dx + dy * dy).sqrt();

            let delay_time = distance / self.wave_speed.max(1.0); // Use per-stimulus wave speed, min 1.0 to avoid div by zero
            let delay_samples = delay_time * ctx.sample_rate;

            let delayed = self.delay_lines[i].write_and_read(source, delay_samples);
            let attenuated = delayed / (1.0 + distance * 2.0); // Distance attenuation

            output[i] = attenuated;
        }

        // Update phase
        self.phase += self.frequency * ctx.dt;
        if self.phase >= 1.0 { self.phase -= 1.0; }

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
        self.mpe = mpe;
        self.mpe_target = mpe;
        self.env_state = EnvelopeState::Attack;
        self.env_time = 0.0;
        self.wave_speed = DEFAULT_WAVE_SPEED; // Overridden by set_wave_speed after note_on
    }

    fn note_off(&mut self) {
        if self.env_state != EnvelopeState::Idle {
            self.env_state = EnvelopeState::Release;
            self.env_time = 0.0;
        }
    }

    fn mpe_update(&mut self, mpe: MpeData) {
        self.mpe_target = mpe;
    }

    fn reset(&mut self) {
        for line in &mut self.delay_lines {
            line.reset();
        }
        self.phase = 0.0;
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
        self.wave_speed = DEFAULT_WAVE_SPEED;
    }

    fn set_wave_speed(&mut self, wave_speed: f32) {
        self.wave_speed = wave_speed;
    }
}

// StandingWaveStimulus - simpler, no propagation delay
#[derive(Default)]
pub struct StandingWaveStimulus {
    frequency: f32,
    phase: f32,
    amplitude: f32,
    env_state: EnvelopeState,
    env_level: f32,
    env_time: f32,
    mpe: MpeData,
    mpe_target: MpeData,
}

impl Stimulus for StandingWaveStimulus {
    fn process(&mut self, ctx: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT] {
        let mut output = [0.0; TRANSDUCER_COUNT];

        // Update envelope (same as WaveStimulus)
        match self.env_state {
            EnvelopeState::Idle => return output,
            EnvelopeState::Attack => {
                self.env_time += ctx.dt;
                self.env_level = (self.env_time * 10.0).min(1.0);
                if self.env_level >= 1.0 {
                    self.env_state = EnvelopeState::Sustain;
                }
            }
            EnvelopeState::Sustain => {
                self.env_level = 1.0;
            }
            EnvelopeState::Release => {
                self.env_time += ctx.dt;
                self.env_level = (1.0 - self.env_time * 2.0).max(0.0);
                if self.env_level <= 0.0 {
                    self.env_state = EnvelopeState::Idle;
                }
            }
        }

        smooth_mpe(&mut self.mpe, &self.mpe_target, ctx.dt);

        let source = (self.phase * 2.0 * std::f32::consts::PI).sin()
                    * self.amplitude * self.env_level * self.mpe.pressure;

        // Simple spatial distribution without delay
        for i in 0..TRANSDUCER_COUNT {
            output[i] = source; // All transducers in phase
        }

        self.phase += self.frequency * ctx.dt;
        if self.phase >= 1.0 { self.phase -= 1.0; }

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
        self.mpe = mpe;
        self.mpe_target = mpe;
        self.env_state = EnvelopeState::Attack;
        self.env_time = 0.0;
    }

    fn note_off(&mut self) {
        if self.env_state != EnvelopeState::Idle {
            self.env_state = EnvelopeState::Release;
            self.env_time = 0.0;
        }
    }

    fn mpe_update(&mut self, mpe: MpeData) {
        self.mpe_target = mpe;
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
        self.mpe = MpeData::default();
        self.mpe_target = MpeData::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f32 = 48000.0;

    fn full_mpe() -> MpeData {
        MpeData { pressure: 1.0, pitch_bend: 0.0, timbre: 0.5 }
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

    #[test]
    fn frequency_mapping_targets_haptic_band() {
        // Middle C lands near 65 Hz
        assert!((note_to_haptic_frequency(60) - 65.4).abs() < 0.5);
        // Octave relationship preserved inside the band
        let c3 = note_to_haptic_frequency(48);
        let c4 = note_to_haptic_frequency(60);
        assert!((c4 / c3 - 2.0).abs() < 1e-3);
        // Extremes clamp to the transducer band
        assert_eq!(note_to_haptic_frequency(0), 20.0);
        assert_eq!(note_to_haptic_frequency(127), 200.0);
    }

    #[test]
    fn note_on_produces_output_and_note_off_releases() {
        let (mut engine, mut producer, _layout_producer, _voices) = StimulusEngine::new(TransducerLayout::default());
        send(&mut producer, EngineCommand::NoteOn { note: 60, velocity: 100, channel: 1, mpe: full_mpe() });

        // 200ms: through the attack, into sustain
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert!(peak > 0.0, "expected audible output after note on");
        assert_eq!(active_wave_voices(&engine), 1);

        send(&mut producer, EngineCommand::NoteOff { note: 60, channel: 1 });
        // 700ms: past the 500ms release
        run_samples(&mut engine, (0.7 * SAMPLE_RATE) as usize);
        assert_eq!(active_wave_voices(&engine), 0, "voice should be reaped after release");

        let residual = run_samples(&mut engine, 1024);
        assert_eq!(residual, 0.0, "released voice should be silent");
    }

    #[test]
    fn note_off_only_affects_matching_note_and_channel() {
        let (mut engine, mut producer, _layout_producer, _voices) = StimulusEngine::new(TransducerLayout::default());
        send(&mut producer, EngineCommand::NoteOn { note: 60, velocity: 100, channel: 1, mpe: full_mpe() });
        send(&mut producer, EngineCommand::NoteOn { note: 64, velocity: 100, channel: 2, mpe: full_mpe() });
        run_samples(&mut engine, 256);
        assert_eq!(active_wave_voices(&engine), 2);

        // Wrong channel: no effect
        send(&mut producer, EngineCommand::NoteOff { note: 60, channel: 2 });
        run_samples(&mut engine, (0.7 * SAMPLE_RATE) as usize);
        assert_eq!(active_wave_voices(&engine), 2);

        send(&mut producer, EngineCommand::NoteOff { note: 60, channel: 1 });
        run_samples(&mut engine, (0.7 * SAMPLE_RATE) as usize);
        assert_eq!(active_wave_voices(&engine), 1);
    }

    #[test]
    fn voice_stealing_prefers_releasing_then_oldest() {
        let (mut engine, mut producer, _layout_producer, _voices) = StimulusEngine::new(TransducerLayout::default());
        for note in 0..MAX_WAVE_STIMULI as u8 {
            send(&mut producer, EngineCommand::NoteOn { note: 40 + note, velocity: 100, channel: note, mpe: full_mpe() });
        }
        run_samples(&mut engine, 64);
        assert_eq!(active_wave_voices(&engine), MAX_WAVE_STIMULI);

        // Release note 42 (channel 2), then allocate over capacity
        send(&mut producer, EngineCommand::NoteOff { note: 42, channel: 2 });
        run_samples(&mut engine, 64); // still releasing, still owned
        send(&mut producer, EngineCommand::NoteOn { note: 90, velocity: 100, channel: 10, mpe: full_mpe() });
        run_samples(&mut engine, 64);

        assert_eq!(active_wave_voices(&engine), MAX_WAVE_STIMULI);
        let notes: Vec<u8> = engine.wave_owners.iter().flatten().map(|o| o.note).collect();
        assert!(notes.contains(&90), "new note should have a voice");
        assert!(!notes.contains(&42), "releasing voice should have been stolen");

        // Pool still full, nothing releasing: the oldest (note 40) is stolen
        send(&mut producer, EngineCommand::NoteOn { note: 91, velocity: 100, channel: 11, mpe: full_mpe() });
        run_samples(&mut engine, 64);
        let notes: Vec<u8> = engine.wave_owners.iter().flatten().map(|o| o.note).collect();
        assert!(notes.contains(&91));
        assert!(!notes.contains(&40), "oldest voice should have been stolen");
    }

    #[test]
    fn mpe_update_reaches_owning_stimulus_smoothly() {
        let (mut engine, mut producer, _layout_producer, _voices) = StimulusEngine::new(TransducerLayout::default());
        let mut quiet = full_mpe();
        quiet.pressure = 0.0;
        send(&mut producer, EngineCommand::NoteOn { note: 60, velocity: 100, channel: 3, mpe: quiet });

        // Past the attack with zero pressure: silent
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert_eq!(peak, 0.0, "zero pressure should be silent");

        // Press: output should fade in via smoothing. The window must cover
        // wave propagation to the nearest transducer (~0.13 m at 100 m/s
        // with the default cell-centred layout ≈ 1.3 ms).
        send(&mut producer, EngineCommand::MpeUpdate { channel: 3, mpe: full_mpe() });
        let early = run_samples(&mut engine, (0.01 * SAMPLE_RATE) as usize); // 10 ms
        let later = run_samples(&mut engine, (0.1 * SAMPLE_RATE) as usize);
        assert!(early > 0.0, "pressure should start taking effect");
        assert!(later > early, "smoothed pressure should keep rising");

        // Update on a different channel must not affect this voice
        let mut half = full_mpe();
        half.pressure = 0.5;
        send(&mut producer, EngineCommand::MpeUpdate { channel: 5, mpe: half });
        run_samples(&mut engine, (0.1 * SAMPLE_RATE) as usize);
        let target = engine.wave_pool.stimuli[0].mpe_target;
        assert_eq!(target.pressure, 1.0, "other channel's update must not leak in");
    }

    #[test]
    fn set_parameter_switches_pool_and_wave_speed() {
        let (mut engine, mut producer, _layout_producer, _voices) = StimulusEngine::new(TransducerLayout::default());
        send(&mut producer, EngineCommand::SetParameter { parameter: Parameter::StimulusType(StimulusType::Standing) });
        send(&mut producer, EngineCommand::SetParameter { parameter: Parameter::WaveSpeed(250.0) });
        send(&mut producer, EngineCommand::NoteOn { note: 60, velocity: 100, channel: 1, mpe: full_mpe() });
        run_samples(&mut engine, 64);

        assert_eq!(active_wave_voices(&engine), 0);
        assert_eq!(engine.standing_owners.iter().flatten().count(), 1);
        assert_eq!(engine.wave_speed, 250.0);

        send(&mut producer, EngineCommand::SetParameter { parameter: Parameter::StimulusType(StimulusType::Wave) });
        send(&mut producer, EngineCommand::NoteOn { note: 62, velocity: 100, channel: 2, mpe: full_mpe() });
        run_samples(&mut engine, 64);
        assert_eq!(active_wave_voices(&engine), 1);
        assert_eq!(engine.wave_pool.stimuli[0].wave_speed, 250.0, "wave speed parameter should apply at note on");
    }

    #[test]
    fn long_propagation_delays_are_delayed_not_wrapped() {
        // Source pushed to (-0.2, 0.0) by full negative bend, zero timbre;
        // the far corner transducer (ch 31 at (0.875, 1.875)) is ~2.16 m
        // away -> ~108 ms at the default 20 m/s = ~5188 samples. The old
        // 4800-sample delay line overflowed here and read the just-written
        // sample instead (zero delay).
        let mpe = MpeData { pressure: 1.0, pitch_bend: -1.0, timbre: 0.0 };
        let (mut engine, mut producer, _lp, _voices) = StimulusEngine::new(TransducerLayout::default());
        send(&mut producer, EngineCommand::NoteOn { note: 60, velocity: 100, channel: 1, mpe });

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
        assert_eq!(far_early_peak, 0.0, "far corner must stay silent until the wave arrives (~108 ms)");
        assert!(far_late_peak > 0.0, "far corner should sound once the wave arrives");
    }

    #[test]
    fn layout_gains_apply_and_hot_swap_takes_effect() {
        let mut muted = TransducerLayout::default();
        muted.gains = [0.0; TRANSDUCER_COUNT];
        let (mut engine, mut producer, mut layout_producer, _voices) = StimulusEngine::new(muted);

        send(&mut producer, EngineCommand::NoteOn { note: 60, velocity: 100, channel: 1, mpe: full_mpe() });
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert_eq!(peak, 0.0, "zero gains must mute all output");

        // Hot-swap to unity gains: same note keeps sounding, now audible
        layout_producer.push(Box::new(TransducerLayout::default())).unwrap();
        let peak = run_samples(&mut engine, (0.2 * SAMPLE_RATE) as usize);
        assert!(peak > 0.0, "restored gains must un-mute the running voice");
    }

    #[test]
    fn panic_silences_everything() {
        let (mut engine, mut producer, _layout_producer, _voices) = StimulusEngine::new(TransducerLayout::default());
        for note in 60..64u8 {
            send(&mut producer, EngineCommand::NoteOn { note, velocity: 100, channel: note - 60, mpe: full_mpe() });
        }
        run_samples(&mut engine, 256);
        send(&mut producer, EngineCommand::Panic);
        let peak = run_samples(&mut engine, 256);
        assert_eq!(peak, 0.0);
        assert_eq!(active_wave_voices(&engine), 0);
    }
}
