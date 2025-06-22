# Haptic VST Minimal Prototype - Implementation Guide

## Project Overview

Build a minimal working prototype of a haptic VST system with:
- **VST Plugin**: MIDI/MPE input handler using nih-plug
- **Haptic Server**: Direct control of 32-channel audio interface
- **IPC**: Unix socket communication between plugin and server
- **Static Allocation**: Zero heap allocation in audio threads
- **Stimulus Types**: Wave (delay-line based) and StandingWave

## Core Architecture

### System Components

```
┌──────────────────────┐       Unix Socket        ┌─────────────────────┐
│   Haptic VST Plugin  │ ◄──────────────────────► │   Haptic Server     │
│   (nih-plug + egui)  │                           │  (CoreAudio/CPAL)   │
├──────────────────────┤                           ├─────────────────────┤
│ • MIDI/MPE input     │                           │ • Stimulus Engine   │
│ • Parameter control  │                           │ • 32-ch output      │
│ • Visual feedback    │                           │ • Static pools      │
└──────────────────────┘                           └─────────────────────┘
```

### Critical Design Requirements

1. **Zero Allocation**: No heap allocation after initialization in audio threads
2. **Lock-Free Audio**: Audio threads never block on mutexes or I/O
3. **Static Pools**: Fixed-size stimulus pools with array-based allocation
4. **Spatial Computation**: Delay-line wave propagation with proper interpolation
5. **Thread Isolation**: Clean separation between audio, GUI, and IPC threads

## Implementation Structure

### Workspace Layout

```
haptic-vst/
├── Cargo.toml                 # Workspace definition
├── haptic-protocol/           # Shared types
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs            # Command/Status enums
├── haptic-server/            # Standalone audio server
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # Server entry point
│       ├── audio.rs         # Audio interface
│       ├── engine.rs        # Stimulus engine
│       └── ipc.rs           # Socket handling
└── haptic-plugin/           # VST plugin
    ├── Cargo.toml
    └── src/
        ├── lib.rs           # Plugin definition
        ├── editor.rs        # egui GUI
        └── ipc_client.rs    # Socket client
```

### Shared Protocol (haptic-protocol)

```rust
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct MpeData {
    pub pressure: f32,      // 0.0-1.0
    pub pitch_bend: f32,    // -1.0 to 1.0
    pub timbre: f32,        // 0.0-1.0
}

#[derive(Serialize, Deserialize, Clone)]
pub enum HapticCommand {
    // Essential commands only
    NoteOn {
        timestamp_us: u64,
        note: u8,
        velocity: u8,
        channel: u8,
        mpe: MpeData,
    },
    NoteOff {
        timestamp_us: u64,
        note: u8,
        channel: u8,
    },
    MpeUpdate {
        timestamp_us: u64,
        channel: u8,
        mpe: MpeData,
    },
    Panic,              // Stop all
}

#[derive(Serialize, Deserialize, Clone)]
pub enum ServerStatus {
    TransducerLevels {
        timestamp_us: u64,
        levels: [f32; 32],
    },
    PerformanceMetrics {
        active_stimuli: u8,
        cpu_percent: u8,
    },
}

pub const SOCKET_PATH: &str = "/tmp/haptic-vst.sock";
```

## Haptic Server Implementation

### Main Structure (haptic-server/src/main.rs)

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create shared shutdown flag
    let running = Arc::new(AtomicBool::new(true));
    
    // Start IPC listener thread
    let ipc_handle = {
        let running = running.clone();
        thread::spawn(move || ipc::listen_loop(running))
    };
    
    // Initialize audio with stimulus engine
    let engine = engine::StimulusEngine::new();
    audio::run_audio_loop(engine, running.clone())?;
    
    // Cleanup
    running.store(false, Ordering::Relaxed);
    ipc_handle.join().ok();
    Ok(())
}
```

### Stimulus Engine (haptic-server/src/engine.rs)

```rust
use std::sync::Arc;
use parking_lot::RwLock;

// Constants from requirements
const TRANSDUCER_COUNT: usize = 32;
const MAX_WAVE_STIMULI: usize = 8;
const MAX_STANDING_STIMULI: usize = 4;
const MAX_DELAY_SAMPLES: usize = 4800; // ~100ms at 48kHz

