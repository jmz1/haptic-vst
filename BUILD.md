# Building the Haptic VST Plugin

This project uses `nih_plug_xtask` for easy plugin building and bundling.

> **PATH note:** `cargo` lives in `~/.cargo/bin`, which is not on the
> default shell PATH on this machine. Run
> `echo 'source "$HOME/.cargo/env"' >> ~/.zshrc` once (new terminal after),
> or prefix sessions with `export PATH="$HOME/.cargo/bin:$PATH"`.

> **Testing:** build instructions are here; how to *run and test* the
> system (server, viewer, plugin, scripted clients) is in `TESTING.md`.

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
└── haptic-plugin.vst3    # VST3 format plugin
```

(CLAP export is currently disabled in `haptic-plugin/src/lib.rs`; a stale
`haptic-plugin.clap` may remain in `target/bundled/` from older builds.)

## Installation

### macOS
Copy the plugin bundle to your DAW's plugin directory:

```bash
mkdir -p ~/Library/Audio/Plug-Ins/VST3
cp -r target/bundled/haptic-plugin.vst3 ~/Library/Audio/Plug-Ins/VST3/
```

### System Directories (macOS)
- User: `~/Library/Audio/Plug-Ins/VST3/`
- System-wide: `/Library/Audio/Plug-Ins/VST3/`

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

5. **Optionally start the visualiser** (`cargo run -p haptic-viewer
   --release`) — see `TESTING.md` for the full testing workflow

## Features

- **VST3 Support**: Works with most modern DAWs (CLAP currently disabled)
- **Real-time Processing**: Lock-free audio path in the server
- **MIDI/MPE Input**: Velocity → amplitude, bend/pressure/slide → source
  position and intensity
- **DAW-automatable parameters**: Wave Speed (20–500 m/s) and Stimulus Type
- **32-channel Output**: Direct control of haptic transducer arrays
- **Logging**: Plugin log at `/Users/jmz/tmp/log/haptic-vst.log`
  (override with `NIH_LOG`)
- **Cross-platform**: macOS and Linux support

## Standalone Mode

### Build Standalone Binary
```bash
cargo build --package haptic-plugin-standalone --release
```

### Run Standalone
```bash
cargo run --package haptic-plugin-standalone
```

**Note**: The standalone version successfully initializes and connects to the haptic server, but may have GUI stability issues on macOS due to baseview library limitations. For production use, the VST3/CLAP versions in a DAW host are recommended.

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