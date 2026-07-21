use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use haptic_protocol::{encode_frame, ClientRole, HapticCommand, InstanceConfig, SOCKET_PATH};
use nih_plug::prelude::nih_log;
use parking_lot::Mutex;

/// Recent-activity snapshot shared with the editor for on-screen diagnostics.
/// The audio thread writes to it (MIDI events, send failures); the connection
/// manager writes connect/disconnect transitions; the editor reads it.
#[derive(Default)]
pub struct Diagnostics {
    pub instance_id: u64,
    /// Increments on every successful (re)connect — a reconnect counter.
    pub connect_generation: u64,
    pub notes_on: u64,
    pub notes_off: u64,
    pub mpe_updates: u64,
    /// Commands dropped because the outgoing queue was full (server down or
    /// slow); a quick signal that notes are not reaching the server.
    pub sends_dropped: u64,
    /// Recent human-readable events, newest last (capped).
    pub events: VecDeque<String>,
}

impl Diagnostics {
    pub fn log(&mut self, line: String) {
        self.events.push_back(line);
        while self.events.len() > 16 {
            self.events.pop_front();
        }
    }
}

/// Pure write-side, reconnecting client. It only sends to the server; the
/// server (told our Controller role) streams us no status, so there is no
/// reader. A background manager thread keeps the connection up, re-sending the
/// `Hello` handshake on every (re)connect, so the plugin survives server
/// restarts without being reloaded.
pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    connected: Arc<AtomicBool>,
    /// Current note-type config, re-sent in `Hello` on each (re)connect so the
    /// server always has this instance's live configuration after a restart.
    config: Arc<Mutex<InstanceConfig>>,
}

impl IpcClient {
    /// Spawn the connection manager. Infallible: if the server is down the
    /// manager simply keeps retrying in the background.
    pub fn spawn(
        instance_id: u64,
        initial_config: InstanceConfig,
        diag: Arc<Mutex<Diagnostics>>,
    ) -> Self {
        let (tx, rx) = bounded(256);
        let connected = Arc::new(AtomicBool::new(false));
        let config = Arc::new(Mutex::new(initial_config));
        {
            let connected = connected.clone();
            let config = config.clone();
            thread::spawn(move || connection_manager(instance_id, rx, connected, config, diag));
        }
        nih_log!("IPC client manager spawned for instance {}", instance_id);
        Self { command_tx: tx, connected, config }
    }

    /// Non-blocking send; drops (and reports `Err`) if the queue is full.
    pub fn send_command(
        &self,
        cmd: HapticCommand,
    ) -> Result<(), crossbeam_channel::TrySendError<HapticCommand>> {
        self.command_tx.try_send(cmd)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Update the config re-sent on the next (re)connect. Called when the
    /// host changes a parameter, alongside the live `SetParameter` send.
    pub fn set_config(&self, cfg: InstanceConfig) {
        *self.config.lock() = cfg;
    }
}

fn connection_manager(
    instance_id: u64,
    rx: Receiver<HapticCommand>,
    connected: Arc<AtomicBool>,
    config: Arc<Mutex<InstanceConfig>>,
    diag: Arc<Mutex<Diagnostics>>,
) {
    let mut frame = Vec::with_capacity(256);
    loop {
        let mut stream = match UnixStream::connect(SOCKET_PATH) {
            Ok(s) => s,
            Err(_) => {
                // Back off before retrying; recv_timeout doubles as teardown
                // detection — if every Sender has dropped, the plugin is gone.
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Err(RecvTimeoutError::Disconnected) => return,
                    _ => continue, // timed out, or a command we drop while offline
                }
            }
        };
        // Drop any commands queued while disconnected so stale notes don't
        // fire on reconnect.
        while rx.try_recv().is_ok() {}
        // Handshake with the live config.
        let hello = HapticCommand::Hello {
            instance_id,
            role: ClientRole::Controller,
            config: *config.lock(),
        };
        if encode_frame(&hello, &mut frame).is_err() || stream.write_all(&frame).is_err() {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        connected.store(true, Ordering::Relaxed);
        {
            let mut d = diag.lock();
            d.connect_generation += 1;
            let g = d.connect_generation;
            d.log(format!("connected (#{g})"));
        }

        // Write commands until the socket fails, then fall through to reconnect.
        loop {
            match rx.recv() {
                Ok(cmd) => {
                    if encode_frame(&cmd, &mut frame).is_ok()
                        && stream.write_all(&frame).is_err()
                    {
                        break;
                    }
                }
                // Sender dropped: plugin is being torn down.
                Err(_) => {
                    connected.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
        diag.lock().log("disconnected — reconnecting".into());
    }
}
