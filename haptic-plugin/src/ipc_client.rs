use std::os::unix::net::UnixStream;
use std::io::Write;
use crossbeam_channel::{Sender, Receiver, bounded};
use std::thread;
use haptic_protocol::{HapticCommand, SOCKET_PATH};
use nih_plug::prelude::nih_log;

pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    _worker_handle: thread::JoinHandle<()>,
}

impl IpcClient {
    pub fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        nih_log!("Attempting to connect to haptic server at {}", SOCKET_PATH);
        
        let stream = match UnixStream::connect(SOCKET_PATH) {
            Ok(s) => {
                nih_log!("Successfully connected to Unix socket");
                s
            }
            Err(e) => {
                nih_log!("Failed to connect to Unix socket: {}", e);
                return Err(Box::new(e));
            }
        };
        
        stream.set_nonblocking(false)?; // Use blocking mode for simplicity
        nih_log!("Socket configured for blocking mode");
        
        let (tx, rx) = bounded(256);
        nih_log!("Created command channel with capacity 256");
        
        let handle = thread::spawn(move || {
            nih_log!("Starting IPC worker thread");
            ipc_worker(stream, rx);
        });
        
        nih_log!("IPC client initialized successfully");
        Ok(Self {
            command_tx: tx,
            _worker_handle: handle,
        })
    }
    
    pub fn send_command(&self, cmd: HapticCommand) -> Result<(), crossbeam_channel::TrySendError<HapticCommand>> {
        // Non-blocking send, drops if queue full
        self.command_tx.try_send(cmd)
    }
    
    pub fn is_connected(&self) -> bool {
        !self.command_tx.is_full() // Simple heuristic
    }
}

fn ipc_worker(mut stream: UnixStream, commands: Receiver<HapticCommand>) {
    let mut write_buffer = Vec::with_capacity(1024);
    nih_log!("IPC worker thread started, buffer capacity: 1024 bytes");
    
    while let Ok(cmd) = commands.recv() {
        write_buffer.clear();
        
        match bincode::serialize_into(&mut write_buffer, &cmd) {
            Ok(_) => {
                // Only log occasionally to avoid spam
                if write_buffer.len() > 0 {
                    // Successfully serialized, try to send
                }
                
                if let Err(e) = stream.write_all(&write_buffer) {
                    nih_log!("IPC write error: {}", e);
                    break;
                }
            }
            Err(e) => {
                nih_log!("Failed to serialize command: {}", e);
            }
        }
    }
    
    nih_log!("IPC worker thread stopped");
}