# Rust Build and Export Targets Guide

This document explains all the build targets, export formats, and compilation options available in this haptic VST project for newcomers to Rust.

## Project Structure Overview

This is a **Rust workspace** containing multiple related packages:

```
haptic-vst/
├── Cargo.toml                    # Workspace configuration
├── haptic-protocol/              # Shared communication protocol
├── haptic-server/                # Audio server application
├── haptic-plugin/                # Main VST plugin library
├── haptic-plugin-standalone/     # Standalone audio application
└── xtask/                        # Build automation tools
```

## Understanding Rust Crate Types

### Library Crate Types (`crate-type`)

The haptic plugin uses multiple crate types to support different use cases:

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

- **`cdylib`** (C Dynamic Library): Creates a dynamic library (.dylib on macOS, .dll on Windows, .so on Linux) that can be loaded by DAW host applications as VST3/CLAP plugins
- **`rlib`** (Rust Library): Creates a Rust-native library format that can be imported by other Rust crates (used by the standalone package)

## 1. VST3 and CLAP Plugin Targets

### What They Are
- **VST3**: Steinberg's Virtual Studio Technology 3.0 plugin format
- **CLAP**: CLever Audio Plugin format (modern open-source alternative)
- Both are **audio plugin formats** that run inside Digital Audio Workstations (DAWs)

### Build Commands
```bash
# Build both VST3 and CLAP plugins (development)
cargo xtask bundle haptic-plugin

# Build optimized release versions
cargo xtask bundle haptic-plugin --release

# Build universal binary for macOS (Intel + Apple Silicon)
cargo xtask bundle-universal haptic-plugin --release
```

### Output Location
```
target/bundled/
├── haptic-plugin.vst3    # VST3 plugin bundle
└── haptic-plugin.clap    # CLAP plugin bundle
```

### Technical Details
- **Export Macros**: `nih_export_vst3!()` and `nih_export_clap!()`
- **Host Integration**: Plugins run inside DAW processes
- **GUI Support**: Full egui-based graphical interface
- **Platform**: macOS, Windows, Linux

### Installation
```bash
# macOS VST3
cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/

# macOS CLAP  
cp -r target/bundled/haptic-plugin.clap ~/Library/Audio/Plug-Ins/CLAP/
```

## 2. Standalone Application Target

### What It Is
A **standalone audio application** that runs independently without requiring a DAW host. Uses the same plugin code but with a built-in audio interface.

### Build Commands
```bash
# Build standalone application (development)
cargo build --package haptic-plugin-standalone

# Build optimized release version
cargo build --package haptic-plugin-standalone --release

# Run directly
cargo run --package haptic-plugin-standalone
```

### Output Location
```
target/debug/haptic-plugin-standalone      # Debug build
target/release/haptic-plugin-standalone    # Release build
```

### Technical Details
- **Export Function**: `nih_export_standalone::<HapticPlugin>()`
- **Audio Backend**: Automatically detects system audio (CoreAudio on macOS, WASAPI on Windows, ALSA on Linux)
- **GUI Behavior**: 
  - **macOS**: Disabled (headless) to avoid baseview crashes
  - **Other platforms**: Full GUI support
- **Independence**: Self-contained application, no DAW required

### Platform-Specific Behavior
```rust
#[cfg(all(target_os = "macos", feature = "standalone"))]
{
    // GUI disabled on macOS standalone
    None
}
#[cfg(not(all(target_os = "macos", feature = "standalone")))]
{
    // Full GUI on other platforms
    editor::create(params, ipc_client)
}
```

## 3. Server Application Target

### What It Is
The **haptic audio server** that processes spatial audio and controls 32-channel haptic transducer arrays.

### Build Commands
```bash
# Build server (development)
cargo build --bin haptic-server

# Build optimized release version
cargo build --bin haptic-server --release

# Run server
cargo run --bin haptic-server
```

### Technical Details
- **Purpose**: Real-time audio processing and haptic device control
- **Communication**: Unix domain socket (`/tmp/haptic-vst.sock`)
- **Architecture**: Multi-threaded with lock-free audio processing
- **Protocol**: Custom binary protocol via `haptic-protocol` crate

## 4. Rust Feature Flags

### Understanding Features
Rust **features** are conditional compilation flags that enable/disable code sections:

```toml
[features]
default = []
standalone = ["nih_plug/standalone"]
```

### Available Features
- **`standalone`**: Enables standalone application support
- **`vst3`**: Enables VST3 plugin format (always enabled)

### Using Features
```bash
# Build with specific features
cargo build --features standalone

# Build without default features
cargo build --no-default-features

# Build with multiple features
cargo build --features "standalone,vst3"
```

