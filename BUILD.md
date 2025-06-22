# Building the Haptic VST Plugin

This project uses `nih_plug_xtask` for easy plugin building and bundling.

## Quick Start

### Build Development Plugin
```bash
cargo xtask bundle haptic-plugin
```

### Build Release Plugin (Optimized)
```bash
cargo xtask bundle haptic-plugin --release
```

### Build Universal Binary (macOS)
```bash
cargo xtask bundle-universal haptic-plugin --release
```

## Output Locations

Built plugins are placed in:
```
target/bundled/
├── haptic-plugin.clap    # CLAP format plugin
└── haptic-plugin.vst3    # VST3 format plugin
```

## Installation

### macOS
Copy the plugin bundles to your DAW's plugin directory:

**VST3:**
```bash
cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/
```

**CLAP:**
```bash
cp -r target/bundled/haptic-plugin.clap ~/Library/Audio/Plug-Ins/CLAP/
```

### System Directories (macOS)
- VST3: `/Library/Audio/Plug-Ins/VST3/` (system-wide)
- CLAP: `/Library/Audio/Plug-Ins/CLAP/` (system-wide)
- User: `~/Library/Audio/Plug-Ins/VST3/` or `~/Library/Audio/Plug-Ins/CLAP/`

## Running the Server

The plugin requires the haptic server to be running:

```bash
cargo run --bin haptic-server
```

## Development Workflow

1. **Start the server:**
   ```bash
   cargo run --bin haptic-server
   ```

2. **Build the plugin:**
   ```bash
   cargo xtask bundle haptic-plugin --release
   ```

3. **Install the plugin:**
   ```bash
   cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/
   ```

4. **Load in your DAW** and send MIDI notes to test haptic feedback

## Features

- **VST3 & CLAP Support**: Works with most modern DAWs
- **Real-time Processing**: Zero-allocation audio processing
- **MIDI/MPE Input**: Full support for expressive MIDI controllers
- **Velocity-based Wave Speed**: Note velocity controls wave propagation (20-500 m/s)
- **32-channel Output**: Direct control of haptic transducer arrays
- **Enhanced Logging**: Comprehensive debug logging to `/tmp/haptic-vst.log`
- **Cross-platform**: macOS and Linux support

## Standalone Mode

Note: Standalone mode is currently not supported. The plugin requires a DAW host to function.

## Supported DAWs

Tested with:
- **Logic Pro** (VST3/CLAP)
- **Ableton Live** (VST3/CLAP) 
- **Cubase** (VST3)
- **FL Studio** (VST3)
- **Reaper** (VST3/CLAP)

## Troubleshooting

### Plugin Not Found
- Ensure the plugin is in the correct directory
- Restart your DAW after installation
- Check DAW plugin scanner settings

### No Haptic Output
- Verify haptic-server is running: `cargo run --bin haptic-server`
- Check server logs for connection status
- Ensure 32-channel audio interface is connected

### Build Issues
- Update Rust: `rustup update`
- Clean build: `cargo clean`
- For universal binaries: `rustup target add x86_64-apple-darwin`