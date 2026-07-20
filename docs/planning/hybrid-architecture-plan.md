# Haptic VST Hybrid Architecture - Requirements & Implementation Plan

## Executive Summary

A hybrid architecture system for haptic stimulus generation consisting of:
- **Controller Plugin (VST3)**: MIDI/MPE input processing, parameter control, and visualization using nih-plug with egui
- **Haptic Server**: Dedicated real-time audio process with direct multichannel interface control
- **IPC Protocol**: Low-latency Unix socket communication between components

This architecture provides full control over 32 haptic transducers while maintaining DAW integration for sequencing and automation.

## System Architecture

### Component Overview

```
┌─────────────────┐         ┌──────────────────┐
│   DAW Host      │         │  Haptic Server   │
│                 │         │                  │
│  ┌───────────┐  │  IPC    │  ┌────────────┐ │
│  │VST Plugin │◄─┼─────────┼─►│  Stimulus  │ │
│  │(nih-plug) │  │ Socket  │  │   Engine   │ │
│  └─────┬─────┘  │         │  └──────┬─────┘ │
│        │        │         │         │       │
│   ┌────▼────┐   │         │  ┌─────▼─────┐ │
│   │  egui   │   │         │  │32-Channel │ │
│   │   GUI   │   │         │  │Audio I/O  │ │
│   └─────────┘   │         │  └───────────┘ │
└─────────────────┘         └──────────────────┘
```

### Core Design Principles

1. **Zero-allocation real-time processing**: Static memory pools, no heap allocation in audio threads
2. **Lock-free communication**: Ring buffers for IPC, atomic state updates
3. **Parallel processing**: Spatial field computation across multiple cores
4. **Graceful degradation**: Server continues operation if VST disconnects
5. **Hot-reload configuration**: Live transducer layout updates without audio interruption

## Technical Requirements

### Controller Plugin (VST3)

#### Core Functionality
- **MIDI/MPE Processing**: Full MPE specification support with per-note expression
- **Parameter Management**: Automated parameters for DAW integration
- **Visual Feedback**: Real-time visualization at 120Hz target framerate
- **Configuration Control**: Load/save transducer layouts and stimulus presets
- **Server Monitoring**: Connection status, performance metrics, error reporting

#### Technical Specifications
- **Framework**: nih-plug 0.x with egui for GUI
- **Thread Safety**: Arc<Mutex<>> for shared state, lock-free queues for audio thread
- **Memory Model**: Bounded allocations, pre-allocated buffers
- **Update Rate**: Audio thread at host buffer rate, GUI at 60-120Hz

### Haptic Server

#### Core Functionality
- **Stimulus Generation**: Multiple concurrent stimuli with superposition
- **Spatial Processing**: Wave propagation with Doppler effects
- **Hardware Control**: Direct multichannel audio interface management
- **Configuration**: Hot-reload of transducer positions and calibration

#### Technical Specifications
- **Audio Backend**: CoreAudio (macOS), WASAPI (Windows), ALSA/JACK (Linux)
- **Processing Model**: Static allocation pools, zero runtime allocation
- **Parallelism**: SIMD operations, parallel spatial field computation
- **Latency Target**: <5ms total system latency

### IPC Protocol

#### Communication Architecture
```rust
// Command messages (VST → Server)
#[derive(Serialize, Deserialize, Clone)]
pub enum HapticCommand {
    // Lifecycle
    Connect { client_id: Uuid, version: Version },
    Disconnect { client_id: Uuid },
    
    // MIDI/MPE Events
    NoteOn { 
        timestamp: u64,
        note: u8, 
        velocity: u8, 
        channel: u8,
        pressure: f32,
        pitch_bend: f32,
        timbre: f32,
    },
    NoteOff { 
        timestamp: u64,
        note: u8, 
        channel: u8,
    },
    MpeUpdate {
        timestamp: u64,
        channel: u8,
        pressure: Option<f32>,
        pitch_bend: Option<f32>,
        timbre: Option<f32>,
    },
    
    // Parameter Updates
    SetParameter { param_id: u32, value: f32 },
    LoadConfiguration { config: TransducerConfig },
    
    // Control
    Panic, // Stop all stimuli immediately
    Reset, // Clear all state
}

// Status messages (Server → VST)
#[derive(Serialize, Deserialize, Clone)]
pub enum ServerStatus {
    // Connection
    Connected { server_version: Version },
    Disconnected { reason: String },
    
    // Performance
    PerformanceMetrics {
        cpu_usage: f32,
        active_stimuli: usize,
        buffer_underruns: u32,
        average_latency_us: u32,
    },
    
    // Visualization Data
    TransducerLevels {
        timestamp: u64,
        levels: [f32; 32],
        stimulus_types: [Option<StimulusType>; 32],
    },
    
    // Errors
    Error { code: ErrorCode, message: String },
}
```

