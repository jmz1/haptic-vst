# Haptic VST Plugin Implementation Plan

## Project Overview

A real-time VST plugin for generating haptic stimulus patterns across 32 vibratory transducers, designed for research into haptic phenomenology and nervous system healing applications. The plugin supports multiple stimulus generation methods with MPE (MIDI Polyphonic Expression) control and real-time spatial visualization.

## Core Design Requirements

### Hardware Specifications
- **32 vibratory transducers** in fixed configuration
- **Frequency range**: 20-200 Hz stimulus generation
- **Target platform**: macOS Sequoia (primary), VST3 format
- **Real-time performance**: Low-latency audio processing requirements
- **Spatial configuration**: User-configurable transducer positions via external file

### Technical Architecture
- **Language**: Rust with `vst` crate ecosystem
- **Memory model**: Static allocation with bounded pools (zero heap allocation in real-time path)
- **Concurrency**: Real-time audio thread safety
- **Configuration**: External files for transducer layout and gain settings

### Core Functionality
- **Multiple stimulus types** with distinct generation algorithms
- **Superposition support** for simultaneous multiple stimuli
- **MPE integration** for expressive real-time control
- **Spatial wave simulation** using delay lines with Doppler effects
- **Visual feedback** with 120Hz-capable real-time display
- **ADSR envelope control** for stimulus lifecycle management

## Current Implementation Status

### ✅ Completed Core Architecture
- **Static allocation system** with typed stimulus pools
- **Stimulus trait hierarchy** for extensible stimulus types
- **Delay-line wave simulation** with fractional interpolation
- **MPE event handling** and parameter mapping system
- **ADSR envelope implementation** for stimulus lifecycle
- **Spatial field representation** with transducer output arrays
- **Superposition system** for combining multiple stimulus sources
- **Error handling** with graceful degradation and logging

### ✅ Implemented Stimulus Types
- **DelayLineWave**: Full implementation with Doppler effects, distance attenuation, and MPE control
- **SpatialSweep**: Stubbed interface ready for path-based movement patterns
- **ChaoticNetwork**: Stubbed interface ready for oscillator network implementation

### ✅ Configuration System
- **TransducerConfig**: Position and gain reduction support
- **Parameter mapping**: Configurable MPE-to-stimulus parameter conversion
- **Velocity-based stimulus selection**: MIDI velocity ranges determine stimulus type

## Implementation Plan

### Phase 1: VST Integration Foundation (2-3 weeks)

#### 1.1 VST Wrapper Implementation
- [ ] Set up `vst` crate project structure
- [ ] Implement basic VST3 plugin interface
- [ ] Create parameter management system for plugin automation
- [ ] Integrate stimulus engine with VST audio processing callbacks
- [ ] Implement MIDI/MPE event routing from VST to stimulus engine

#### 1.2 Configuration System
- [ ] Design transducer configuration file format (TOML/JSON)
- [ ] Implement configuration file parsing with `serde`
- [ ] Add hot-reload capability for configuration changes
- [ ] Create default configuration templates
- [ ] Add configuration validation and error reporting

#### 1.3 Basic Audio Output
- [ ] Integrate stimulus engine with VST audio buffers
- [ ] Implement sample rate adaptation
- [ ] Add output gain and limiting for safety
- [ ] Test basic wave stimulus generation through DAW

### Phase 2: GUI Development (3-4 weeks)

#### 2.1 GUI Framework Setup
- [ ] Choose and integrate GUI framework (`egui` or `iced`)
- [ ] Set up VST GUI lifecycle management
- [ ] Create basic plugin window and layout system
- [ ] Implement GUI-to-audio thread communication

#### 2.2 Real-time Visualization
- [ ] Design spatial transducer layout display
- [ ] Implement real-time output level visualization (120Hz target)
- [ ] Add color mapping for stimulus intensity
- [ ] Create stimulus type indicators and activity display
- [ ] Add MPE parameter visualization (pressure, pitch bend, timbre)

#### 2.3 Debug and Control Interface
- [ ] Parameter editing interface for stimulus settings
- [ ] Real-time envelope and phase visualization
- [ ] Stimulus pool status and allocation display
- [ ] Configuration file editor with live preview
- [ ] MIDI input monitoring and event display

### Phase 3: Stimulus Enhancement (2-3 weeks)

#### 3.1 Wave Simulation Refinement
- [ ] Implement proper Doppler shift calculations
- [ ] Add variable wave propagation characteristics
- [ ] Optimize delay line performance for real-time use
- [ ] Add wave reflection and boundary condition support
- [ ] Implement frequency-dependent attenuation models

#### 3.2 Additional Stimulus Types
- [ ] **SpatialSweep**: Implement path-based movement patterns
  - Bezier curve path definition
  - Variable sweep speed with MPE control
  - Path looping and reversing modes
