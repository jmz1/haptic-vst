# Haptic VST Architecture and Rust Guide

This document explains the architecture of the 32-channel haptic VST plugin system and comprehensively covers the Rust features, syntax, and idioms used throughout the project.

## Table of Contents

1. [Project Overview](#project-overview)
2. [Architecture Overview](#architecture-overview)
3. [Rust Fundamentals Used](#rust-fundamentals-used)
4. [Module-by-Module Analysis](#module-by-module-analysis)
5. [Key Rust Patterns and Idioms](#key-rust-patterns-and-idioms)
6. [Thread Safety and Concurrency](#thread-safety-and-concurrency)
7. [Memory Management](#memory-management)
8. [Error Handling](#error-handling)
9. [Building and Running](#building-and-running)

## Project Overview

This is a real-time haptic stimulus system consisting of:
- **VST3 Plugin**: A digital audio workstation (DAW) plugin that receives MIDI input
- **Server Application**: A real-time audio processing engine that drives 32 haptic transducers
- **IPC Protocol**: Inter-process communication between plugin and server via Unix domain sockets

The system converts MIDI notes into spatial haptic stimuli with wave propagation simulation.

## Architecture Overview

```
┌─────────────────┐    Unix Socket    ┌──────────────────┐
│   VST3 Plugin   │ ←─────────────→   │   Haptic Server  │
│                 │   (IPC Protocol)  │                  │
│ • MIDI Input    │                   │ • Stimulus Engine│
│ • GUI Interface │                   │ • Audio Output   │
│ • Parameter Ctrl│                   │ • 32 Channels    │
└─────────────────┘                   └──────────────────┘
        │                                       │
        │                                       │
    ┌───▼───┐                               ┌───▼────┐
    │  DAW  │                               │ Audio  │
    │       │                               │Hardware│
    └───────┘                               └────────┘
```

### Components

1. **haptic-protocol**: Shared data structures and communication protocol
2. **haptic-plugin**: VST3 plugin with GUI and MIDI processing
3. **haptic-server**: Real-time audio engine with haptic stimulus generation

## Rust Fundamentals Used

### 1. Ownership and Borrowing

**Concept**: Rust's memory safety without garbage collection through ownership rules.

```rust
// Ownership transfer (move)
let engine = StimulusEngine::new();
let wrapped_engine = Arc::new(Mutex::new(engine)); // engine is moved

// Borrowing (references)
fn process_audio(engine: &mut StimulusEngine) { // Mutable borrow
    // Use engine without taking ownership
}

// Immutable reference
fn read_config(config: &Config) { // Immutable borrow
    // Can read but not modify
}
```

**In our project**: Used extensively for passing audio buffers, configuration data, and sharing state between threads.

### 2. Lifetimes

**Concept**: Annotations that ensure references are valid for as long as needed.

```rust
// From haptic-server/src/engine.rs
pub struct ProcessContext<'a> {
    pub sample_rate: f32,
    pub dt: f32,
    pub wave_speed: f32,
    pub transducer_positions: &'a [(f32, f32); TRANSDUCER_COUNT],
    //                        ^^^ Lifetime annotation
}
```

**Explanation**: The `'a` lifetime ensures that `ProcessContext` cannot outlive the data it references. This prevents dangling pointers at compile time.

### 3. Traits

**Concept**: Similar to interfaces in other languages, defining shared behavior.

```rust
// From haptic-server/src/engine.rs
pub trait Stimulus: Send + Sync {
    fn process(&mut self, context: &ProcessContext<'_>) -> [f32; TRANSDUCER_COUNT];
    fn is_active(&self) -> bool;
    fn note_on(&mut self, note: u8, velocity: u8, mpe: MpeData);
    fn note_off(&mut self);
    fn mpe_update(&mut self, mpe: MpeData);
    fn reset(&mut self);
}
```

**Explanation**: 
- `Send + Sync`: Trait bounds ensuring thread safety
- All stimulus types must implement these methods
- Enables polymorphism and code reuse

### 4. Generics and Type Parameters

```rust
// From haptic-server/src/engine.rs
pub struct StimulusPool<T: Stimulus + Default, const N: usize> {
    stimuli: [T; N],
    active_mask: [bool; N],
}
//               ^^^^^^^^^^ Generic type with trait bounds
//                              ^^^^^^^^^^^^^ Const generic for array size
```

**Explanation**:
- `T`: Generic type that must implement `Stimulus + Default`
- `const N: usize`: Compile-time constant for array size
- Enables code reuse for different stimulus types and pool sizes

### 5. Pattern Matching

```rust
// From haptic-plugin/src/lib.rs
match event {
    NoteEvent::NoteOn { note, velocity, channel, .. } => {
        let cmd = HapticCommand::NoteOn {
            timestamp_us,
            note,
            velocity: (velocity * 127.0) as u8,
            channel: channel as u8,
            mpe: MpeData { /* ... */ },
        };
        let _ = client.send_command(cmd);
    }
    NoteEvent::NoteOff { note, channel, .. } => {
        // Handle note off
    }
    _ => {} // Catch-all for unhandled events
}
```

**Explanation**: Pattern matching is Rust's primary control flow mechanism, more powerful than switch statements in C.

### 6. Error Handling with Result<T, E>

```rust
// From haptic-server/src/ipc.rs
pub fn listen_loop(
    running: Arc<AtomicBool>,
    command_producer: crossbeam_channel::Sender<crate::engine::EngineCommand>
) -> Result<(), Box<dyn std::error::Error>> {
    //   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ Result type
    
    let listener = UnixListener::bind(SOCKET_PATH)?;
    //                                            ^ ? operator for error propagation
    Ok(())
}
```

**Explanation**: Rust uses `Result<T, E>` instead of exceptions. The `?` operator propagates errors up the call stack.

### 7. Smart Pointers

```rust
// Arc: Atomic Reference Counter for shared ownership across threads
let command_producer: Arc<Sender<EngineCommand>> = Arc::new(sender);

// Mutex: Mutual exclusion for thread-safe access
let engine = Arc::new(Mutex::new(stimulus_engine));

// Box: Heap allocation (similar to malloc/new)
let error: Box<dyn std::error::Error> = Box::new(io_error);
```

## Module-by-Module Analysis

### haptic-protocol/src/lib.rs

**Purpose**: Shared data structures and constants for IPC communication.

```rust
use serde::{Deserialize, Serialize};
//    ^^^^ External crate for serialization
```

**Key Rust Features**:
- **Derive macros**: `#[derive(Serialize, Deserialize, Clone, Debug)]`
  - Automatically generates implementations
  - Similar to Python's dataclass or C++ template specialization
  
- **Enums with data**: 
```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum HapticCommand {
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
    // ...
}
```
**Explanation**: Unlike C enums, Rust enums can carry data. This is similar to tagged unions but type-safe.

- **Struct definitions**:
```rust
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MpeData {
    pub pressure: f32,    // 0.0 to 1.0
    pub pitch_bend: f32,  // -1.0 to 1.0
    pub timbre: f32,      // 0.0 to 1.0
}
```

### haptic-plugin/src/lib.rs

**Purpose**: Main VST3 plugin implementation using the NIH-plug framework.

**Key Rust Features**:

1. **Struct with trait implementations**:
```rust
struct HapticPlugin {
    params: Arc<HapticParams>,
    ipc_client: Arc<Mutex<Option<IpcClient>>>,
}

impl Plugin for HapticPlugin {
    const NAME: &'static str = "Haptic Controller";
    // ...
}
```

2. **Associated constants**: `const NAME: &'static str`
   - Similar to static class members in C++
   - `&'static str`: String slice with static lifetime

3. **Option<T> type**:
```rust
ipc_client: Arc<Mutex<Option<IpcClient>>>
//                    ^^^^^^^^^^^^^^^^^ Option for nullable values
```
**Explanation**: Rust doesn't have null pointers. `Option<T>` is an enum: `Some(T)` or `None`.

4. **Closure syntax**:
```rust
while let Some(event) = context.next_event() {
    // Process event
}
```

5. **Method chaining and builder pattern**:
```rust
FloatParam::new(
    "Wave Speed",
    100.0,
    FloatRange::Linear { min: 20.0, max: 500.0 }
).with_smoother(SmoothingStyle::Linear(50.0))
```

### haptic-plugin/src/editor.rs

**Purpose**: GUI implementation using egui (immediate mode GUI).

**Key Rust Features**:

1. **Function pointers and closures**:
```rust
create_egui_editor(
    editor_state,
    params.clone(),
    |_ctx, _setter| {
        // Initialize callback
    },
    move |egui_ctx, setter, _state| {
        // Update callback - 'move' captures variables by value
        // ...
    },
)
```

2. **Move semantics**: `move |args| { ... }`
   - Transfers ownership of captured variables into the closure
   - Essential for threading and callbacks

3. **Method chaining for UI**:
```rust
ui.horizontal(|ui| {
    ui.label("Wave Speed (m/s):");
    ui.add(
        ParamSlider::for_param(&params.wave_speed, setter).with_width(200.0)
    );
});
```

### haptic-server/src/engine.rs

**Purpose**: Real-time audio processing engine with stimulus generation.

**Key Rust Features**:

1. **Const generics and arrays**:
```rust
const TRANSDUCER_COUNT: usize = 32;
type TransducerArray = [f32; TRANSDUCER_COUNT];
//                     ^^^^^^^^^^^^^^^^^^^^^^^ Fixed-size array
```

2. **Default trait implementation**:
```rust
#[derive(Default)]
pub struct WaveStimulus {
    delay_lines: [DelayLine; TRANSDUCER_COUNT],
    frequency: f32,
    // ...
}
```

3. **Static methods**:
```rust
impl StimulusEngine {
    pub fn new() -> Self {  // Associated function (static method)
        // ...
    }
    
    pub fn process(&mut self, output: &mut [f32; TRANSDUCER_COUNT], sample_rate: f32) {
        // Instance method
    }
}
```

4. **Complex generic bounds**:
```rust
impl<T: Stimulus + Default, const N: usize> StimulusPool<T, N> {
    pub fn new() -> Self {
        Self {
            stimuli: std::array::from_fn(|_| T::default()),
            //       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ Array initialization
            active_mask: [false; N],
        }
    }
}
```

5. **Channel-based communication**:
```rust
let (sender, receiver) = crossbeam_channel::unbounded();
// sender: crossbeam_channel::Sender<EngineCommand>
// receiver: crossbeam_channel::Receiver<EngineCommand>
```

### haptic-server/src/audio.rs

**Purpose**: Audio system interface using CPAL (Cross-Platform Audio Library).

**Key Rust Features**:

1. **Error propagation chain**:
```rust
pub fn run_audio_loop(
    engine: StimulusEngine, 
    running: Arc<AtomicBool>
) -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.output_devices()?  // ? propagates errors
        .find(|d| {
            // Closure with error handling
        })
        .unwrap_or_else(|| {  // Fallback if None
            host.default_output_device().expect("No output device available")
        });
    // ...
}
```

2. **Iterator methods**:
```rust
let device = host.output_devices()?
    .find(|d| {  // find() returns Option<T>
        if let Ok(mut configs) = d.supported_output_configs() {
            configs.any(|c| c.channels() >= 32)  // any() returns bool
        } else {
            false
        }
    })
```

3. **Thread-safe shared state**:
```rust
let engine = Arc::new(Mutex::new(engine));
let engine_clone = engine.clone();  // Clone the Arc, not the data

// In audio callback:
if let Some(mut engine_guard) = engine_clone.try_lock() {
    engine_guard.process(&mut output, sample_rate);
}
```

## Key Rust Patterns and Idioms

### 1. RAII (Resource Acquisition Is Initialization)

```rust
{
    let _guard = mutex.lock();  // Acquires lock
    // Critical section
} // Lock automatically released when _guard goes out of scope
```

### 2. Builder Pattern with Method Chaining

```rust
FloatParam::new("Wave Speed", 100.0, range)
    .with_smoother(SmoothingStyle::Linear(50.0))
    .with_unit(" m/s")
    .with_value_to_string(formatters::float_formatter())
```

### 3. Type State Pattern

```rust
pub struct StimulusEngine {
    // State transitions controlled by types
    wave_pool: StimulusPool<WaveStimulus, MAX_WAVE_STIMULI>,
    standing_pool: StimulusPool<StandingWaveStimulus, MAX_STANDING_STIMULI>,
}
```

### 4. Zero-Cost Abstractions

```rust
// This compiles to the same code as manual loop unrolling
for (i, stimulus) in self.stimuli.iter_mut().enumerate() {
    if self.active_mask[i] && stimulus.is_active() {
        let output = stimulus.process(context);
        // ...
    }
}
```

### 5. Newtype Pattern

```rust
struct Frequency(f32);  // Wraps f32 but with semantic meaning
struct Velocity(u8);
```

## Thread Safety and Concurrency

### 1. Send and Sync Traits

```rust
pub trait Stimulus: Send + Sync {
    // Send: Can be transferred between threads
    // Sync: Can be accessed from multiple threads simultaneously
}
```

### 2. Atomic Types

```rust
use std::sync::atomic::{AtomicBool, Ordering};

let running = Arc::new(AtomicBool::new(true));
// Lock-free atomic operations
running.store(false, Ordering::Relaxed);
let is_running = running.load(Ordering::Relaxed);
```

### 3. Channel Communication

```rust
// Multiple producer, single consumer
let (sender, receiver) = crossbeam_channel::unbounded();

// Producer thread
sender.send(command)?;

// Consumer thread  
while let Ok(cmd) = receiver.try_recv() {
    process_command(cmd);
}
```

### 4. Mutex for Shared Mutable State

```rust
let shared_data = Arc::new(Mutex::new(data));

// Thread-safe access
{
    let mut guard = shared_data.lock();
    guard.modify_data();
} // Lock released automatically
```

## Memory Management

### 1. Stack vs Heap Allocation

```rust
// Stack allocated (fixed size, fast)
let array = [0.0f32; 32];
let small_struct = Point { x: 1.0, y: 2.0 };

// Heap allocated (dynamic size, slower)
let vector = Vec::new();  // Growable array
let boxed = Box::new(large_struct);  // Explicit heap allocation
```

### 2. Reference Counting

```rust
// Single threaded reference counting
let rc_data = Rc::new(data);

// Multi-threaded atomic reference counting
let arc_data = Arc::new(data);
let arc_clone = arc_data.clone();  // Increments reference count
// When last Arc is dropped, data is deallocated
```

### 3. Copy vs Clone vs Move

```rust
// Copy: Implicit duplication for small types
let a = 5i32;
let b = a;  // a is copied, both a and b are valid

// Clone: Explicit duplication
let vec1 = vec![1, 2, 3];
let vec2 = vec1.clone();  // Deep copy

// Move: Transfer ownership (default for large types)
let vec1 = vec![1, 2, 3];
let vec2 = vec1;  // vec1 is no longer valid
```

## Error Handling

### 1. Result<T, E> Type

```rust
fn might_fail() -> Result<String, std::io::Error> {
    std::fs::read_to_string("file.txt")  // Returns Result
}

// Usage
match might_fail() {
    Ok(content) => println!("File content: {}", content),
    Err(error) => eprintln!("Error reading file: {}", error),
}
```

### 2. Error Propagation with ?

```rust
fn chain_operations() -> Result<(), Box<dyn std::error::Error>> {
    let listener = UnixListener::bind(path)?;  // Propagates error if bind fails
    listener.set_nonblocking(true)?;           // Propagates error if this fails
    Ok(())  // Success case
}
```

### 3. Option<T> for Nullable Values

```rust
fn find_stimulus(&mut self) -> Option<&mut WaveStimulus> {
    for (i, active) in self.active_mask.iter_mut().enumerate() {
        if !*active {
            *active = true;
            return Some(&mut self.stimuli[i]);  // Found inactive stimulus
        }
    }
    None  // No inactive stimulus available
}

// Usage
if let Some(stimulus) = pool.find_stimulus() {
    stimulus.note_on(note, velocity, mpe);
}
```

## Building and Running

### 1. Cargo Workspace

```toml
# Root Cargo.toml
[workspace]
members = ["haptic-protocol", "haptic-server", "haptic-plugin"]
resolver = "2"

[workspace.dependencies]
serde = { version = "1.0", features = ["derive"] }
bincode = "1.3"
```

**Explanation**: Workspaces allow multiple related packages in one repository with shared dependencies.

### 2. Feature Flags

```toml
# haptic-plugin/Cargo.toml
[dependencies]
nih_plug = { git = "https://github.com/robbert-vdh/nih-plug.git", features = ["vst3"] }
```

### 3. Build Commands

```bash
cargo build              # Build all workspace members
cargo build --release    # Optimized build
cargo run --bin haptic-server  # Run specific binary
cargo test               # Run tests
```

## Comparison to C and Python

### Memory Management
- **C**: Manual malloc/free, prone to leaks and segfaults
- **Python**: Garbage collection, automatic but with runtime overhead
- **Rust**: Ownership system, memory safety at compile time with zero runtime cost

### Error Handling
- **C**: Return codes, easy to ignore
- **Python**: Exceptions, can be caught anywhere in call stack
- **Rust**: Result types, must be explicitly handled

### Concurrency
- **C**: Manual thread management, data races possible
- **Python**: GIL limits true parallelism, thread-safe collections
- **Rust**: Compile-time prevention of data races, zero-cost abstractions

### Performance
- **C**: Full control, minimal abstraction overhead
- **Python**: Interpreted, slower but more productive
- **Rust**: Zero-cost abstractions, C-like performance with safety

## Learning Resources

1. **The Rust Book**: https://doc.rust-lang.org/book/
2. **Rust by Example**: https://doc.rust-lang.org/rust-by-example/
3. **Rustlings**: Interactive exercises
4. **Rust Standard Library Documentation**: https://doc.rust-lang.org/std/

This haptic VST project demonstrates real-world Rust usage in a performance-critical, multi-threaded application with complex state management and external C library integration.