#### Transport Specifications
- **Protocol**: Unix domain sockets (primary), TCP sockets (fallback)
- **Serialization**: bincode for efficiency, MessagePack as alternative
- **Buffer Size**: 64KB ring buffers, separate for commands and status
- **Timing**: Microsecond-precision timestamps, NTP sync for distributed setups

## Detailed Implementation Plan

### Phase 1: Foundation (3 weeks)

#### Week 1: Core Infrastructure
- [ ] Set up Rust workspace with shared crate for protocol types
- [ ] Implement lock-free ring buffer for IPC communication
- [ ] Create Unix socket abstraction with automatic reconnection
- [ ] Design command/status protocol with versioning support
- [ ] Implement basic serialization with bincode

#### Week 2: Haptic Server Core
- [ ] Port static allocation stimulus engine to server architecture
- [ ] Implement CoreAudio backend for 32-channel output
- [ ] Create server lifecycle management (start/stop/configure)
- [ ] Add command processing loop with timing guarantees
- [ ] Implement performance monitoring subsystem

#### Week 3: VST Plugin Foundation
- [ ] Create nih-plug project structure
- [ ] Implement IPC client with connection management
- [ ] Add basic MIDI/MPE event forwarding
- [ ] Create parameter structure for automation
- [ ] Implement plugin state serialization

### Phase 2: Stimulus Engine Enhancement (2 weeks)

#### Week 4: Parallel Processing
- [ ] Refactor spatial field computation for parallel execution
- [ ] Implement SIMD optimizations for delay line processing
- [ ] Add work-stealing thread pool for stimulus updates
- [ ] Create parallel transducer output mixing
- [ ] Profile and optimize cache usage patterns

#### Week 5: Advanced Features
- [ ] Complete Doppler effect implementation
- [ ] Add SpatialSweep stimulus with Bezier paths
- [ ] Implement ChaoticNetwork with coupled oscillators
- [ ] Add stimulus parameter interpolation
- [ ] Create preset system for stimulus configurations

### Phase 3: GUI Development (3 weeks)

#### Week 6: egui Integration
- [ ] Set up egui with nih-plug editor framework
- [ ] Create responsive layout system
- [ ] Implement theme system with dark/light modes
- [ ] Add font loading and scaling
- [ ] Create reusable widget library

#### Week 7: Visualization Components
- [ ] Design and implement transducer grid visualization
- [ ] Create real-time level meters with peak hold
- [ ] Add stimulus type indicators with animations
- [ ] Implement wave propagation visualization
- [ ] Create MPE parameter displays

#### Week 8: Control Interface
- [ ] Build parameter editing panels
- [ ] Add configuration file browser/editor
- [ ] Implement server connection status display
- [ ] Create performance monitoring dashboard
- [ ] Add preset management interface

### Phase 4: Integration and Testing (2 weeks)

#### Week 9: System Integration
- [ ] Implement comprehensive error handling
- [ ] Add automatic server discovery
- [ ] Create installer with server daemon setup
- [ ] Implement configuration migration tools
- [ ] Add comprehensive logging system

#### Week 10: Testing and Optimization
- [ ] Create automated test suite for IPC protocol
- [ ] Implement stress testing for maximum stimuli
- [ ] Profile and optimize GUI rendering
- [ ] Test with various audio interfaces
- [ ] Create benchmark suite for latency measurement

### Phase 5: Polish and Documentation (1 week)

#### Week 11: Final Polish
- [ ] Create user manual with illustrations
- [ ] Document configuration file format
- [ ] Add in-app help system
- [ ] Create video tutorials
- [ ] Package for distribution

## Thread Architecture

### VST Plugin Threads

```rust
// Audio Thread (Real-time, Lock-free)
struct AudioThread {
    command_queue: spsc::Producer<HapticCommand>,
    midi_events: RingBuffer<MidiEvent>,
}

// GUI Thread (60-120Hz)
struct GuiThread {
    shared_state: Arc<Mutex<PluginState>>,
    status_receiver: mpsc::Receiver<ServerStatus>,
    frame_limiter: FrameLimiter,
}

// IPC Thread (Dedicated I/O)
struct IpcThread {
    socket: UnixStream,
    command_consumer: spsc::Consumer<HapticCommand>,
    status_broadcaster: mpsc::Sender<ServerStatus>,
}
```

### Haptic Server Threads

```rust
// Audio Callback Thread (Highest Priority)
struct AudioCallbackThread {
    stimulus_engine: Pin<Box<StimulusEngine>>,
    output_buffer: [f32; 32],
}

// Command Processing Thread
struct CommandThread {
    command_receiver: Receiver<HapticCommand>,
    engine_handle: Arc<AtomicEngineHandle>,
}

// Parallel Computation Workers
struct ComputeWorker {
    work_queue: WorkStealingQueue<SpatialComputation>,
    simd_context: SimdContext,
}
```

## Memory Management Strategy

### Static Allocation Pools

