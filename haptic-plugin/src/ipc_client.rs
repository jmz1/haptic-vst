use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crossbeam_channel::{Sender, Receiver, bounded};
use std::thread;
use parking_lot::Mutex;
use haptic_protocol::{encode_frame, FrameDecoder, HapticCommand, ServerStatus, SOCKET_PATH};
use nih_plug::prelude::nih_log;

pub const TRANSDUCER_COUNT: usize = 32;

pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    connected: Arc<AtomicBool>,
    levels: Arc<Mutex<[f32; TRANSDUCER_COUNT]>>,
    _writer_handle: thread::JoinHandle<()>,
    _reader_handle: thread::JoinHandle<()>,
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

        stream.set_nonblocking(false)?; // Worker threads may block

        // Bounded queue: sends from the audio thread never block, they drop
        let (tx, rx) = bounded(256);
        let connected = Arc::new(AtomicBool::new(true));
        let levels = Arc::new(Mutex::new([0.0f32; TRANSDUCER_COUNT]));

        let read_stream = stream.try_clone()?;

        let writer_handle = {
            let connected = connected.clone();
            thread::spawn(move || {
                ipc_writer(stream, rx);
                connected.store(false, Ordering::Relaxed);
            })
        };

        let reader_handle = {
            let connected = connected.clone();
            let levels = levels.clone();
            thread::spawn(move || {
                ipc_reader(read_stream, &levels, &connected);
                connected.store(false, Ordering::Relaxed);
            })
        };

        nih_log!("IPC client initialized successfully");
        Ok(Self {
            command_tx: tx,
            connected,
            levels,
            _writer_handle: writer_handle,
            _reader_handle: reader_handle,
        })
    }

    pub fn send_command(&self, cmd: HapticCommand) -> Result<(), crossbeam_channel::TrySendError<HapticCommand>> {
        // Non-blocking send, drops if queue full
        self.command_tx.try_send(cmd)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Latest per-transducer RMS levels broadcast by the server.
    pub fn levels(&self) -> [f32; TRANSDUCER_COUNT] {
        *self.levels.lock()
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

fn ipc_reader(
    mut stream: UnixStream,
    levels: &Mutex<[f32; TRANSDUCER_COUNT]>,
    connected: &AtomicBool,
) {
    let mut buffer = [0u8; 4096];
    let mut decoder = FrameDecoder::new();
    nih_log!("IPC reader thread started");

    while connected.load(Ordering::Relaxed) {
        match stream.read(&mut buffer) {
            Ok(0) => {
                nih_log!("Server closed the connection");
                break;
            }
            Ok(n) => {
                decoder.extend(&buffer[..n]);
                loop {
                    match decoder.next_frame::<ServerStatus>() {
                        Ok(Some(ServerStatus::TransducerLevels { levels: new_levels, .. })) => {
                            *levels.lock() = new_levels;
                        }
                        Ok(Some(_)) => {
                            // Other status messages not yet consumed
                        }
                        Ok(None) => break,
                        Err(e) => {
                            nih_log!("Status stream error, disconnecting: {}", e);
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                nih_log!("IPC read error, disconnecting: {}", e);
                break;
            }
        }
    }

    nih_log!("IPC reader thread stopped");
}
