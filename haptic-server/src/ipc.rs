use crate::config::TransducerLayout;
use crate::engine::VoiceSnapshot;
use haptic_protocol::{
    encode_frame, FrameDecoder, FrameError, HapticCommand, InstanceConfig, MpeData, Parameter,
    ServerStatus, MAX_ATTEN_D0_M, MAX_ATTEN_EXPONENT, MAX_FRAME_SIZE, MAX_WAVELENGTH_M,
    MAX_WAVE_SPEED, MIDI_CHANNEL_COUNT, MIN_ATTEN_D0_M, MIN_ATTEN_EXPONENT, MIN_WAVELENGTH_M,
    MIN_WAVE_SPEED, PROTOCOL_VERSION,
};
use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Minimum interval between TransducerLevels broadcasts (~60 Hz).
const LEVELS_BROADCAST_INTERVAL: Duration = Duration::from_millis(16);

/// Minimum interval between ActiveVoices broadcasts. 4 ms (~250 Hz) keeps the
/// socket well ahead of a 120 Hz display without flooding slow clients.
const VOICE_BROADCAST_INTERVAL: Duration = Duration::from_millis(4);

/// Leave enough FIFO capacity for every currently possible voice to receive a
/// NoteOff plus instance teardown/panic traffic. High-rate MPE and parameter
/// updates are expendable when the queue approaches this reserve.
const CRITICAL_COMMAND_RESERVE: usize = 32;

/// Per-observer bound for framed status data waiting on a temporarily
/// backpressured nonblocking socket. This is roughly half a second of the
/// current high-rate voice stream. A client that exceeds it is disconnected
/// through the normal instance-cleanup path rather than allowed unbounded
/// memory growth.
const MAX_CLIENT_STATUS_BYTES: usize = MAX_FRAME_SIZE * 16;

/// A connected plugin instance with its stream-reassembly state.
struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    /// Instance identity bound by this connection's `Hello`. Every command
    /// from this client is stamped with it before entering the engine queue.
    /// Set exactly once by a valid `Hello`. Commands are rejected until then.
    instance_id: Option<u64>,
    /// Whether this client receives the status stream (set true when it
    /// identifies as an Observer in its `Hello`). Controllers receive only a
    /// one-shot handshake acknowledgement, never the continuous status stream.
    /// False until `Hello`.
    wants_status: bool,
    /// Whether the one-time layout+routing greeting has been sent (only to
    /// observers, once they identify).
    greeted: bool,
    /// Framed server-status bytes awaiting a nonblocking write. The cursor
    /// preserves partial writes, so a temporary `WouldBlock` never corrupts
    /// framing or immediately disconnects a healthy observer.
    status_output: Vec<u8>,
    status_cursor: usize,
    status_overflowed: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn listen_loop(
    socket_path: &str,
    running: Arc<AtomicBool>,
    command_producer: rtrb::Producer<crate::engine::EngineCommand>,
    levels_consumer: rtrb::Consumer<[f32; 32]>,
    voice_consumer: rtrb::Consumer<VoiceSnapshot>,
    layout: TransducerLayout,
    layout_consumer: rtrb::Consumer<TransducerLayout>,
    device_channels: Arc<AtomicU16>,
) -> Result<(), Box<dyn std::error::Error>> {
    listen_loop_at(
        socket_path,
        running,
        command_producer,
        levels_consumer,
        voice_consumer,
        layout,
        layout_consumer,
        device_channels,
    )
}