```rust
// Fixed-size pools with generational indices
pub struct GenerationalPool<T, const N: usize> {
    items: [T; N],
    generations: [u32; N],
    free_list: ArrayVec<usize, N>,
}

// Per-stimulus-type pools
pub struct MemoryPools {
    wave_pool: GenerationalPool<DelayLineWave, 8>,
    sweep_pool: GenerationalPool<SpatialSweep, 4>,
    chaotic_pool: GenerationalPool<ChaoticNetwork, 4>,
    
    // Shared delay line memory
    delay_memory: Box<[f32; TOTAL_DELAY_SAMPLES]>,
    delay_allocator: BumpAllocator,
}
```

### Lock-free Data Structures

```rust
// Wait-free ring buffer for commands
pub struct CommandRing {
    buffer: CachePadded<[MaybeUninit<HapticCommand>; 1024]>,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
}

// Atomic state for cross-thread visibility
pub struct AtomicState {
    active_stimuli: AtomicU32,
    cpu_usage: AtomicU32, // Fixed-point percentage
    error_flags: AtomicU32,
}
```

## Performance Requirements

### Latency Targets
- **MIDI to transducer output**: <5ms total
- **IPC command delivery**: <100μs average
- **GUI update rate**: 60Hz minimum, 120Hz target
- **Parameter smoothing**: 1ms rise time

### Resource Constraints
- **CPU Usage**: <10% on modern 4-core processor
- **Memory Usage**: <50MB total allocation
- **Real-time Allocations**: Zero after initialization
- **Cache Misses**: <5% in audio callback

### Scalability
- **Maximum concurrent stimuli**: 16 (configurable)
- **Transducer count**: 32 (extensible to 64)
- **Sample rates**: 44.1kHz, 48kHz, 96kHz
- **Buffer sizes**: 64-2048 samples

## Configuration Management

### File Formats

```toml
# transducer_config.toml
[layout]
name = "Standard 4x8 Grid"
version = "1.0"

[[transducers]]
id = 0
position = [0.0, 0.0]  # meters
gain_reduction_db = 0.0
frequency_response = "flat"

# ... repeated for all 32 transducers

[calibration]
reference_level_db = -20.0
safety_limiter_threshold_db = -3.0
```

### Hot Reload Implementation

```rust
pub struct ConfigWatcher {
    watcher: notify::RecommendedWatcher,
    config_path: PathBuf,
    update_channel: mpsc::Sender<TransducerConfig>,
}

impl ConfigWatcher {
    pub fn watch(&mut self) -> Result<(), Error> {
        self.watcher.watch(&self.config_path, RecursiveMode::NonRecursive)?;
        // Debounced updates sent to server
        Ok(())
    }
}
```

## Testing Strategy

### Unit Tests
- Stimulus generation accuracy
- Delay line interpolation
- MPE parameter mapping
- IPC protocol serialization

### Integration Tests
- VST ↔ Server communication
- Configuration hot-reload
- Multi-client connections
- Error recovery scenarios

### Performance Tests
- Maximum stimulus count benchmarks
- Latency measurement suite
- CPU usage profiling
- Memory allocation tracking

### User Acceptance Tests
- DAW compatibility matrix
- Audio interface compatibility
- Extended session stability
- Preset recall accuracy

## Risk Mitigation

### Technical Risks

1. **Real-time Performance**
   - Mitigation: Extensive profiling, conservative pool sizes
   - Fallback: Reduce stimulus count or disable visualizations

2. **IPC Latency**
   - Mitigation: Lock-free queues, dedicated I/O thread
   - Fallback: Direct shared memory if sockets too slow

3. **GUI Performance Impact**
   - Mitigation: Separate render thread, frame limiting
   - Fallback: Simplified visualization mode

### Operational Risks

1. **Server Crash**
   - Mitigation: Automatic restart, state persistence
   - Fallback: Emergency stop button in hardware

2. **Configuration Errors**
   - Mitigation: Schema validation, safe defaults
   - Fallback: Built-in reference configuration

## Success Metrics

### Functional
- [ ] Stable operation for 24+ hour sessions
- [ ] Support for all major DAWs
- [ ] Full MPE specification compliance
- [ ] Hot configuration reload without glitches

### Performance  
- [ ] <5ms total system latency
- [ ] 120Hz GUI updates without audio impact
- [ ] <10% CPU usage at full stimulus load
- [ ] Zero memory allocations during operation

### Usability
- [ ] Intuitive visual feedback
- [ ] Clear error messages
- [ ] Comprehensive documentation
- [ ] One-click installation

## Timeline Summary

**Total Duration**: 11 weeks

1. **Foundation**: 3 weeks
2. **Stimulus Engine**: 2 weeks  
3. **GUI Development**: 3 weeks
4. **Integration**: 2 weeks
5. **Polish**: 1 week

The modular architecture allows parallel development of server and plugin components after the foundation phase, potentially reducing total development time.
