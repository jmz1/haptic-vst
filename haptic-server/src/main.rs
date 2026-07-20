use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

mod engine;
mod audio;
mod ipc;

use engine::StimulusEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Starting Haptic VST Server");

    let test_tone = std::env::args().any(|arg| arg == "--test-tone");

    // Create shared shutdown flag
    let running = Arc::new(AtomicBool::new(true));

    // Create stimulus engine - the IPC thread gets the command producer
    let (engine, command_producer) = StimulusEngine::new();

    // Levels path: audio callback → IPC thread → connected clients
    let (levels_producer, levels_consumer) = rtrb::RingBuffer::new(256);

    // Start IPC listener thread
    let ipc_handle = {
        let running = running.clone();
        thread::spawn(move || {
            if let Err(e) = ipc::listen_loop(running, command_producer, levels_consumer) {
                eprintln!("IPC error: {}", e);
            }
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
    
    eprintln!("Haptic VST Server stopped");
    Ok(())
}