use std::os::unix::net::{UnixListener, UnixStream};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::thread;
use std::time::Duration;
use std::io::Write;
use std::time::Instant;
use haptic_protocol::{encode_frame, FrameDecoder, FrameError, HapticCommand, Parameter, ServerStatus, SOCKET_PATH};
use crate::config::TransducerLayout;
use crate::engine::VoiceSnapshot;

/// Minimum interval between TransducerLevels broadcasts (~60 Hz).
const LEVELS_BROADCAST_INTERVAL: Duration = Duration::from_millis(16);

/// Minimum interval between ActiveVoices broadcasts. 4 ms (~250 Hz) keeps the
/// socket well ahead of a 120 Hz display without flooding slow clients.
const VOICE_BROADCAST_INTERVAL: Duration = Duration::from_millis(4);

/// A connected plugin instance with its stream-reassembly state.
struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    /// Instance identity bound by this connection's `Hello`. Every command
    /// from this client is stamped with it before entering the engine queue.
    /// 0 until a `Hello` arrives (the default-instance fallback).
    instance_id: u64,
    /// Whether this client receives the status stream (set true when it
    /// identifies as an Observer in its `Hello`). Controllers never receive
    /// status, so a pure write-side plugin is never dropped for an unread
    /// socket. False until `Hello`.
    wants_status: bool,
    /// Whether the one-time layout+routing greeting has been sent (only to
    /// observers, once they identify).
    greeted: bool,
}

pub fn listen_loop(
    running: Arc<AtomicBool>,
    mut command_producer: rtrb::Producer<crate::engine::EngineCommand>,
    mut levels_consumer: rtrb::Consumer<[f32; 32]>,
    mut voice_consumer: rtrb::Consumer<VoiceSnapshot>,
    mut layout: TransducerLayout,
    mut layout_consumer: rtrb::Consumer<TransducerLayout>,
    device_channels: Arc<AtomicU16>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = UnixListener::bind(SOCKET_PATH)?;
    listener.set_nonblocking(true)?;

    eprintln!("IPC server listening on {}", SOCKET_PATH);

    let mut clients: Vec<Client> = Vec::new();
    let mut latest_levels: Option<[f32; 32]> = None;
    let mut last_broadcast = Instant::now();
    let mut last_voice_broadcast = Instant::now();
    let mut status_frame = Vec::with_capacity(512);

    // Mirror of the engine's monitor routing (commands pass through this
    // thread, so a snoop keeps this authoritative for clients)
    let mut routes: [u8; 32] = std::array::from_fn(|i| i as u8);
    let mut routing_dirty = false;
    let mut last_device_channels = 0u16;

    while running.load(Ordering::Relaxed) {
        // Accept new connections
        match listener.accept() {
            Ok((stream, _)) => {
                eprintln!("New client connected");
                if let Err(e) = stream.set_nonblocking(true) {
                    eprintln!("Failed to set stream nonblocking: {}", e);
                    continue;
                }
                // The layout+routing greeting is deferred until the client
                // identifies as an Observer in its Hello (below): controllers
                // receive no status at all.
                clients.push(Client {
                    stream,
                    decoder: FrameDecoder::new(),
                    instance_id: 0,
                    wants_status: false,
                    greeted: false,
                });
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
            handle_client(client, &mut command_producer, &mut routes, &mut routing_dirty)
        });

        // Greet newly-identified observers with the resolved layout and
        // current monitor routing so they mirror the running state.
        let dc = device_channels.load(Ordering::Relaxed);
        for client in clients.iter_mut() {
            if client.wants_status && !client.greeted {
                for status in [layout_status(&layout), routing_status(&routes, dc)] {
                    if encode_frame(&status, &mut status_frame).is_ok() {
                        let _ = client.stream.write_all(&status_frame);
                    }
                }
                client.greeted = true;
            }
        }

        // Broadcast routing when it changes or once the device is known
        if dc != last_device_channels {
            last_device_channels = dc;
            routing_dirty = true;
        }
        if routing_dirty {
            routing_dirty = false;
            if encode_frame(&routing_status(&routes, dc), &mut status_frame).is_ok() {
                broadcast(&mut clients, &status_frame);
            }
        }

        // Hot reload: adopt and rebroadcast the layout to every client
        while let Ok(new_layout) = layout_consumer.pop() {
            layout = new_layout;
            if encode_frame(&layout_status(&layout), &mut status_frame).is_ok() {
                broadcast(&mut clients, &status_frame);
            }
        }

        // Keep only the freshest levels frame from the audio thread
        while let Ok(levels) = levels_consumer.pop() {
            latest_levels = Some(levels);
        }

        // Forward the freshest voice snapshot for phase visualisation
        let mut latest_voice: Option<VoiceSnapshot> = None;
        while let Ok(snapshot) = voice_consumer.pop() {
            latest_voice = Some(snapshot);
        }
        if let Some(v) = latest_voice {
            if last_voice_broadcast.elapsed() >= VOICE_BROADCAST_INTERVAL {
                last_voice_broadcast = Instant::now();
                let status = ServerStatus::ActiveVoices {
                    timestamp_us: now_us(),
                    sample_rate: v.sample_rate,
                    count: v.count,
                    voices: v.voices,
                };
                if encode_frame(&status, &mut status_frame).is_ok() {
                    broadcast(&mut clients, &status_frame);
                }
            }
        }

        // Broadcast levels to all clients at ~60 Hz
        if last_broadcast.elapsed() >= LEVELS_BROADCAST_INTERVAL {
            if let Some(levels) = latest_levels.take() {
                last_broadcast = Instant::now();
                let status = ServerStatus::TransducerLevels { timestamp_us: now_us(), levels };
                if encode_frame(&status, &mut status_frame).is_ok() {
                    broadcast(&mut clients, &status_frame);
                }
            }
        }

        thread::sleep(Duration::from_millis(1));
    }

    // Cleanup
    let _ = std::fs::remove_file(SOCKET_PATH);
    eprintln!("IPC server stopped");

    Ok(())
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn layout_status(layout: &TransducerLayout) -> ServerStatus {
    ServerStatus::Layout {
        positions: layout.positions,
        gains: layout.gains,
        table_m: layout.table_m,
    }
}

fn routing_status(routes: &[u8; 32], device_channels: u16) -> ServerStatus {
    ServerStatus::MonitorRouting { device_channels, routes: *routes }
}

/// Write a frame to every observer client, dropping clients whose stream
/// fails (a partial write would corrupt their framing anyway). Controllers
/// (which never read) are skipped so their socket buffers never fill.
fn broadcast(clients: &mut Vec<Client>, frame: &[u8]) {
    clients.retain_mut(|client| {
        if !client.wants_status {
            return true;
        }
        match client.stream.write_all(frame) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("Dropping client on status write failure: {}", e);
                false
            }
        }
    });
}