#[allow(clippy::too_many_arguments)]
fn listen_loop_at(
    socket_path: &str,
    running: Arc<AtomicBool>,
    mut command_producer: rtrb::Producer<crate::engine::EngineCommand>,
    mut levels_consumer: rtrb::Consumer<[f32; 32]>,
    mut voice_consumer: rtrb::Consumer<VoiceSnapshot>,
    mut layout: TransducerLayout,
    mut layout_consumer: rtrb::Consumer<TransducerLayout>,
    device_channels: Arc<AtomicU16>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = bind_listener(socket_path)?;
    listener.set_nonblocking(true)?;

    eprintln!("IPC server listening on {socket_path}");

    let mut clients: Vec<Client> = Vec::new();
    // IDs stay reserved until their DisconnectInstance command has entered the
    // engine FIFO. This prevents a fast reconnect from being released by an
    // older connection's delayed cleanup command.
    let mut active_instances: HashSet<u64> = HashSet::new();
    let mut pending_disconnects: VecDeque<u64> = VecDeque::new();
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
                // receive only their handshake acknowledgement.
                clients.push(Client {
                    stream,
                    decoder: FrameDecoder::new(),
                    instance_id: None,
                    wants_status: false,
                    greeted: false,
                    status_output: Vec::with_capacity(1024),
                    status_cursor: 0,
                    status_overflowed: false,
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
            let keep = handle_client(
                client,
                &mut command_producer,
                &mut active_instances,
                &mut routes,
                &mut routing_dirty,
            );
            if !keep {
                if let Some(instance_id) = client.instance_id {
                    pending_disconnects.push_back(instance_id);
                }
            }
            keep
        });

        // Cleanup is never discarded merely because the audio command ring is
        // temporarily full. Preserve FIFO order by retrying on later IPC loops.
        while let Some(&instance_id) = pending_disconnects.front() {
            match command_producer
                .push(crate::engine::EngineCommand::DisconnectInstance { instance_id })
            {
                Ok(()) => {
                    pending_disconnects.pop_front();
                    active_instances.remove(&instance_id);
                }
                Err(rtrb::PushError::Full(_)) => break,
            }
        }

        // Greet newly-identified observers with the resolved layout and
        // current monitor routing so they mirror the running state.
        let dc = device_channels.load(Ordering::Relaxed);
        for client in clients.iter_mut() {
            if client.wants_status && !client.greeted {
                for status in [layout_status(&layout), routing_status(&routes, dc)] {
                    if encode_frame(&status, &mut status_frame).is_ok() {
                        queue_status_frame(client, &status_frame);
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
                let status = ServerStatus::TransducerLevels {
                    timestamp_us: now_us(),
                    levels,
                };
                if encode_frame(&status, &mut status_frame).is_ok() {
                    broadcast(&mut clients, &status_frame);
                }
            }
        }

        // Complete queued observer writes without blocking this IPC loop.
        // Any terminal failure or sustained backlog is removed through the
        // same disconnect queue as a read-side socket closure, guaranteeing
        // that viewer-owned held notes are released.
        flush_status_clients(&mut clients, &mut pending_disconnects);

        thread::sleep(Duration::from_millis(1));
    }

    // Cleanup
    let _ = std::fs::remove_file(socket_path);
    eprintln!("IPC server stopped");

    Ok(())
}

fn bind_listener(socket_path: &str) -> std::io::Result<UnixListener> {
    match UnixStream::connect(socket_path) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("a haptic server is already listening on {socket_path}"),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // The path exists but has no listener: a stale socket left by an
            // unclean shutdown. This is the only existing path we unlink.
            std::fs::remove_file(socket_path)?;
        }
        Err(e) => return Err(e),
    }
    UnixListener::bind(socket_path)
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
    ServerStatus::MonitorRouting {
        device_channels,
        routes: *routes,
    }
}

/// Queue a complete framed message for every observer. Controllers are skipped
/// after their one-shot acknowledgement, so their socket buffers never fill.
fn broadcast(clients: &mut [Client], frame: &[u8]) {
    for client in clients.iter_mut().filter(|client| client.wants_status) {
        queue_status_frame(client, frame);
    }
}