## 5. Build Profiles (Debug vs Release)

### Debug Profile (Default)
```bash
cargo build
```
- **Optimization**: None (`opt-level = 0`)
- **Debug Info**: Full
- **Compile Time**: Fast
- **Runtime**: Slow, large binary
- **Use Case**: Development, debugging

### Release Profile
```bash
cargo build --release
```
- **Optimization**: Maximum (`opt-level = 3`)
- **Debug Info**: Minimal
- **Compile Time**: Slow
- **Runtime**: Fast, small binary
- **Use Case**: Production, distribution

## 6. Target Architecture Options

### Universal Binaries (macOS)
```bash
# Build for both Intel and Apple Silicon
cargo xtask bundle-universal haptic-plugin --release
```

### Specific Targets
```bash
# List available targets
rustup target list

# Add specific target
rustup target add x86_64-apple-darwin

# Build for specific target
cargo build --target x86_64-apple-darwin
```

## 7. Workspace vs Package Builds

### Workspace-Level Commands
```bash
# Build entire workspace
cargo build

# Build specific package
cargo build --package haptic-plugin

# Run specific binary
cargo run --bin haptic-server
```

### Package-Level Commands
```bash
# Change to package directory
cd haptic-plugin/

# Build current package only
cargo build

# Run if package has binary
cargo run
```

## 8. Development Workflow

### 1. Server Development
```bash
# Terminal 1: Run server with live reload
cargo watch -x "run --bin haptic-server"

# Terminal 2: Test server
telnet /tmp/haptic-vst.sock
```

### 2. Plugin Development
```bash
# Build and install plugin
cargo xtask bundle haptic-plugin --release
cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/

# Check logs
tail -f ~/tmp/log/haptic-vst.log
```

### 3. Standalone Development
```bash
# Run with live reload
cargo watch -x "run --package haptic-plugin-standalone"
```

## 9. Dependencies and External Tools

### Rust Tools Required
```bash
# Core Rust toolchain
rustup update

# Code formatting
rustup component add rustfmt

# Linting
rustup component add clippy

# Watch for file changes
cargo install cargo-watch
```

### System Dependencies (macOS)
- **Xcode Command Line Tools**: For system libraries
- **CoreAudio**: Audio framework (system provided)
- **Jack**: Optional audio server (`brew install jack`)

## 10. Common Build Issues and Solutions

### Issue: "Package does not contain feature"
```bash
# Wrong
cargo build --features standalone

# Correct
cargo build --package haptic-plugin --features standalone
```

### Issue: Library import errors
```bash
# Ensure rlib crate type is included
crate-type = ["cdylib", "rlib"]
```

### Issue: macOS signing
```bash
# xtask handles code signing automatically
cargo xtask bundle haptic-plugin --release
```

### Issue: Missing audio backends
```bash
# Install Jack (optional)
brew install jack

# Check audio devices
cargo run --bin haptic-server
```

## 11. Understanding the Build Process

### 1. Compilation Phases
1. **Dependency Resolution**: Cargo downloads and compiles dependencies
2. **Macro Expansion**: Rust macros like `nih_export_vst3!()` generate code
3. **Type Checking**: Rust's borrow checker validates memory safety
4. **Optimization**: LLVM optimizes the compiled code
5. **Linking**: Creates final binary/library files

### 2. Cross-Package Dependencies
```
haptic-plugin-standalone → haptic-plugin → haptic-protocol
haptic-server → haptic-protocol
```

### 3. Build Cache
- **Location**: `target/` directory
- **Incremental**: Only rebuilds changed code
- **Clean**: `cargo clean` removes all build artifacts

## 12. Production Deployment

### Plugin Distribution
```bash
# Build release versions
cargo xtask bundle haptic-plugin --release

# Create installer package (manual)
# Distribute target/bundled/ contents
```

### Server Deployment
```bash
# Build optimized server
cargo build --bin haptic-server --release

# Binary location
target/release/haptic-server
```

## 13. Debugging and Diagnostics

### Build Diagnostics
```bash
# Verbose build output
cargo build -v

# Check what features are enabled
cargo build --features standalone -v | grep "feature"

# Show build timings
cargo build --timings
```

### Runtime Diagnostics
```bash
# Set log level
export NIH_LOG=~/tmp/log/haptic-vst.log

# Run with debug info
cargo run --bin haptic-server
```

This guide covers all the essential build targets and processes for the haptic VST project. Each target serves a specific purpose in the audio plugin ecosystem, from development to production deployment.