- [ ] **ChaoticNetwork**: Implement coupled oscillator systems
  - Lorenz or van der Pol oscillator networks
  - Spatial coupling between transducers
  - Coherence evolution over time
  - MPE control of coupling strength and network topology

#### 3.3 Advanced MPE Features
- [ ] Per-channel MPE parameter tracking
- [ ] Advanced parameter mapping curves (exponential, logarithmic)
- [ ] MPE gesture recording and playback
- [ ] Multi-dimensional parameter modulation

### Phase 4: Testing and Optimization (2-3 weeks)

#### 4.1 Performance Optimization
- [ ] Profile real-time audio processing performance
- [ ] Optimize memory access patterns for cache efficiency
- [ ] Minimize real-time allocations and syscalls
- [ ] Benchmark against various buffer sizes and sample rates
- [ ] Stress test with maximum stimulus load

#### 4.2 Stability and Robustness
- [ ] Extensive MIDI edge case testing
- [ ] Plugin state save/restore functionality
- [ ] Error recovery and graceful degradation
- [ ] Memory safety validation
- [ ] Cross-platform compatibility testing

#### 4.3 User Experience Polish
- [ ] Preset system for common configurations
- [ ] Comprehensive user documentation
- [ ] Example configurations for different body layouts
- [ ] Performance tuning guidelines
- [ ] Troubleshooting guide for common issues

### Phase 5: Research Integration (1-2 weeks)

#### 5.1 Research Tools
- [ ] Stimulus logging and analysis tools
- [ ] Session recording and playback capabilities
- [ ] Automated stimulus pattern generation
- [ ] Integration with external analysis software
- [ ] Data export formats for research use

#### 5.2 Advanced Features
- [ ] Audio input analysis for reactive stimulus generation
- [ ] Biometric input integration (heart rate, breathing patterns)
- [ ] Machine learning integration for adaptive stimulus patterns
- [ ] Network synchronization for multi-device setups

## Technical Dependencies

### Core Dependencies
- `vst` - VST plugin framework
- `serde` + `toml`/`json` - Configuration serialization
- `log` - Logging framework
- `egui` or `iced` - GUI framework

### Audio Processing
- `dasp` or `fundsp` - DSP utilities
- `rustfft` - FFT operations (if needed for advanced analysis)
- `hound` - WAV file I/O (for recording/playback features)

### Platform Specific
- `core-audio-sys` (macOS) - Low-level audio interface
- `winapi` (Windows) - Windows-specific VST hosting
- `x11` (Linux) - X11 GUI integration

## Risk Assessment and Mitigation

### High Risk Items
1. **Real-time performance constraints**
   - Mitigation: Early performance profiling, conservative resource allocation
2. **VST3 compliance and DAW compatibility**
   - Mitigation: Test with multiple DAWs early, follow VST3 specification strictly
3. **GUI framework real-time integration**
   - Mitigation: Prototype GUI update mechanisms early, consider alternative frameworks

### Medium Risk Items
1. **MPE implementation complexity**
   - Mitigation: Start with basic MPE, iterate toward full specification
2. **Cross-platform configuration differences**
   - Mitigation: Abstract platform-specific code, extensive testing
3. **Delay line memory and performance**
   - Mitigation: Profile memory usage, optimize for cache efficiency

## Success Metrics

### Functional Requirements
- [ ] Stable operation in major DAWs (Logic Pro, Ableton Live, Reaper)
- [ ] Sub-10ms total latency from MIDI input to audio output
- [ ] Support for 32 simultaneous transducers at 48kHz/96kHz
- [ ] Reliable MPE expression tracking across all parameters

### Performance Requirements
- [ ] <5% CPU usage on target hardware for typical load
- [ ] 120Hz GUI update rate without audio dropouts
- [ ] Memory usage under 100MB total allocation
- [ ] Zero real-time heap allocations during audio processing

### User Experience Requirements
- [ ] Intuitive visual feedback for all stimulus activity
- [ ] Configuration changes apply without audio interruption
- [ ] Clear documentation for research applications
- [ ] Stable operation for extended research sessions (hours)

## Timeline Summary

**Total Estimated Duration**: 10-15 weeks

- **Phase 1**: VST Integration (2-3 weeks)
- **Phase 2**: GUI Development (3-4 weeks)
- **Phase 3**: Stimulus Enhancement (2-3 weeks)
- **Phase 4**: Testing and Optimization (2-3 weeks)
- **Phase 5**: Research Integration (1-2 weeks)

The modular design allows for parallel development of certain components and provides clear milestones for evaluating progress. Early phases focus on establishing a working foundation, while later phases add sophistication and polish for research applications.