fn queue_status_frame(client: &mut Client, frame: &[u8]) {
    if client.status_overflowed {
        return;
    }

    let pending = client.status_output.len() - client.status_cursor;
    if pending + frame.len() > MAX_CLIENT_STATUS_BYTES {
        client.status_overflowed = true;
        return;
    }

    if client.status_cursor == client.status_output.len() {
        client.status_output.clear();
        client.status_cursor = 0;
    } else if client.status_cursor > 0 {
        client.status_output.drain(..client.status_cursor);
        client.status_cursor = 0;
    }
    client.status_output.extend_from_slice(frame);
}

fn flush_status_output(client: &mut Client) -> bool {
    if client.status_overflowed {
        eprintln!(
            "Dropping observer after status backlog exceeded {} bytes",
            MAX_CLIENT_STATUS_BYTES
        );
        return false;
    }

    while client.status_cursor < client.status_output.len() {
        match client
            .stream
            .write(&client.status_output[client.status_cursor..])
        {
            Ok(0) => {
                eprintln!("Dropping client after zero-byte status write");
                return false;
            }
            Ok(written) => client.status_cursor += written,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                eprintln!("Dropping client on terminal status write failure: {error}");
                return false;
            }
        }
    }

    client.status_output.clear();
    client.status_cursor = 0;
    true
}

fn flush_status_clients(clients: &mut Vec<Client>, pending_disconnects: &mut VecDeque<u64>) {
    clients.retain_mut(|client| {
        let keep = flush_status_output(client);
        if !keep {
            if let Some(instance_id) = client.instance_id {
                pending_disconnects.push_back(instance_id);
            }
        }
        keep
    });
}

