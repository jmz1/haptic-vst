use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

mod engine;
mod audio;
mod ipc;

use engine::StimulusEngine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Starting Haptic VST Server");
    
    // Create shared shutdown flag
    let running = Arc::new(AtomicBool::new(true));
    
    // Create stimulus engine - the IPC thread will get a handle to send commands
    let engine = StimulusEngine::new();
    let command_producer = engine.get_command_producer();
    
    // Start IPC listener thread
    let ipc_handle = {
        let running = running.clone();
        thread::spawn(move || {
            if let Err(e) = ipc::listen_loop(running, command_producer) {
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
    if let Err(e) = audio::run_audio_loop(engine, running.clone()) {
        eprintln!("Audio error: {}", e);
    }
    
    // Cleanup
    running.store(false, Ordering::Relaxed);
    ipc_handle.join().ok();
    
    eprintln!("Haptic VST Server stopped");
    Ok(())
}