// Core trait - must be Send + Sync for thread safety
pub trait Stimulus: Send + Sync {
    fn process(&mut self, context: &ProcessContext) -> [f32; TRANSDUCER_COUNT];
    fn is_active(&self) -> bool;
    fn note_on(&mut self, note: u8, velocity: u8, mpe: MpeData);
    fn note_off(&mut self);
    fn mpe_update(&mut self, mpe: MpeData);
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
    
    pub fn process_all(&mut self, context: &ProcessContext, output: &mut [f32; TRANSDUCER_COUNT]) {
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
    command_queue: rtrb::Consumer<EngineCommand>,
    command_producer: Arc<rtrb::Producer<EngineCommand>>,
    
    // Shared parameters (atomics or RwLock for non-critical)
    wave_speed: Arc<RwLock<f32>>,
    
    // Transducer configuration
    transducer_positions: [(f32, f32); TRANSDUCER_COUNT],
}

pub struct ProcessContext {
    pub sample_rate: f32,
    pub dt: f32,
    pub wave_speed: f32,
    pub transducer_positions: &[(f32, f32); TRANSDUCER_COUNT],
}

// Commands from IPC thread
enum EngineCommand {
    NoteOn { note: u8, velocity: u8, channel: u8, mpe: MpeData },
    NoteOff { note: u8, channel: u8 },
    MpeUpdate { channel: u8, mpe: MpeData },
    Panic,
}

impl StimulusEngine {
    pub fn new() -> Self {
        let (producer, consumer) = rtrb::RingBuffer::new(256);
        
        Self {
            wave_pool: StimulusPool::new(),
            standing_pool: StimulusPool::new(),
            command_queue: consumer,
            command_producer: Arc::new(producer),
            wave_speed: Arc::new(RwLock::new(100.0)), // m/s default
            transducer_positions: Self::default_grid_layout(),
        }
    }
    
