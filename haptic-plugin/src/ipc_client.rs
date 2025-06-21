use std::os::unix::net::UnixStream;
use std::io::Write;
use crossbeam_channel::{Sender, Receiver, bounded};
use std::thread;
use haptic_protocol::{HapticCommand, SOCKET_PATH};

pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    _worker_handle: thread::JoinHandle<()>,
}

impl IpcClient {
    pub fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let stream = UnixStream::connect(SOCKET_PATH)?;
        stream.set_nonblocking(false)?; // Use blocking mode for simplicity
        
        let (tx, rx) = bounded(256);
        
        let handle = thread::spawn(move || {
            ipc_worker(stream, rx);
        });
        
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
    
    while let Ok(cmd) = commands.recv() {
        write_buffer.clear();
        if let Ok(_) = bincode::serialize_into(&mut write_buffer, &cmd) {
            if let Err(e) = stream.write_all(&write_buffer) {
                eprintln!("IPC write error: {}", e);
                break;
            }
        }
    }
    
    eprintln!("IPC worker thread stopped");
}