/// Drain all available bytes from the client and dispatch every complete
/// frame. Returns `false` when the connection should be dropped.
fn handle_client(
    client: &mut Client,
    command_producer: &mut rtrb::Producer<crate::engine::EngineCommand>,
    active_instances: &mut HashSet<u64>,
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
            Ok(Some(mut command)) => {
                if let Err(e) = validate_command(&mut command) {
                    if client.instance_id.is_none()
                        || matches!(&command, HapticCommand::Hello { .. })
                    {
                        eprintln!("Handshake validation failed, dropping client: {e}");
                        return false;
                    }
                    // A malformed performance/control update should not tear
                    // down an otherwise healthy held-note connection. Drop
                    // only that command; lifecycle cleanup still happens if
                    // the transport itself actually fails.
                    eprintln!("Protocol validation failed, dropping command: {e}");
                    continue;
                }

                // Bind identity and role once. Every other command requires a
                // successful Hello first.
                if let HapticCommand::Hello {
                    instance_id, role, ..
                } = &command
                {
                    if client.instance_id.is_some() {
                        eprintln!("Protocol error, dropping client: repeated Hello");
                        return false;
                    }
                    if active_instances.contains(instance_id) {
                        eprintln!("Protocol error, dropping client: instance {instance_id} already connected");
                        return false;
                    }
                    client.instance_id = Some(*instance_id);
                    client.wants_status = *role == haptic_protocol::ClientRole::Observer;
                    active_instances.insert(*instance_id);
                } else if client.instance_id.is_none() {
                    eprintln!("Protocol error, dropping client: command before Hello");
                    return false;
                }
                let routing_change = if let HapticCommand::SetParameter {
                    parameter: Parameter::MonitorRoute { output, source },
                    ..
                } = &command
                {
                    Some((*output, *source))
                } else {
                    None
                };
                let handshake_ack = if let HapticCommand::Hello {
                    protocol_version,
                    instance_id,
                    ..
                } = &command
                {
                    Some((*protocol_version, *instance_id))
                } else {
                    None
                };
                let is_handshake = matches!(&command, HapticCommand::Hello { .. });
                let critical = matches!(
                    &command,
                    HapticCommand::Hello { .. }
                        | HapticCommand::NoteOff { .. }
                        | HapticCommand::Panic
                );
                if !critical && command_producer.slots() <= CRITICAL_COMMAND_RESERVE {
                    eprintln!("Command queue reserve reached, dropping non-critical command");
                    continue;
                }
                let engine_cmd = crate::engine::EngineCommand::from_wire(
                    command,
                    client.instance_id.expect("validated handshake state"),
                );
                if command_producer.push(engine_cmd).is_ok() {
                    // A socket connect and successful client-side write do not
                    // prove that the server accepted the protocol or identity.
                    // Acknowledge only after registration entered the engine
                    // FIFO, so clients can report a real connection.
                    if let Some((protocol_version, instance_id)) = handshake_ack {
                        let status = ServerStatus::HelloAccepted {
                            protocol_version,
                            instance_id,
                        };
                        let mut ack_frame = Vec::with_capacity(32);
                        if encode_frame(&status, &mut ack_frame).is_err() {
                            eprintln!("Failed to acknowledge client handshake");
                            return false;
                        }
                        queue_status_frame(client, &ack_frame);
                    }
                    // Mirror routing only after the engine FIFO accepted the
                    // command, so status never acknowledges a dropped update.
                    if let Some((output, source)) = routing_change {
                        routes[output as usize] = source;
                        *routing_dirty = true;
                    }
                } else {
                    eprintln!("Command queue full, dropping critical command");
                    if is_handshake {
                        return false;
                    }
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

fn validate_mpe(mpe: &mut MpeData) -> Result<(), &'static str> {
    if !mpe.pressure.is_finite() || !mpe.pitch_bend.is_finite() || !mpe.timbre.is_finite() {
        return Err("MPE values must be finite");
    }
    // Hosts and controller conversions may overshoot normalized endpoints by
    // a few ULPs. Normalize finite values instead of turning one expressive
    // sample into a full connection teardown.
    mpe.pressure = mpe.pressure.clamp(0.0, 1.0);
    mpe.pitch_bend = mpe.pitch_bend.clamp(-1.0, 1.0);
    mpe.timbre = mpe.timbre.clamp(0.0, 1.0);
    Ok(())
}

fn validate_config(config: &mut InstanceConfig) -> Result<(), &'static str> {
    if !config.wave_speed.is_finite() || !config.travelling_wave.wave_speed.is_finite() {
        return Err("wave speeds must be finite");
    }
    if !config.travelling_wave.wavelength_m.is_finite() {
        return Err("wavelength must be finite");
    }
    if !config.distance_decay.d0_m.is_finite() || !config.distance_decay.exponent.is_finite() {
        return Err("distance decay must be finite");
    }
    config.wave_speed = config.wave_speed.clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED);
    config.travelling_wave.wave_speed = config
        .travelling_wave
        .wave_speed
        .clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED);
    config.travelling_wave.wavelength_m = config
        .travelling_wave
        .wavelength_m
        .clamp(MIN_WAVELENGTH_M, MAX_WAVELENGTH_M);
    config.distance_decay.d0_m = config
        .distance_decay
        .d0_m
        .clamp(MIN_ATTEN_D0_M, MAX_ATTEN_D0_M);
    config.distance_decay.exponent = config
        .distance_decay
        .exponent
        .clamp(MIN_ATTEN_EXPONENT, MAX_ATTEN_EXPONENT);
    Ok(())
}

