use std::os::unix::net::UnixStream;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crossbeam_channel::{Sender, Receiver, bounded};
use std::thread;
use haptic_protocol::{encode_frame, HapticCommand, SOCKET_PATH};
use nih_plug::prelude::nih_log;

/// The plugin is a pure controller: it only *sends* to the server (handshake,
/// notes, config) and never consumes the status stream — the server, told via
/// the `Hello` role, sends it none, so there is no reader thread to keep the
/// socket drained. Whole-system visualisation is the viewer's job now.
pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    connected: Arc<AtomicBool>,
    _writer_handle: thread::JoinHandle<()>,
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

        stream.set_nonblocking(false)?; // Writer thread may block

        // Bounded queue: sends from the audio thread never block, they drop
        let (tx, rx) = bounded(256);
        let connected = Arc::new(AtomicBool::new(true));

        let writer_handle = {
            let connected = connected.clone();
            thread::spawn(move || {
                ipc_writer(stream, rx);
                connected.store(false, Ordering::Relaxed);
            })
        };

        nih_log!("IPC client initialized successfully");
        Ok(Self {
            command_tx: tx,
            connected,
            _writer_handle: writer_handle,
        })
    }

    pub fn send_command(&self, cmd: HapticCommand) -> Result<(), crossbeam_channel::TrySendError<HapticCommand>> {
        // Non-blocking send, drops if queue full
        self.command_tx.try_send(cmd)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

fn ipc_writer(mut stream: UnixStream, commands: Receiver<HapticCommand>) {
    let mut write_buffer = Vec::with_capacity(1024);
    nih_log!("IPC writer thread started");

    while let Ok(cmd) = commands.recv() {
        match encode_frame(&cmd, &mut write_buffer) {
            Ok(()) => {
                if let Err(e) = stream.write_all(&write_buffer) {
                    nih_log!("IPC write error, disconnecting: {}", e);
                    break;
                }
            }
            Err(e) => {
                nih_log!("Failed to serialize command: {}", e);
            }
        }
    }

    nih_log!("IPC writer thread stopped");
}