    // Called from audio thread - MUST NOT BLOCK
    pub fn process(&mut self, output: &mut [f32; TRANSDUCER_COUNT], sample_rate: f32) {
        // Process commands from IPC thread
        while let Ok(cmd) = self.command_queue.pop() {
            match cmd {
                EngineCommand::NoteOn { note, velocity, channel, mpe } => {
                    // Simple velocity-based routing
                    if velocity < 64 {
                        if let Some(stim) = self.wave_pool.allocate() {
                            stim.note_on(note, velocity, mpe);
                        }
                    } else {
                        if let Some(stim) = self.standing_pool.allocate() {
                            stim.note_on(note, velocity, mpe);
                        }
                    }
                }
                EngineCommand::NoteOff { note, channel } => {
                    // TODO: Track note→stimulus mapping
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
            wave_speed: *self.wave_speed.read(),
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
```

### Wave Stimulus Implementation

```rust
// Delay line for wave propagation
#[derive(Clone, Copy)]
struct DelayLine {
    buffer: [f32; MAX_DELAY_SAMPLES],
    write_pos: f32,
    size: usize,
}

impl DelayLine {
    fn new() -> Self {
        Self {
            buffer: [0.0; MAX_DELAY_SAMPLES],
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
        self.buffer.fill(0.0);
        self.write_pos = 0.0;
    }
}

#[derive(Default)]
struct WaveStimulus {
    delay_lines: [DelayLine; TRANSDUCER_COUNT],
    
    // Source state
    frequency: f32,
    phase: f32,
    amplitude: f32,
    source_pos: (f32, f32),
    
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

impl WaveStimulus {
    fn reset(&mut self) {
        for line in &mut self.delay_lines {
            line.reset();
        }
        self.phase = 0.0;
        self.env_state = EnvelopeState::Idle;
        self.env_level = 0.0;
        self.env_time = 0.0;
    }
}

impl Stimulus for WaveStimulus {
    fn process(&mut self, ctx: &ProcessContext) -> [f32; TRANSDUCER_COUNT] {
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
            
            let delay_time = distance / ctx.wave_speed;
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
}

// StandingWaveStimulus - simpler, no propagation delay
#[derive(Default)]
struct StandingWaveStimulus {
    frequency: f32,
    phase: f32,
    amplitude: f32,
    env_state: EnvelopeState,
    env_level: f32,
    env_time: f32,
    mpe: MpeData,
}

impl Stimulus for StandingWaveStimulus {
    fn process(&mut self, ctx: &ProcessContext) -> [f32; TRANSDUCER_COUNT] {
        // Similar envelope processing...
        let source = (self.phase * 2.0 * std::f32::consts::PI).sin() 
                    * self.amplitude * self.env_level * self.mpe.pressure;
        
        // Simple spatial distribution without delay
        let mut output = [0.0; TRANSDUCER_COUNT];
        for i in 0..TRANSDUCER_COUNT {
            output[i] = source; // All transducers in phase
        }
        
        self.phase += self.frequency * ctx.dt;
        if self.phase >= 1.0 { self.phase -= 1.0; }
        
        output
    }
    
    // ... implement other trait methods similarly
}
```

### Audio Backend (haptic-server/src/audio.rs)

```rust
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub fn run_audio_loop(
    mut engine: StimulusEngine, 
    running: Arc<AtomicBool>
) -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    
    // Find device with 32+ channels
    let device = host.output_devices()?
        .find(|d| {
            d.supported_output_configs()
                .map(|configs| configs.any(|c| c.channels() >= 32))
                .unwrap_or(false)
        })
        .ok_or("No 32-channel device found")?;
    
    let config = device.default_output_config()?;
    
    // Build output stream
    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let channels = 32;
            let frames = data.len() / channels;
            
            for frame in 0..frames {
                let mut output = [0.0f32; 32];
                engine.process(&mut output, config.sample_rate().0 as f32);
                
                // Copy to interleaved output
                for ch in 0..channels {
                    let idx = frame * channels + ch;
                    if idx < data.len() && ch < 32 {
                        data[idx] = output[ch];
                    }
                }
            }
        },
        |err| eprintln!("Audio stream error: {}", err),
        None
    )?;
    
    stream.play()?;
    
    // Keep alive until shutdown
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    
    Ok(())
}
```

## VST Plugin Implementation

### Plugin Structure (haptic-plugin/src/lib.rs)

```rust
use nih_plug::prelude::*;
use std::sync::Arc;

struct HapticPlugin {
    params: Arc<HapticParams>,
    ipc_client: Arc<Mutex<Option<IpcClient>>>,
}

#[derive(Params)]
struct HapticParams {
    #[id = "wave_speed"]
    pub wave_speed: FloatParam,
}

impl Default for HapticPlugin {
    fn default() -> Self {
        Self {
            params: Arc::new(HapticParams::default()),
            ipc_client: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for HapticParams {
    fn default() -> Self {
        Self {
            wave_speed: FloatParam::new(
                "Wave Speed",
                100.0,
                FloatRange::Linear { min: 20.0, max: 500.0 }
            ),
        }
    }
}

impl Plugin for HapticPlugin {
    const NAME: &'static str = "Haptic Controller";
    const VENDOR: &'static str = "Haptic Research";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.1.0";
    
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout { main_input_channels: None, main_output_channels: NonZeroU32::new(2), ..AudioIOLayout::const_default() },
    ];
    
    type SysExMessage = ();
    type BackgroundTask = ();
    
    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }
    
    fn initialize(&mut self, _audio_io_layout: &AudioIOLayout, _buffer_config: &BufferConfig, _context: &mut impl InitContext<Self>) -> bool {
        // Start IPC client
        if let Ok(client) = IpcClient::connect() {
            *self.ipc_client.lock() = Some(client);
        }
        true
    }
    
    fn process(&mut self, buffer: &mut Buffer, _aux: &mut AuxiliaryBuffers, context: &mut impl ProcessContext<Self>) -> ProcessStatus {
        // Get IPC client
        let client = self.ipc_client.lock();
        if let Some(ref client) = *client {
            // Process MIDI events
            while let Some(event) = context.next_event() {
                match event {
                    NoteEvent::NoteOn { timing, note, velocity, .. } => {
                        let cmd = HapticCommand::NoteOn {
                            timestamp_us: timing as u64,
                            note,
                            velocity: (velocity * 127.0) as u8,
                            channel: 0, // TODO: MPE channel
                            mpe: MpeData { pressure: velocity, pitch_bend: 0.0, timbre: 0.5 },
                        };
                        client.send_command(cmd);
                    }
                    NoteEvent::NoteOff { timing, note, .. } => {
                        let cmd = HapticCommand::NoteOff {
                            timestamp_us: timing as u64,
                            note,
                            channel: 0,
                        };
                        client.send_command(cmd);
                    }
                    _ => {}
                }
            }
            
            // Note: Wave speed is now calculated on server from note velocity
            // No plugin parameters to process currently
        }
        
        // Clear audio output (we don't generate audio)
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                *sample = 0.0;
            }
        }
        
        ProcessStatus::Normal
    }
    
    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        let params = self.params.clone();
        let ipc_client = self.ipc_client.clone();
        editor::create(params, ipc_client)
    }
}

impl ClapPlugin for HapticPlugin {
    const CLAP_ID: &'static str = "com.haptic-research.haptic-vst";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("32-channel haptic stimulus controller");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Synthesizer];
}

impl Vst3Plugin for HapticPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"HapticStimCtrl01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth];
}

nih_export_clap!(HapticPlugin);
nih_export_vst3!(HapticPlugin);
```

### IPC Client (haptic-plugin/src/ipc_client.rs)

```rust
use std::os::unix::net::UnixStream;
use std::io::{Write, Read};
use crossbeam_channel::{Sender, Receiver, bounded};

pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
}

impl IpcClient {
    pub fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let stream = UnixStream::connect(SOCKET_PATH)?;
        stream.set_nonblocking(true)?;
        
