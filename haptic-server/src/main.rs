use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

mod config;
mod engine;
mod audio;
mod ipc;

use config::TransducerLayout;
use engine::StimulusEngine;

const DEFAULT_CONFIG_PATH: &str = "haptic.toml";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Starting Haptic VST Server");

    let args: Vec<String> = std::env::args().collect();
    let test_tone = args.iter().any(|arg| arg == "--test-tone");
    let config_path = args
        .iter()
        .position(|arg| arg == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));

    // Load the transducer layout: a missing file falls back to the built-in
    // default (4x8 grid over 1m x 2m); a present-but-invalid file is a hard
    // error so a typo can't silently drive the wrong layout.
    let layout = if config_path.exists() {
        match config::load_layout(&config_path) {
            Ok(layout) => {
                eprintln!("Loaded transducer layout from {}", config_path.display());
                layout
            }
            Err(e) => {
                eprintln!("Invalid config {}: {}", config_path.display(), e);
                return Err(e.into());
            }
        }
    } else {
        eprintln!(
            "No config at {}, using default layout (4x8 grid over 1m x 2m)",
            config_path.display()
        );
        TransducerLayout::default()
    };

    // Create shared shutdown flag
    let running = Arc::new(AtomicBool::new(true));

    // Create stimulus engine - the IPC thread gets the command producer and
    // voice-snapshot consumer, the config watcher gets the layout producer
    let (engine, command_producer, engine_layout_producer, voice_consumer) =
        StimulusEngine::new(layout);

    // Levels path: audio callback → IPC thread → connected clients
    let (levels_producer, levels_consumer) = rtrb::RingBuffer::new(256);

    // Layout path to the IPC thread (for broadcast to clients); the engine
    // has its own ring, so the watcher feeds both
    let (ipc_layout_producer, ipc_layout_consumer) = rtrb::RingBuffer::new(4);

    // Start IPC listener thread
    let ipc_handle = {
        let running = running.clone();
        thread::spawn(move || {
            if let Err(e) = ipc::listen_loop(
                running,
                command_producer,
                levels_consumer,
                voice_consumer,
                layout,
                ipc_layout_consumer,
            ) {
                eprintln!("IPC error: {}", e);
            }
        })
    };

    // Config watcher: hot-reload the layout when the file's mtime changes
    let watcher_handle = {
        let running = running.clone();
        thread::spawn(move || {
            config_watcher(running, config_path, engine_layout_producer, ipc_layout_producer)
        })
    };

    // Set up signal handler for graceful shutdown
    let running_for_signal = running.clone();
    ctrlc::set_handler(move || {
        eprintln!("Received interrupt signal, shutting down...");
        running_for_signal.store(false, Ordering::Relaxed);
    })?;

    // Run audio loop on main thread (highest priority)
    if let Err(e) = audio::run_audio_loop(engine, running.clone(), test_tone, levels_producer) {
        eprintln!("Audio error: {}", e);
    }

    // Cleanup
    running.store(false, Ordering::Relaxed);
    ipc_handle.join().ok();
    watcher_handle.join().ok();

    eprintln!("Haptic VST Server stopped");
    Ok(())
}

/// Poll the config file's mtime (~1 Hz); on change, parse it off the audio
/// thread and push the new layout into the engine's layout ring. Parse
/// errors leave the current layout running.
fn config_watcher(
    running: Arc<AtomicBool>,
    path: PathBuf,
    mut engine_producer: rtrb::Producer<Box<TransducerLayout>>,
    mut ipc_producer: rtrb::Producer<TransducerLayout>,
) {
    let mtime_of = |path: &std::path::Path| -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    };

    let mut last_mtime = mtime_of(&path);
    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(1000));
        let mtime = mtime_of(&path);
        if mtime.is_some() && mtime != last_mtime {
            last_mtime = mtime;
            match config::load_layout(&path) {
                Ok(layout) => {
                    if engine_producer.push(Box::new(layout)).is_ok() {
                        eprintln!("Config reloaded from {}", path.display());
                    }
                    let _ = ipc_producer.push(layout);
                }
                Err(e) => {
                    eprintln!("Config reload failed (keeping current layout): {}", e);
                }
            }
        }
    }
}
