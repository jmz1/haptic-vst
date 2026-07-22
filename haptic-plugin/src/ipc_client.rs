use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use haptic_protocol::{
    encode_frame, ClientRole, FrameDecoder, HapticCommand, InstanceConfig, ServerStatus,
    PROTOCOL_VERSION, SOCKET_PATH,
};
use nih_plug::prelude::nih_log;

/// Recent-activity snapshot shared with the editor for on-screen diagnostics.
/// The audio thread writes to it (MIDI events, send failures); the connection
/// manager writes connect/disconnect transitions; the editor reads it.
pub struct Diagnostics {
    pub instance_id: u64,
    /// Increments on every successful (re)connect — a reconnect counter.
    connect_generation: AtomicU64,
    notes_on: AtomicU64,
    notes_off: AtomicU64,
    mpe_updates: AtomicU64,
    /// Commands dropped because the outgoing queue was full (server down or
    /// slow); a quick signal that notes are not reaching the server.
    sends_dropped: AtomicU64,
}

impl Diagnostics {
    pub fn new(instance_id: u64) -> Self {
        Self {
            instance_id,
            connect_generation: AtomicU64::new(0),
            notes_on: AtomicU64::new(0),
            notes_off: AtomicU64::new(0),
            mpe_updates: AtomicU64::new(0),
            sends_dropped: AtomicU64::new(0),
        }
    }

    pub fn record(&self, notes_on: u64, notes_off: u64, mpe_updates: u64, dropped: u64) {
        self.notes_on.fetch_add(notes_on, Ordering::Relaxed);
        self.notes_off.fetch_add(notes_off, Ordering::Relaxed);
        self.mpe_updates.fetch_add(mpe_updates, Ordering::Relaxed);
        self.sends_dropped.fetch_add(dropped, Ordering::Relaxed);
    }

