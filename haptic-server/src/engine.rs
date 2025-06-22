// Removed unused imports: Arc and RwLock (wave speed now per-stimulus)
use haptic_protocol::{HapticCommand, MpeData};

// Constants from requirements
const TRANSDUCER_COUNT: usize = 32;
const MAX_WAVE_STIMULI: usize = 8;
const MAX_STANDING_STIMULI: usize = 4;
const MAX_DELAY_SAMPLES: usize = 4800; // ~100ms at 48kHz

// Core trait - must be Send + Sync for thread safety
pub trait Stimulus: Send + Sync {
    fn process(&mut self, context: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT];
    fn is_active(&self) -> bool;
    fn note_on(&mut self, note: u8, velocity: u8, mpe: MpeData);
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
    
    pub fn allocate(&mut self) -> Option<&mut T> {
        for (i, active) in self.active_mask.iter_mut().enumerate() {
            if !*active {
                *active = true;
                self.stimuli[i].reset();
                return Some(&mut self.stimuli[i]);
            }
        }
        None
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

// Main engine with thread-safe command queue
pub struct StimulusEngine {
    wave_pool: StimulusPool<WaveStimulus, MAX_WAVE_STIMULI>,
    standing_pool: StimulusPool<StandingWaveStimulus, MAX_STANDING_STIMULI>,
    
    // Lock-free command queue for IPC thread → audio thread
    command_queue: crossbeam_channel::Receiver<EngineCommand>,
    command_producer: crossbeam_channel::Sender<EngineCommand>,
    
    // Note: Wave speed is now calculated per-stimulus from note velocity
    
    // Transducer configuration
    transducer_positions: [(f32, f32); TRANSDUCER_COUNT],
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
    Panic,
}

impl StimulusEngine {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        
        Self {
            wave_pool: StimulusPool::new(),
            standing_pool: StimulusPool::new(),
            command_queue: receiver,
            command_producer: sender,
            transducer_positions: Self::default_grid_layout(),
        }
    }
    
    pub fn get_command_producer(&self) -> crossbeam_channel::Sender<EngineCommand> {
        self.command_producer.clone()
    }
    
    pub fn handle_command(&self, cmd: HapticCommand) {
        let engine_cmd = match cmd {
            HapticCommand::NoteOn { note, velocity, channel, mpe, .. } => {
                EngineCommand::NoteOn { note, velocity, channel, mpe }
            }
            HapticCommand::NoteOff { note, channel, .. } => {
                EngineCommand::NoteOff { note, channel }
            }
            HapticCommand::MpeUpdate { channel, mpe, .. } => {
                EngineCommand::MpeUpdate { channel, mpe }
            }
            HapticCommand::SetWaveSpeed(_speed) => {
                // Wave speed is now calculated per-stimulus from velocity
                return;
            }
            HapticCommand::Panic => EngineCommand::Panic,
        };
        
        let _ = self.command_producer.send(engine_cmd);
    }
    
    // Called from audio thread - MUST NOT BLOCK
    pub fn process(&mut self, output: &mut [f32; TRANSDUCER_COUNT], sample_rate: f32) {
        // Process commands from IPC thread
        while let Ok(cmd) = self.command_queue.try_recv() {
            match cmd {
                EngineCommand::NoteOn { note, velocity, channel: _, mpe } => {
                    // Calculate wave speed from velocity: 20-500 m/s based on velocity 0-127
                    let wave_speed = 20.0 + (velocity as f32 / 127.0) * 480.0;
                    
                    // Route based on velocity: low velocity = wave stimuli, high velocity = standing wave
                    if velocity < 64 {
                        if let Some(stim) = self.wave_pool.allocate() {
                            stim.note_on(note, velocity, mpe);
                            stim.set_wave_speed(wave_speed);
                        }
                    } else {
                        if let Some(stim) = self.standing_pool.allocate() {
                            stim.note_on(note, velocity, mpe);
                            // Standing wave stimuli don't use propagation delay
                        }
                    }
                }
                EngineCommand::NoteOff { note: _, channel: _ } => {
                    // TODO: Track note→stimulus mapping for proper note off
                }
                EngineCommand::Panic => {
                    // Reset all pools
                    self.wave_pool = StimulusPool::new();
                    self.standing_pool = StimulusPool::new();
                }
                _ => {}
            }
        }
        
        // Clear output
        output.fill(0.0);
        
        // Process all active stimuli
        let context = ProcessContext {
            sample_rate,
            dt: 1.0 / sample_rate,
            transducer_positions: &self.transducer_positions,
        };
        
        self.wave_pool.process_all(&context, output);
        self.standing_pool.process_all(&context, output);
        
        // Apply safety limiting
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
    
    fn default_grid_layout() -> [(f32, f32); TRANSDUCER_COUNT] {
        let mut positions = [(0.0, 0.0); TRANSDUCER_COUNT];
        for i in 0..32 {
            let row = i / 8;
            let col = i % 8;
            positions[i] = (col as f32 * 0.05, row as f32 * 0.05); // 5cm spacing
        }
        positions
    }
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
    
    // MPE
    mpe: MpeData,
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
    
    fn note_on(&mut self, note: u8, velocity: u8, mpe: MpeData) {
        self.frequency = 440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0);
        self.amplitude = velocity as f32 / 127.0;
        self.mpe = mpe;
        self.env_state = EnvelopeState::Attack;
        self.env_time = 0.0;
        self.wave_speed = 100.0; // Default wave speed, will be overridden by set_wave_speed
    }
    
    fn note_off(&mut self) {
        if self.env_state != EnvelopeState::Idle {
            self.env_state = EnvelopeState::Release;
            self.env_time = 0.0;
        }
    }
    
    // JMZTODO: explicit smoothing should be applied to server decoded MPE updates
    fn mpe_update(&mut self, mpe: MpeData) {
        self.mpe = mpe;
    }
    
    fn reset(&mut self) {
        for line in &mut self.delay_lines {
            line.reset();
        }
        self.phase = 0.0;
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
        self.wave_speed = 100.0; // Reset to default wave speed
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
    
    fn note_on(&mut self, note: u8, velocity: u8, mpe: MpeData) {
        self.frequency = 440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0);
        self.amplitude = velocity as f32 / 127.0;
        self.mpe = mpe;
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
        self.mpe = mpe;
    }
    
    fn reset(&mut self) {
        self.phase = 0.0;
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
    }
}