/// Drain all available bytes from the client and dispatch every complete
/// frame. Returns `false` when the connection should be dropped.
fn handle_client(
    client: &mut Client,
    command_producer: &mut rtrb::Producer<crate::engine::EngineCommand>,
    routes: &mut [u8; 32],
    routing_dirty: &mut bool,
) -> bool {
    let mut buffer = [0u8; 1024];

    loop {
        match client.stream.read(&mut buffer) {
            Ok(0) => {
                eprintln!("Client disconnected");
                return false;
            }
            Ok(n) => {
                client.decoder.extend(&buffer[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                eprintln!("Error reading from client: {}", e);
                return false;
            }
        }
    }

    loop {
        match client.decoder.next_frame::<HapticCommand>() {
            Ok(Some(command)) => {
                // Bind this connection's instance identity and status role
                // on handshake.
                if let HapticCommand::Hello { instance_id, role, .. } = &command {
                    client.instance_id = *instance_id;
                    client.wants_status = *role == haptic_protocol::ClientRole::Observer;
                }
                // Track routing changes for rebroadcast to all clients
                if let HapticCommand::SetParameter {
                    parameter: Parameter::MonitorRoute { output, source },
                    ..
                } = &command
                {
                    if (*output as usize) < routes.len() {
                        routes[*output as usize] = (*source).min(31);
                        *routing_dirty = true;
                    }
                }
                let engine_cmd = crate::engine::EngineCommand::from_wire(command, client.instance_id);
                if command_producer.push(engine_cmd).is_err() {
                    // Ring buffer full: the audio thread is not draining or
                    // the client is flooding; drop the command
                    eprintln!("Command queue full, dropping command");
                }
            }
            Ok(None) => break,
            Err(FrameError::Deserialize(e)) => {
                // Frame boundary is intact; skip the bad frame and continue
                eprintln!("Dropping undecodable frame: {}", e);
            }
            Err(e @ FrameError::Oversized(_)) => {
                eprintln!("Protocol error, dropping client: {}", e);
                return false;
            }
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineCommand;
    use haptic_protocol::MpeData;

    /// End-to-end over the real Unix socket: coalesced and fragmented
    /// frames must all arrive intact on the engine's command queue.
    #[test]
    fn framed_commands_over_socket_reach_engine_queue() {
        let running = Arc::new(AtomicBool::new(true));
        let (tx, mut rx) = rtrb::RingBuffer::new(64);
        let (_levels_tx, levels_rx) = rtrb::RingBuffer::new(64);
        let (_voice_tx, voice_rx) = rtrb::RingBuffer::new(64);
        let (_layout_tx, layout_rx) = rtrb::RingBuffer::new(4);
        let listener = {
            let running = running.clone();
            thread::spawn(move || {
                let _ = listen_loop(
                    running,
                    tx,
                    levels_rx,
                    voice_rx,
                    TransducerLayout::default(),
                    layout_rx,
                    Arc::new(AtomicU16::new(2)),
                );
            })
        };

        // Retry until the listener is accepting (a stale socket file from a
        // killed server may exist before the fresh bind, so probing the
        // path is not enough)
        let mut stream = None;
        for _ in 0..200 {
            match UnixStream::connect(SOCKET_PATH) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        let mut stream = stream.expect("connect to listener");

        // Handshake first (as an Observer), then two frames coalesced into a
        // single write. The Hello binds instance_id 42 to this connection, so
        // the following commands must arrive stamped with it.
        let mut coalesced = Vec::new();
        let mut frame = Vec::new();
        encode_frame(&HapticCommand::Hello {
            instance_id: 42,
            role: haptic_protocol::ClientRole::Observer,
            config: haptic_protocol::InstanceConfig::default(),
        }, &mut frame).unwrap();
        coalesced.extend_from_slice(&frame);
        encode_frame(&HapticCommand::NoteOn {
            timestamp_us: 0,
            note: 60,
            velocity: 100,
            channel: 1,
            mpe: MpeData::default(),
        }, &mut frame).unwrap();
        coalesced.extend_from_slice(&frame);
        encode_frame(&HapticCommand::MpeUpdate {
            timestamp_us: 0,
            channel: 1,
            mpe: MpeData { pressure: 0.9, pitch_bend: 0.1, timbre: 0.4 },
        }, &mut frame).unwrap();
        coalesced.extend_from_slice(&frame);
        stream.write_all(&coalesced).unwrap();

        // One frame fragmented across two delayed writes
        encode_frame(&HapticCommand::NoteOff { timestamp_us: 0, note: 60, channel: 1 }, &mut frame).unwrap();
        let (head, tail) = frame.split_at(3);
        stream.write_all(head).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(20));
        stream.write_all(tail).unwrap();

        let mut pop_with_timeout = || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(cmd) = rx.pop() {
                    return cmd;
                }
                assert!(std::time::Instant::now() < deadline, "timed out waiting for command");
                thread::sleep(Duration::from_millis(5));
            }
        };
        // Handshake becomes a RegisterInstance; every following command is
        // stamped with the connection's bound instance_id (42).
        assert!(matches!(pop_with_timeout(), EngineCommand::RegisterInstance { instance_id: 42, .. }));
        assert!(matches!(pop_with_timeout(), EngineCommand::NoteOn { instance_id: 42, note: 60, channel: 1, .. }));
        assert!(matches!(pop_with_timeout(), EngineCommand::MpeUpdate { instance_id: 42, channel: 1, .. }));
        assert!(matches!(pop_with_timeout(), EngineCommand::NoteOff { instance_id: 42, note: 60, channel: 1 }));

        // Role-gating: the Observer above receives the layout+routing greeting.
        stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).expect("observer should receive a greeting");
        assert!(n > 0, "observer received no status");

        // A Controller connection receives no status at all (so a pure
        // write-side plugin is never dropped for an unread socket).
        let mut ctrl = UnixStream::connect(SOCKET_PATH).expect("second connect");
        encode_frame(&HapticCommand::Hello {
            instance_id: 99,
            role: haptic_protocol::ClientRole::Controller,
            config: haptic_protocol::InstanceConfig::default(),
        }, &mut frame).unwrap();
        ctrl.write_all(&frame).unwrap();
        assert!(matches!(pop_with_timeout(), EngineCommand::RegisterInstance { instance_id: 99, .. }));
        ctrl.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut cbuf = [0u8; 64];
        match ctrl.read(&mut cbuf) {
            Ok(0) => {}                                    // closed, fine
            Ok(n) => panic!("controller received {n} bytes of status"),
            Err(e) => assert!(
                matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
                "unexpected read error: {e}"
            ),
        }

        running.store(false, Ordering::Relaxed);
        listener.join().unwrap();
    }
}