    pub fn connected(&self) {
        self.connect_generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DiagnosticSnapshot {
        DiagnosticSnapshot {
            connect_generation: self.connect_generation.load(Ordering::Relaxed),
            notes_on: self.notes_on.load(Ordering::Relaxed),
            notes_off: self.notes_off.load(Ordering::Relaxed),
            mpe_updates: self.mpe_updates.load(Ordering::Relaxed),
            sends_dropped: self.sends_dropped.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
pub struct DiagnosticSnapshot {
    pub connect_generation: u64,
    pub notes_on: u64,
    pub notes_off: u64,
    pub mpe_updates: u64,
    pub sends_dropped: u64,
}

struct ConfigSnapshot {
    sequence: AtomicU64,
    stimulus_type: AtomicU32,
    wave_speed: AtomicU32,
    tw_wave_speed: AtomicU32,
    scale_mode: AtomicU32,
    wavelength_m: AtomicU32,
    atten_d0_m: AtomicU32,
    atten_exponent: AtomicU32,
}

impl ConfigSnapshot {
    fn new(config: InstanceConfig) -> Self {
        Self {
            sequence: AtomicU64::new(0),
            stimulus_type: AtomicU32::new(Self::encode_stimulus(config.stimulus_type)),
            wave_speed: AtomicU32::new(config.wave_speed.to_bits()),
            tw_wave_speed: AtomicU32::new(config.travelling_wave.wave_speed.to_bits()),
            scale_mode: AtomicU32::new(Self::encode_scale_mode(config.travelling_wave.scale_mode)),
            wavelength_m: AtomicU32::new(config.travelling_wave.wavelength_m.to_bits()),
            atten_d0_m: AtomicU32::new(config.distance_decay.d0_m.to_bits()),
            atten_exponent: AtomicU32::new(config.distance_decay.exponent.to_bits()),
        }
    }

    fn store(&self, config: InstanceConfig) {
        self.sequence.fetch_add(1, Ordering::AcqRel);
        self.stimulus_type.store(
            Self::encode_stimulus(config.stimulus_type),
            Ordering::Relaxed,
        );
        self.wave_speed
            .store(config.wave_speed.to_bits(), Ordering::Relaxed);
        self.tw_wave_speed.store(
            config.travelling_wave.wave_speed.to_bits(),
            Ordering::Relaxed,
        );
        self.scale_mode.store(
            Self::encode_scale_mode(config.travelling_wave.scale_mode),
            Ordering::Relaxed,
        );
        self.wavelength_m.store(
            config.travelling_wave.wavelength_m.to_bits(),
            Ordering::Relaxed,
        );
        self.atten_d0_m
            .store(config.distance_decay.d0_m.to_bits(), Ordering::Relaxed);
        self.atten_exponent
            .store(config.distance_decay.exponent.to_bits(), Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    fn load(&self) -> InstanceConfig {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let stimulus_type = match self.stimulus_type.load(Ordering::Relaxed) {
                0 => haptic_protocol::StimulusType::Wave,
                _ => haptic_protocol::StimulusType::TravellingWave,
            };
            let wave_speed = f32::from_bits(self.wave_speed.load(Ordering::Relaxed));
            let tw_wave_speed = f32::from_bits(self.tw_wave_speed.load(Ordering::Relaxed));
            let scale_mode = match self.scale_mode.load(Ordering::Relaxed) {
                0 => haptic_protocol::SpatialScaleMode::Speed,
                _ => haptic_protocol::SpatialScaleMode::Wavelength,
            };
            let wavelength_m = f32::from_bits(self.wavelength_m.load(Ordering::Relaxed));
            let atten_d0_m = f32::from_bits(self.atten_d0_m.load(Ordering::Relaxed));
            let atten_exponent = f32::from_bits(self.atten_exponent.load(Ordering::Relaxed));
            if before == self.sequence.load(Ordering::Acquire) {
                return InstanceConfig {
                    stimulus_type,
                    wave_speed,
                    travelling_wave: haptic_protocol::TravellingWaveConfig {
                        scale_mode,
                        wave_speed: tw_wave_speed,
                        wavelength_m,
                    },
                    distance_decay: haptic_protocol::DistanceDecay {
                        d0_m: atten_d0_m,
                        exponent: atten_exponent,
                    },
                };
            }
        }
    }

    fn encode_stimulus(stimulus_type: haptic_protocol::StimulusType) -> u32 {
        match stimulus_type {
            haptic_protocol::StimulusType::Wave => 0,
            haptic_protocol::StimulusType::TravellingWave => 1,
        }
    }

    fn encode_scale_mode(mode: haptic_protocol::SpatialScaleMode) -> u32 {
        match mode {
            haptic_protocol::SpatialScaleMode::Speed => 0,
            haptic_protocol::SpatialScaleMode::Wavelength => 1,
        }
    }
}

/// Command-side reconnecting client. It waits for the server's one-shot
/// `HelloAccepted` response, then receives no continuous status stream. A
/// background manager keeps the connection up, re-sending the `Hello`
/// handshake on every (re)connect, so the plugin survives server restarts
/// without being reloaded.
pub struct IpcClient {
    command_tx: Sender<HapticCommand>,
    connected: Arc<AtomicBool>,
    /// Current note-type config, re-sent in `Hello` on each (re)connect so the
    /// server always has this instance's live configuration after a restart.
    config: Arc<ConfigSnapshot>,
}

impl IpcClient {
    /// Spawn the connection manager. Infallible: if the server is down the
    /// manager simply keeps retrying in the background.
    pub fn spawn(instance_id: u64, initial_config: InstanceConfig, diag: Arc<Diagnostics>) -> Self {
        let (tx, rx) = bounded(256);
        let connected = Arc::new(AtomicBool::new(false));
        let config = Arc::new(ConfigSnapshot::new(initial_config));
        let socket_path =
            std::env::var("HAPTIC_SOCKET_PATH").unwrap_or_else(|_| SOCKET_PATH.to_string());
        nih_log!(
            "IPC client manager spawned for instance {} on {}",
            instance_id,
            socket_path
        );
        {
            let connected = connected.clone();
            let config = config.clone();
            thread::spawn(move || {
                connection_manager(instance_id, &socket_path, rx, connected, config, diag)
            });
        }
        Self {
            command_tx: tx,
            connected,
            config,
        }
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
        self.config.store(cfg);
    }
}

fn connection_manager(
    instance_id: u64,
    socket_path: &str,
    rx: Receiver<HapticCommand>,
    connected: Arc<AtomicBool>,
    config: Arc<ConfigSnapshot>,
    diag: Arc<Diagnostics>,
) {
    let mut frame = Vec::with_capacity(256);
    loop {
        let mut stream = match UnixStream::connect(socket_path) {
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
            protocol_version: PROTOCOL_VERSION,
            instance_id,
            role: ClientRole::Controller,
            config: config.load(),
        };
        if encode_frame(&hello, &mut frame).is_err() || stream.write_all(&frame).is_err() {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        if !await_hello_accepted(&mut stream, instance_id) {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        connected.store(true, Ordering::Relaxed);
        diag.connected();
        let _ = stream.set_read_timeout(Some(Duration::from_millis(10)));

        // Write commands until the socket fails. Controllers receive no status,
        // so a short read probe on idle intervals detects a stopped server even
        // when the user is not currently sending MIDI.
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(cmd) => {
                    if encode_frame(&cmd, &mut frame).is_ok() && stream.write_all(&frame).is_err() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let mut probe = [0u8; 1];
                    match stream.read(&mut probe) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) if is_terminal_socket_error(&e) => break,
                        // Read timeouts are reported inconsistently across
                        // Unix variants. Anything other than an explicit
                        // terminal transport error leaves the connection
                        // alive; a later write will also detect a dead peer.
                        Err(_) => {}
                    }
                }
                // Sender dropped: plugin is being torn down.
                Err(RecvTimeoutError::Disconnected) => {
                    connected.store(false, Ordering::Relaxed);
                    return;
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
    }
}

fn await_hello_accepted(stream: &mut UnixStream, instance_id: u64) -> bool {
    if stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
    {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut decoder = FrameDecoder::new();
    let mut input = [0u8; 128];

    while Instant::now() < deadline {
        loop {
            match decoder.next_frame::<ServerStatus>() {
                Ok(Some(ServerStatus::HelloAccepted {
                    protocol_version,
                    instance_id: accepted_id,
                })) => {
                    return protocol_version == PROTOCOL_VERSION && accepted_id == instance_id;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => return false,
            }
        }

        match stream.read(&mut input) {
            Ok(0) => return false,
            Ok(n) => decoder.extend(&input[..n]),
            Err(e) if is_terminal_socket_error(&e) => return false,
            Err(_) => continue,
        }
    }
    false
}

fn is_terminal_socket_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use haptic_protocol::StimulusType;

    #[test]
    fn handshake_is_connected_only_after_matching_server_ack() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            let mut frame = Vec::new();
            encode_frame(
                &ServerStatus::HelloAccepted {
                    protocol_version: PROTOCOL_VERSION,
                    instance_id: 42,
                },
                &mut frame,
            )
            .unwrap();
            let split = frame.len() / 2;
            server.write_all(&frame[..split]).unwrap();
            server.write_all(&frame[split..]).unwrap();
        });

        assert!(await_hello_accepted(&mut client, 42));
        writer.join().unwrap();
    }

    #[test]
    fn atomic_config_snapshot_roundtrips_as_one_value() {
        let snapshot = ConfigSnapshot::new(InstanceConfig::default());
        let expected = InstanceConfig {
            stimulus_type: StimulusType::TravellingWave,
            wave_speed: 3.25,
            travelling_wave: haptic_protocol::TravellingWaveConfig {
                scale_mode: haptic_protocol::SpatialScaleMode::Wavelength,
                wave_speed: 4.5,
                wavelength_m: 0.075,
            },
            distance_decay: haptic_protocol::DistanceDecay {
                d0_m: 0.8,
                exponent: 2.0,
            },
        };
        snapshot.store(expected);
        assert_eq!(snapshot.load(), expected);
    }

    #[test]
    fn diagnostic_snapshot_reads_atomic_counters() {
        let diagnostics = Diagnostics::new(42);
        diagnostics.connected();
        diagnostics.record(2, 1, 7, 3);
        let snapshot = diagnostics.snapshot();
        assert_eq!(diagnostics.instance_id, 42);
        assert_eq!(snapshot.connect_generation, 1);
        assert_eq!(snapshot.notes_on, 2);
        assert_eq!(snapshot.notes_off, 1);
        assert_eq!(snapshot.mpe_updates, 7);
        assert_eq!(snapshot.sends_dropped, 3);
    }
}
