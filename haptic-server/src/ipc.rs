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

/// Minimum interval between VoiceState broadcasts. 4 ms (~250 Hz) keeps the
/// socket well ahead of a 120 Hz display without flooding slow clients.
const VOICE_BROADCAST_INTERVAL: Duration = Duration::from_millis(4);

/// A connected plugin instance with its stream-reassembly state.
struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
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
            Ok((mut stream, _)) => {
                eprintln!("New client connected");
                if let Err(e) = stream.set_nonblocking(true) {
                    eprintln!("Failed to set stream nonblocking: {}", e);
                    continue;
                }
                // Greet each client with the resolved layout and current
                // monitor routing so viewers mirror the running state
                let routing = routing_status(&routes, device_channels.load(Ordering::Relaxed));
                let mut greeted = true;
                for status in [layout_status(&layout), routing] {
                    if encode_frame(&status, &mut status_frame).is_ok() {
                        if let Err(e) = stream.write_all(&status_frame) {
                            eprintln!("Dropping client on greeting write failure: {}", e);
                            greeted = false;
                            break;
                        }
                    }
                }
                if greeted {
                    clients.push(Client { stream, decoder: FrameDecoder::new() });
                }
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

        // Broadcast routing when it changes or once the device is known
        let dc = device_channels.load(Ordering::Relaxed);
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
                let status = ServerStatus::VoiceState {
                    timestamp_us: now_us(),
                    seq: v.seq,
                    note: v.note,
                    frequency: v.frequency,
                    wave_speed: v.wave_speed,
                    source_pos: v.source_pos,
                    amplitude: v.amplitude,
                    sample_rate: v.sample_rate,
                    delay_samples: v.delay_samples,
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

/// Write a frame to every client, dropping clients whose stream fails
/// (a partial write would corrupt their framing anyway).
fn broadcast(clients: &mut Vec<Client>, frame: &[u8]) {
    clients.retain_mut(|client| match client.stream.write_all(frame) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("Dropping client on status write failure: {}", e);
            false
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
                if command_producer.push(command.into()).is_err() {
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

        // Two frames coalesced into a single write
        let mut coalesced = Vec::new();
        let mut frame = Vec::new();
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
        assert!(matches!(pop_with_timeout(), EngineCommand::NoteOn { note: 60, channel: 1, .. }));
        assert!(matches!(pop_with_timeout(), EngineCommand::MpeUpdate { channel: 1, .. }));
        assert!(matches!(pop_with_timeout(), EngineCommand::NoteOff { note: 60, channel: 1 }));

        running.store(false, Ordering::Relaxed);
        listener.join().unwrap();
    }
}
