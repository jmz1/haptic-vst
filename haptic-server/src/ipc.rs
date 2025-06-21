use std::os::unix::net::{UnixListener, UnixStream};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use haptic_protocol::{HapticCommand, SOCKET_PATH};

pub fn listen_loop(
    running: Arc<AtomicBool>,
    command_producer: crossbeam_channel::Sender<crate::engine::EngineCommand>
) -> Result<(), Box<dyn std::error::Error>> {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(SOCKET_PATH);
    
    let listener = UnixListener::bind(SOCKET_PATH)?;
    listener.set_nonblocking(true)?;
    
    eprintln!("IPC server listening on {}", SOCKET_PATH);
    
    let mut clients = Vec::new();
    
    while running.load(Ordering::Relaxed) {
        // Accept new connections
        match listener.accept() {
            Ok((stream, _)) => {
                eprintln!("New client connected");
                if let Err(e) = stream.set_nonblocking(true) {
                    eprintln!("Failed to set stream nonblocking: {}", e);
                    continue;
                }
                clients.push(stream);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No new connections, continue
            }
            Err(e) => {
                eprintln!("Error accepting connection: {}", e);
            }
        }
        
        // Handle existing clients
        clients.retain_mut(|client| {
            handle_client(client, &command_producer)
        });
        
        thread::sleep(Duration::from_millis(1));
    }
    
    // Cleanup
    let _ = std::fs::remove_file(SOCKET_PATH);
    eprintln!("IPC server stopped");
    
    Ok(())
}

fn handle_client(stream: &mut UnixStream, command_producer: &crossbeam_channel::Sender<crate::engine::EngineCommand>) -> bool {
    let mut buffer = [0u8; 1024];
    
    match stream.read(&mut buffer) {
        Ok(0) => {
            // Client disconnected
            eprintln!("Client disconnected");
            false
        }
        Ok(n) => {
            // Try to deserialize command
            match bincode::deserialize::<HapticCommand>(&buffer[..n]) {
                Ok(command) => {
                    // Convert to engine command and send
                    let engine_cmd = match command {
                        HapticCommand::NoteOn { note, velocity, channel, mpe, .. } => {
                            crate::engine::EngineCommand::NoteOn { note, velocity, channel, mpe }
                        }
                        HapticCommand::NoteOff { note, channel, .. } => {
                            crate::engine::EngineCommand::NoteOff { note, channel }
                        }
                        HapticCommand::MpeUpdate { channel, mpe, .. } => {
                            crate::engine::EngineCommand::MpeUpdate { channel, mpe }
                        }
                        HapticCommand::Panic => crate::engine::EngineCommand::Panic,
                        HapticCommand::SetWaveSpeed(_) => {
                            // TODO: Handle wave speed updates
                            return true;
                        }
                    };
                    
                    let _ = command_producer.send(engine_cmd);
                    true
                }
                Err(e) => {
                    eprintln!("Failed to deserialize command: {}", e);
                    true // Keep connection alive
                }
            }
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // No data available, keep connection
            true
        }
        Err(e) => {
            eprintln!("Error reading from client: {}", e);
            false
        }
    }
}