/// Validate and, where documented, normalize one decoded wire command before
/// it can enter the real-time engine queue.
fn validate_command(command: &mut HapticCommand) -> Result<(), &'static str> {
    match command {
        HapticCommand::Hello {
            protocol_version,
            instance_id,
            config,
            ..
        } => {
            if *protocol_version != PROTOCOL_VERSION {
                return Err("unsupported protocol version");
            }
            if *instance_id == 0 {
                return Err("instance id must be non-zero");
            }
            validate_config(config)
        }
        HapticCommand::NoteOn {
            note,
            velocity,
            channel,
            mpe,
            ..
        } => {
            if *note > 127 || *velocity > 127 || *channel >= MIDI_CHANNEL_COUNT {
                return Err("MIDI note-on value out of range");
            }
            validate_mpe(mpe)
        }
        HapticCommand::NoteOff { note, channel, .. } => {
            if *note > 127 || *channel >= MIDI_CHANNEL_COUNT {
                return Err("MIDI note-off value out of range");
            }
            Ok(())
        }
        HapticCommand::MpeUpdate { channel, mpe, .. } => {
            if *channel >= MIDI_CHANNEL_COUNT {
                return Err("MPE channel out of range");
            }
            validate_mpe(mpe)
        }
        HapticCommand::SetParameter { parameter, .. } => match parameter {
            Parameter::WaveSpeed(speed) => {
                if !speed.is_finite() {
                    return Err("wave speed must be finite");
                }
                *speed = speed.clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED);
                Ok(())
            }
            Parameter::StimulusType(_) => Ok(()),
            Parameter::MonitorRoute { output, source } => {
                if *output >= 32 || *source >= 32 {
                    return Err("monitor route out of range");
                }
                Ok(())
            }
            Parameter::TravellingWaveScaleMode(_) => Ok(()),
            Parameter::TravellingWaveWavelength(wavelength_m) => {
                if !wavelength_m.is_finite() {
                    return Err("wavelength must be finite");
                }
                *wavelength_m = wavelength_m.clamp(MIN_WAVELENGTH_M, MAX_WAVELENGTH_M);
                Ok(())
            }
            Parameter::AttenuationD0(d0_m) => {
                if !d0_m.is_finite() {
                    return Err("attenuation d0 must be finite");
                }
                *d0_m = d0_m.clamp(MIN_ATTEN_D0_M, MAX_ATTEN_D0_M);
                Ok(())
            }
            Parameter::AttenuationExponent(exponent) => {
                if !exponent.is_finite() {
                    return Err("attenuation exponent must be finite");
                }
                *exponent = exponent.clamp(MIN_ATTEN_EXPONENT, MAX_ATTEN_EXPONENT);
                Ok(())
            }
        },
        HapticCommand::Panic => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineCommand;
    use haptic_protocol::MpeData;

    fn test_client(stream: UnixStream, instance_id: u64) -> Client {
        Client {
            stream,
            decoder: FrameDecoder::new(),
            instance_id: Some(instance_id),
            wants_status: true,
            greeted: true,
            status_output: Vec::new(),
            status_cursor: 0,
            status_overflowed: false,
        }
    }

    #[test]
    fn queued_status_preserves_partial_frame_before_appending() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut client = test_client(stream, 42);
        queue_status_frame(&mut client, &[1, 2, 3, 4]);
        client.status_cursor = 2;
        queue_status_frame(&mut client, &[5, 6]);
        assert_eq!(client.status_output, [3, 4, 5, 6]);
        assert_eq!(client.status_cursor, 0);
    }

    #[test]
    fn terminal_status_failure_queues_instance_cleanup() {
        let (stream, peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut client = test_client(stream, 42);
        queue_status_frame(&mut client, &[1, 2, 3, 4]);
        drop(peer);

        let mut clients = vec![client];
        let mut pending_disconnects = VecDeque::new();
        flush_status_clients(&mut clients, &mut pending_disconnects);
        assert!(clients.is_empty());
        assert_eq!(pending_disconnects.pop_front(), Some(42));
    }

    fn pop_with_timeout(rx: &mut rtrb::Consumer<EngineCommand>) -> EngineCommand {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(cmd) = rx.pop() {
                return cmd;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for command"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// End-to-end over the real Unix socket: coalesced and fragmented
    /// frames must all arrive intact on the engine's command queue.
    #[test]
    fn framed_commands_over_socket_reach_engine_queue() {
        let socket_path = format!(
            "/tmp/haptic-vst-test-{}-{}.sock",
            std::process::id(),
            now_us()
        );
        let running = Arc::new(AtomicBool::new(true));
        let (tx, mut rx) = rtrb::RingBuffer::new(64);
        let (_levels_tx, levels_rx) = rtrb::RingBuffer::new(64);
        let (_voice_tx, voice_rx) = rtrb::RingBuffer::new(64);
        let (_layout_tx, layout_rx) = rtrb::RingBuffer::new(4);
        let listener = {
            let running = running.clone();
            let socket_path = socket_path.clone();
            thread::spawn(move || {
                listen_loop_at(
                    &socket_path,
                    running,
                    tx,
                    levels_rx,
                    voice_rx,
                    TransducerLayout::default(),
                    layout_rx,
                    Arc::new(AtomicU16::new(2)),
                )
                .map_err(|error| error.to_string())
            })
        };

        // Retry until the listener is accepting (a stale socket file from a
        // killed server may exist before the fresh bind, so probing the
        // path is not enough)
        let mut stream = None;
        let mut last_connect_error = None;
        for _ in 0..1000 {
            match UnixStream::connect(&socket_path) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(error) => {
                    last_connect_error = Some(error);
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let mut stream = match stream {
            Some(stream) => stream,
            None => {
                running.store(false, Ordering::Relaxed);
                match listener.join().unwrap() {
                    Ok(()) => panic!("listener never became reachable: {last_connect_error:?}"),
                    Err(error) => panic!("listener failed to start: {error}"),
                }
            }
        };

        // Commands before Hello are rejected and never reach the engine.
        let mut invalid = UnixStream::connect(&socket_path).expect("pre-handshake connect");
        let mut frame = Vec::new();
        encode_frame(&HapticCommand::Panic, &mut frame).unwrap();
        invalid.write_all(&frame).unwrap();
        invalid
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut invalid_buf = [0u8; 1];
        assert_eq!(invalid.read(&mut invalid_buf).unwrap(), 0);
        assert!(rx.pop().is_err());

        // An incompatible protocol version is rejected before registration.
        let mut incompatible = UnixStream::connect(&socket_path).expect("version-mismatch connect");
        encode_frame(
            &HapticCommand::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                instance_id: 41,
                role: haptic_protocol::ClientRole::Controller,
                config: haptic_protocol::InstanceConfig::default(),
            },
            &mut frame,
        )
        .unwrap();
        incompatible.write_all(&frame).unwrap();
        incompatible
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        assert_eq!(incompatible.read(&mut invalid_buf).unwrap(), 0);
        assert!(rx.pop().is_err());

        // Handshake first (as an Observer), then two frames coalesced into a
        // single write. The Hello binds instance_id 42 to this connection, so
        // the following commands must arrive stamped with it.
        let mut coalesced = Vec::new();
        encode_frame(
            &HapticCommand::Hello {
                protocol_version: PROTOCOL_VERSION,
                instance_id: 42,
                role: haptic_protocol::ClientRole::Observer,
                config: haptic_protocol::InstanceConfig {
                    stimulus_type: haptic_protocol::StimulusType::TravellingWave,
                    ..haptic_protocol::InstanceConfig::default()
                },
            },
            &mut frame,
        )
        .unwrap();
        coalesced.extend_from_slice(&frame);
        encode_frame(
            &HapticCommand::NoteOn {
                timestamp_us: 0,
                note: 60,
                velocity: 100,
                channel: 1,
                mpe: MpeData::default(),
            },
            &mut frame,
        )
        .unwrap();
        coalesced.extend_from_slice(&frame);
        for parameter in [
            Parameter::WaveSpeed(12.0),
            Parameter::TravellingWaveScaleMode(haptic_protocol::SpatialScaleMode::Wavelength),
            Parameter::TravellingWaveWavelength(0.125),
            Parameter::AttenuationD0(0.75),
            Parameter::AttenuationExponent(1.5),
        ] {
            encode_frame(
                &HapticCommand::SetParameter {
                    timestamp_us: 0,
                    parameter,
                },
                &mut frame,
            )
            .unwrap();
            coalesced.extend_from_slice(&frame);
        }
        encode_frame(
            &HapticCommand::MpeUpdate {
                timestamp_us: 0,
                channel: 1,
                mpe: MpeData {
                    pressure: 0.9,
                    pitch_bend: 0.1,
                    timbre: 0.4,
                },
            },
            &mut frame,
        )
        .unwrap();
        coalesced.extend_from_slice(&frame);
        stream.write_all(&coalesced).unwrap();

        // One frame fragmented across two delayed writes
        encode_frame(
            &HapticCommand::NoteOff {
                timestamp_us: 0,
                note: 60,
                channel: 1,
            },
            &mut frame,
        )
        .unwrap();
        let (head, tail) = frame.split_at(3);
        stream.write_all(head).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(20));
        stream.write_all(tail).unwrap();

        // Handshake becomes a RegisterInstance; every following command is
        // stamped with the connection's bound instance_id (42).
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::RegisterInstance {
                instance_id: 42,
                ..
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::NoteOn {
                instance_id: 42,
                note: 60,
                channel: 1,
                ..
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::SetParameter {
                instance_id: 42,
                parameter: Parameter::WaveSpeed(12.0)
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::SetParameter {
                instance_id: 42,
                parameter: Parameter::TravellingWaveScaleMode(
                    haptic_protocol::SpatialScaleMode::Wavelength
                )
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::SetParameter {
                instance_id: 42,
                parameter: Parameter::TravellingWaveWavelength(0.125)
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::SetParameter {
                instance_id: 42,
                parameter: Parameter::AttenuationD0(0.75)
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::SetParameter {
                instance_id: 42,
                parameter: Parameter::AttenuationExponent(1.5)
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::MpeUpdate {
                instance_id: 42,
                channel: 1,
                ..
            }
        ));
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::NoteOff {
                instance_id: 42,
                note: 60,
                channel: 1
            }
        ));

        // A second live connection cannot impersonate an existing instance.
        let mut duplicate = UnixStream::connect(&socket_path).expect("duplicate-id connect");
        encode_frame(
            &HapticCommand::Hello {
                protocol_version: PROTOCOL_VERSION,
                instance_id: 42,
                role: haptic_protocol::ClientRole::Controller,
                config: haptic_protocol::InstanceConfig::default(),
            },
            &mut frame,
        )
        .unwrap();
        duplicate.write_all(&frame).unwrap();
        duplicate
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        assert_eq!(duplicate.read(&mut invalid_buf).unwrap(), 0);
        assert!(rx.pop().is_err());

        // Role-gating: the Observer above receives the layout+routing greeting.
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut buf = [0u8; 256];
        let n = stream
            .read(&mut buf)
            .expect("observer should receive a greeting");
        assert!(n > 0, "observer received no status");

        // A Controller receives exactly one handshake acknowledgement, but no
        // continuous observer status stream.
        let mut ctrl = UnixStream::connect(&socket_path).expect("second connect");
        encode_frame(
            &HapticCommand::Hello {
                protocol_version: PROTOCOL_VERSION,
                instance_id: 99,
                role: haptic_protocol::ClientRole::Controller,
                config: haptic_protocol::InstanceConfig::default(),
            },
            &mut frame,
        )
        .unwrap();
        ctrl.write_all(&frame).unwrap();
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::RegisterInstance {
                instance_id: 99,
                ..
            }
        ));
        ctrl.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut cbuf = [0u8; 64];
        let n = ctrl.read(&mut cbuf).expect("controller handshake ack");
        let mut ctrl_decoder = FrameDecoder::new();
        ctrl_decoder.extend(&cbuf[..n]);
        assert!(matches!(
            ctrl_decoder.next_frame::<ServerStatus>().unwrap(),
            Some(ServerStatus::HelloAccepted {
                protocol_version: PROTOCOL_VERSION,
                instance_id: 99,
            })
        ));
        match ctrl.read(&mut cbuf) {
            Ok(0) => panic!("controller connection unexpectedly closed"),
            Ok(n) => panic!("controller received {n} bytes after handshake ack"),
            Err(e) => assert!(
                matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ),
                "unexpected read error: {e}"
            ),
        }

        // A bad post-handshake performance sample is command-local: it must
        // not disconnect the controller or prevent later lifecycle traffic.
        let mut commands = Vec::new();
        encode_frame(
            &HapticCommand::MpeUpdate {
                timestamp_us: 0,
                channel: 1,
                mpe: MpeData {
                    pressure: f32::NAN,
                    ..MpeData::default()
                },
            },
            &mut frame,
        )
        .unwrap();
        commands.extend_from_slice(&frame);
        encode_frame(
            &HapticCommand::NoteOff {
                timestamp_us: 0,
                note: 60,
                channel: 1,
            },
            &mut frame,
        )
        .unwrap();
        commands.extend_from_slice(&frame);
        ctrl.write_all(&commands).unwrap();
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::NoteOff {
                instance_id: 99,
                note: 60,
                channel: 1,
            }
        ));

        // Closing each registered socket produces a reliable instance cleanup
        // command on the same FIFO as its earlier note traffic.
        drop(ctrl);
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::DisconnectInstance { instance_id: 99 }
        ));
        drop(stream);
        assert!(matches!(
            pop_with_timeout(&mut rx),
            EngineCommand::DisconnectInstance { instance_id: 42 }
        ));

        running.store(false, Ordering::Relaxed);
        listener.join().unwrap().unwrap();
    }

    #[test]
    fn command_validation_rejects_non_finite_values_and_normalizes_speed() {
        let mut hello = HapticCommand::Hello {
            protocol_version: PROTOCOL_VERSION,
            instance_id: 1,
            role: haptic_protocol::ClientRole::Controller,
            config: InstanceConfig {
                stimulus_type: haptic_protocol::StimulusType::Wave,
                wave_speed: 20_000.0,
                ..InstanceConfig::default()
            },
        };
        validate_command(&mut hello).unwrap();
        assert!(matches!(
            hello,
            HapticCommand::Hello {
                config: InstanceConfig {
                    wave_speed: MAX_WAVE_SPEED,
                    ..
                },
                ..
            }
        ));

        let mut bad_mpe = HapticCommand::MpeUpdate {
            timestamp_us: 0,
            channel: 1,
            mpe: MpeData {
                pressure: f32::NAN,
                pitch_bend: 0.0,
                timbre: 0.5,
            },
        };
        assert!(validate_command(&mut bad_mpe).is_err());

        let mut overshooting_mpe = HapticCommand::MpeUpdate {
            timestamp_us: 0,
            channel: 1,
            mpe: MpeData {
                pressure: 1.000_1,
                pitch_bend: -1.000_1,
                timbre: 1.5,
            },
        };
        validate_command(&mut overshooting_mpe).unwrap();
        assert!(matches!(
            overshooting_mpe,
            HapticCommand::MpeUpdate {
                mpe: MpeData {
                    pressure: 1.0,
                    pitch_bend: -1.0,
                    timbre: 1.0,
                },
                ..
            }
        ));

        let mut bad_speed = HapticCommand::SetParameter {
            timestamp_us: 0,
            parameter: Parameter::WaveSpeed(f32::INFINITY),
        };
        assert!(validate_command(&mut bad_speed).is_err());

        for parameter in [
            Parameter::TravellingWaveWavelength(f32::NAN),
            Parameter::AttenuationD0(f32::INFINITY),
            Parameter::AttenuationExponent(f32::NEG_INFINITY),
        ] {
            let mut command = HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter,
            };
            assert!(validate_command(&mut command).is_err());
        }

        let mut finite_extremes = HapticCommand::SetParameter {
            timestamp_us: 0,
            parameter: Parameter::TravellingWaveWavelength(1_000.0),
        };
        validate_command(&mut finite_extremes).unwrap();
        assert!(matches!(
            finite_extremes,
            HapticCommand::SetParameter {
                parameter: Parameter::TravellingWaveWavelength(MAX_WAVELENGTH_M),
                ..
            }
        ));
    }
}