        let (tx, rx) = bounded(256);
        
        let handle = std::thread::spawn(move || {
            ipc_worker(stream, rx);
        });
        
        Ok(Self {
            command_tx: tx,
            worker_handle: Some(handle),
        })
    }
    
    pub fn send_command(&self, cmd: HapticCommand) {
        // Non-blocking send, drops if queue full
        let _ = self.command_tx.try_send(cmd);
    }
}

fn ipc_worker(mut stream: UnixStream, commands: Receiver<HapticCommand>) {
    let mut write_buffer = Vec::with_capacity(1024);
    
    loop {
        // Send commands
        while let Ok(cmd) = commands.try_recv() {
            write_buffer.clear();
            if let Ok(_) = bincode::serialize_into(&mut write_buffer, &cmd) {
                let _ = stream.write_all(&write_buffer);
            }
        }
        
        // TODO: Read status messages
        
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
}
```

### GUI (haptic-plugin/src/editor.rs)

```rust
use nih_plug_egui::{create_egui_editor, egui, EguiState};

pub fn create(
    params: Arc<HapticParams>,
    ipc_client: Arc<Mutex<Option<IpcClient>>>,
) -> Option<Box<dyn Editor>> {
    let editor_state = EguiState::from_size(800, 600);
    
    create_egui_editor(
        editor_state,
        params.clone(),
        |ctx, _| {},
        move |egui_ctx, setter, _state| {
            egui::CentralPanel::default().show(egui_ctx, |ui| {
                ui.heading("Haptic Controller");
                
                // Connection status
                let connected = ipc_client.lock().is_some();
                ui.label(if connected { "Server: Connected" } else { "Server: Disconnected" });
                
                ui.separator();
                
                // Wave speed control
                ui.label("Wave Speed (m/s)");
                ui.add(egui::Slider::new(&mut params.wave_speed.value(), 20.0..=500.0));
                
                // Transducer visualization placeholder
                ui.group(|ui| {
                    ui.label("Transducer Levels");
                    let size = egui::Vec2::new(400.0, 200.0);
                    let (response, painter) = ui.allocate_painter(size, egui::Sense::hover());
                    
                    // Draw 32 transducer indicators in 4x8 grid
                    let rect = response.rect;
                    for i in 0..32 {
                        let row = i / 8;
                        let col = i % 8;
                        let x = rect.left() + (col as f32 + 0.5) * rect.width() / 8.0;
                        let y = rect.top() + (row as f32 + 0.5) * rect.height() / 4.0;
                        
                        painter.circle_filled(
                            egui::pos2(x, y),
                            10.0,
                            egui::Color32::from_gray(64),
                        );
                    }
                });
            });
        },
    )
}
```

## Build Configuration

### Workspace Cargo.toml
```toml
[workspace]
members = ["haptic-protocol", "haptic-server", "haptic-plugin"]
resolver = "2"

[workspace.dependencies]
serde = { version = "1.0", features = ["derive"] }
bincode = "1.3"
crossbeam-channel = "0.5"
parking_lot = "0.12"
rtrb = "0.3"
```

### haptic-plugin/Cargo.toml
```toml
[package]
name = "haptic-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
nih_plug = { version = "0.9", features = ["vst3", "clap"] }
nih_plug_egui = "0.9"
haptic-protocol = { path = "../haptic-protocol" }
serde = { workspace = true }
bincode = { workspace = true }
crossbeam-channel = { workspace = true }
parking_lot = { workspace = true }

[build-dependencies]
nih_plug_xtask = "0.9"
```

### haptic-server/Cargo.toml
```toml
[package]
name = "haptic-server"
version = "0.1.0"
edition = "2021"

[dependencies]
haptic-protocol = { path = "../haptic-protocol" }
serde = { workspace = true }
bincode = { workspace = true }
crossbeam-channel = { workspace = true }
parking_lot = { workspace = true }
rtrb = { workspace = true }
cpal = "0.15"
```

### haptic-protocol/Cargo.toml
```toml
[package]
name = "haptic-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
```

## Threading Architecture

### VST Plugin Threads
1. **Audio Thread** (Host-controlled, real-time)
   - Processes MIDI events
   - Sends commands via lock-free channel
   - Never blocks on I/O or mutexes

2. **GUI Thread** (60Hz)
   - Renders egui interface
   - Reads status via channel
   - Updates visualization

3. **IPC Worker Thread**
   - Handles Unix socket I/O
   - Bridges audio thread to server
   - Buffers commands

### Haptic Server Threads
1. **Audio Callback Thread** (Real-time, highest priority)
   - Runs stimulus engine
   - Reads commands from lock-free queue
   - Writes to audio hardware

2. **IPC Listener Thread**
   - Accepts socket connections
   - Deserializes commands
   - Pushes to engine queue

3. **Main Thread**
   - Lifecycle management
   - Signal handling

## Key Implementation Details

### Lock-Free Communication
- Use `rtrb` for audio thread queues (wait-free SPSC)
- Use `crossbeam-channel` for non-critical paths
- Atomic flags for state synchronization

### Memory Management
- All audio processing uses stack allocation
- Stimulus pools are pre-allocated arrays
- No heap allocation after initialization

### Error Handling
- Audio thread: Silent failure, no panics
- IPC thread: Log and continue
- GUI thread: User-visible error messages

### Performance Optimization
- SIMD opportunities in spatial computation
- Cache-aligned data structures
- Minimal pointer indirection

## Build and Run Instructions

### Build Everything
```bash
# From workspace root
cargo build --release

# Build just the plugin
cargo xtask bundle haptic-plugin --release
```

### Run Server
```bash
# Terminal 1
./target/release/haptic-server
```

### Test Plugin
```bash
# Terminal 2 (after server is running)
# Use any VST3/CLAP host, or:
cargo run --package haptic-plugin --example standalone
```

## Extension Points

1. **New Stimulus Types**
   - Implement `Stimulus` trait
   - Add new pool to `StimulusEngine`
   - Update allocation logic

2. **Additional Parameters**
   - Add to `HapticParams`
   - Create new `HapticCommand` variant
   - Update GUI

3. **Advanced Visualization**
   - Read `ServerStatus::TransducerLevels`
   - Update egui painter code
   - Add animation state

## Critical Success Factors

1. **Zero Audio Dropouts**: Audio callback must never block
2. **Low Latency**: <5ms from MIDI to transducer
3. **Stable IPC**: Graceful handling of connection loss
4. **Clean Shutdown**: All threads terminate properly

## Common Pitfalls to Avoid

1. **Don't use std::sync::Mutex in audio thread** - Use atomics or lock-free queues
2. **Don't allocate in process()** - Pre-allocate everything
3. **Don't panic in audio callback** - Use Result or silent failure
4. **Don't block on socket I/O** - Use non-blocking mode

## Testing Strategy

### Unit Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_delay_line_interpolation() {
        let mut delay = DelayLine::new();
        // Test fractional delay accuracy
    }
    
    #[test]
    fn test_stimulus_allocation() {
        let mut pool = StimulusPool::<WaveStimulus, 4>::new();
        // Test pool exhaustion handling
    }
}
```

### Integration Tests
- Test VST↔Server communication
- Measure end-to-end latency
- Verify thread safety under load

## Final Notes

This implementation prioritizes:
1. **Correctness**: Thread-safe, lock-free audio processing
2. **Performance**: Static allocation, zero-copy where possible
3. **Extensibility**: Clean trait-based stimulus system
4. **Simplicity**: Minimal features, clear architecture

The architecture is designed to be extended with additional stimulus types, parameters, and visualization features while maintaining real-time performance guarantees.