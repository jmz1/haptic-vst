use serde::{Serialize, Deserialize};
use serde::de::DeserializeOwned;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct MpeData {
    pub pressure: f32,      // 0.0-1.0
    pub pitch_bend: f32,    // -1.0 to 1.0
    pub timbre: f32,        // 0.0-1.0
}

impl Default for MpeData {
    fn default() -> Self {
        Self {
            pressure: 0.0,
            pitch_bend: 0.0,
            timbre: 0.5,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StimulusType {
    #[default]
    Wave,
    Standing,
}

/// Per-instance ("note type") configuration a controller registers with the
/// server. Each connected client owns one; its notes inherit this config at
/// note-on. Distinct instances do not share it — this is what replaces the
/// old single server-global wave speed / stimulus type. Extend with further
/// note-type parameters (attenuation, etc.) as they are introduced.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct InstanceConfig {
    pub stimulus_type: StimulusType,
    pub wave_speed: f32,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self { stimulus_type: StimulusType::Wave, wave_speed: 20.0 }
    }
}

/// Engine parameters settable from clients. `WaveSpeed` and `StimulusType`
/// patch the sending connection's own `InstanceConfig` (per-instance).
/// `MonitorRoute` is a server-global device concern (instance-independent).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum Parameter {
    /// Wave propagation speed in m/s — patches the sender's instance config.
    WaveSpeed(f32),
    /// Which stimulus pool new notes are allocated from — patches the
    /// sender's instance config.
    StimulusType(StimulusType),
    /// Connect physical device output `output` to logical transducer
    /// channel `source` — lets a stereo test device audition any of the
    /// 32 logical channels. Default routing is identity. Server-global.
    MonitorRoute { output: u8, source: u8 },
}

/// How a connected client relates to the server. Controllers only *send*
/// (notes, config) and receive no status broadcasts — the plugin, which no
/// longer visualises anything. Observers receive the status stream (layout,
/// routing, active voices, levels); the viewer is one, and may still send
/// (its test console) using its own instance.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ClientRole {
    #[default]
    Controller,
    Observer,
}

/// Maximum concurrently-visualised voices carried in an `ActiveVoices`
/// status (matches the wave-stimulus pool size).
pub const MAX_ACTIVE_VOICES: usize = 8;

/// Compact per-voice state for visualisation. The viewer recomputes each
/// transducer's propagation delay, relative phase, and local amplitude
/// geometrically from `source_pos`, `wave_speed`, `frequency` and the known
/// layout — so no per-transducer delay array is transmitted, and many voices
/// fit one frame. `instance_id` + `note_type` let the viewer filter or sum.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct VoiceInfo {
    pub instance_id: u64,
    pub seq: u64,
    pub note: u8,
    pub note_type: StimulusType,
    pub frequency: f32,
    pub wave_speed: f32,
    /// Effective (velocity-limited) source position the delay lines radiate from.
    pub source_pos: (f32, f32),
    /// Position MPE is asking the source to move to.
    pub requested_pos: (f32, f32),
    pub amplitude: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum HapticCommand {
    /// Handshake: registers this connection's instance identity and initial
    /// note-type config. Sent once on connect, before any notes. The server
    /// binds `instance_id` to the connection and stamps every later command
    /// from it, so notes/MPE need not carry the id themselves.
    Hello {
        instance_id: u64,
        role: ClientRole,
        config: InstanceConfig,
    },
    NoteOn {
        timestamp_us: u64,
        note: u8,
        velocity: u8,
        channel: u8,
        mpe: MpeData,
    },
    NoteOff {
        timestamp_us: u64,
        note: u8,
        channel: u8,
    },
    MpeUpdate {
        timestamp_us: u64,
        channel: u8,
        mpe: MpeData,
    },
    SetParameter {
        timestamp_us: u64,
        parameter: Parameter,
    },
    Panic,              // Stop all
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerStatus {
    TransducerLevels {
        timestamp_us: u64,
        levels: [f32; 32],
    },
    PerformanceMetrics {
        active_stimuli: u8,
        cpu_percent: u8,
    },
    /// The server's resolved transducer layout, sent to each client on
    /// connect and rebroadcast on config hot-reload. Distances in metres.
    Layout {
        positions: [(f32, f32); 32],
        gains: [f32; 32],
        table_m: (f32, f32),
    },
    /// Physical-output → logical-channel monitor routing currently in
    /// effect, plus how many output channels the audio device has.
    /// Sent on connect and whenever the routing changes.
    MonitorRouting {
        device_channels: u16,
        routes: [u8; 32],
    },
    /// All currently active delay-line (wave) voices, for phase
    /// visualisation. Replaces the old single-voice `VoiceState`: the viewer
    /// is the one observer of whole-server state, so it receives every voice
    /// (tagged with `instance_id` + `note_type` for filtering / summing) and
    /// reconstructs the per-transducer field itself. `count` entries of
    /// `voices` are valid. `sample_rate` is the engine's internal render rate.
    ActiveVoices {
        timestamp_us: u64,
        sample_rate: f32,
        count: u8,
        voices: [VoiceInfo; MAX_ACTIVE_VOICES],
    },
}

pub const SOCKET_PATH: &str = "/tmp/haptic-vst.sock";

// ---------------------------------------------------------------------------
// Message framing
//
// Streams carry length-prefixed bincode frames: a u32 (little-endian) payload
// length followed by the bincode-serialized message. Both directions use the
// same format so coalesced or fragmented socket reads reassemble correctly.
// ---------------------------------------------------------------------------

/// Upper bound on a single frame payload; anything larger indicates stream
/// corruption or a protocol mismatch and the connection should be dropped.
pub const MAX_FRAME_SIZE: usize = 4096;

#[derive(Debug)]
pub enum FrameError {
    /// Declared payload length exceeds MAX_FRAME_SIZE; the stream is
    /// unrecoverable and the connection should be closed.
    Oversized(usize),
    /// A well-framed payload failed to deserialize; the frame has been
    /// discarded and the stream remains usable.
    Deserialize(bincode::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Oversized(n) => write!(f, "frame length {} exceeds maximum {}", n, MAX_FRAME_SIZE),
            FrameError::Deserialize(e) => write!(f, "frame deserialization failed: {}", e),
        }
    }
}

impl std::error::Error for FrameError {}

/// Serialize `msg` as a length-prefixed frame into `out` (cleared first).
pub fn encode_frame<T: Serialize>(msg: &T, out: &mut Vec<u8>) -> Result<(), bincode::Error> {
    out.clear();
    out.extend_from_slice(&[0u8; 4]);
    bincode::serialize_into(&mut *out, msg)?;
    let len = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Accumulates raw stream bytes and yields complete deserialized frames.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::with_capacity(MAX_FRAME_SIZE) }
    }

    /// Append raw bytes read from the stream.
    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to decode the next complete frame. Returns Ok(None) when more
    /// bytes are needed. On FrameError::Deserialize the offending frame is
    /// consumed and subsequent calls continue with the next frame; on
    /// FrameError::Oversized the stream should be abandoned.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(FrameError::Oversized(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let result = bincode::deserialize(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        match result {
            Ok(msg) => Ok(Some(msg)),
            Err(e) => Err(FrameError::Deserialize(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(note: u8) -> HapticCommand {
        HapticCommand::NoteOn {
            timestamp_us: 42,
            note,
            velocity: 100,
            channel: 1,
            mpe: MpeData { pressure: 0.7, pitch_bend: -0.25, timbre: 0.5 },
        }
    }

    fn assert_note(cmd: &HapticCommand, expected: u8) {
        match cmd {
            HapticCommand::NoteOn { note, .. } => assert_eq!(*note, expected),
            other => panic!("unexpected command: {:?}", other),
        }
    }

    #[test]
    fn roundtrip_single_frame() {
        let mut buf = Vec::new();
        encode_frame(&note_on(60), &mut buf).unwrap();

        let mut dec = FrameDecoder::new();
        dec.extend(&buf);
        let cmd: HapticCommand = dec.next_frame().unwrap().unwrap();
        assert_note(&cmd, 60);
        assert!(dec.next_frame::<HapticCommand>().unwrap().is_none());
    }

    #[test]
    fn coalesced_frames_decode_individually() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        encode_frame(&note_on(60), &mut a).unwrap();
        encode_frame(&note_on(64), &mut b).unwrap();

        let mut dec = FrameDecoder::new();
        dec.extend(&a);
        dec.extend(&b);
        assert_note(&dec.next_frame::<HapticCommand>().unwrap().unwrap(), 60);
        assert_note(&dec.next_frame::<HapticCommand>().unwrap().unwrap(), 64);
        assert!(dec.next_frame::<HapticCommand>().unwrap().is_none());
    }

    #[test]
    fn fragmented_frame_reassembles() {
        let mut buf = Vec::new();
        encode_frame(&note_on(72), &mut buf).unwrap();

        let mut dec = FrameDecoder::new();
        for byte in &buf[..buf.len() - 1] {
            dec.extend(std::slice::from_ref(byte));
            assert!(dec.next_frame::<HapticCommand>().unwrap().is_none());
        }
        dec.extend(&buf[buf.len() - 1..]);
        assert_note(&dec.next_frame::<HapticCommand>().unwrap().unwrap(), 72);
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut dec = FrameDecoder::new();
        dec.extend(&(MAX_FRAME_SIZE as u32 + 1).to_le_bytes());
        dec.extend(&[0u8; 8]);
        assert!(matches!(dec.next_frame::<HapticCommand>(), Err(FrameError::Oversized(_))));
    }

    #[test]
    fn corrupt_frame_skipped_stream_continues() {
        let mut dec = FrameDecoder::new();
        // A frame whose payload is garbage for HapticCommand
        let garbage = [0xFFu8; 16];
        dec.extend(&(garbage.len() as u32).to_le_bytes());
        dec.extend(&garbage);
        let mut good = Vec::new();
        encode_frame(&note_on(48), &mut good).unwrap();
        dec.extend(&good);

        assert!(matches!(dec.next_frame::<HapticCommand>(), Err(FrameError::Deserialize(_))));
        assert_note(&dec.next_frame::<HapticCommand>().unwrap().unwrap(), 48);
    }

    #[test]
    fn hello_roundtrips() {
        let hello = HapticCommand::Hello {
            instance_id: 0xDEAD_BEEF_1234_5678,
            role: ClientRole::Observer,
            config: InstanceConfig { stimulus_type: StimulusType::Standing, wave_speed: 3.5 },
        };
        let mut buf = Vec::new();
        encode_frame(&hello, &mut buf).unwrap();
        let mut dec = FrameDecoder::new();
        dec.extend(&buf);
        match dec.next_frame::<HapticCommand>().unwrap().unwrap() {
            HapticCommand::Hello { instance_id, role, config } => {
                assert_eq!(instance_id, 0xDEAD_BEEF_1234_5678);
                assert_eq!(role, ClientRole::Observer);
                assert_eq!(config.stimulus_type, StimulusType::Standing);
                assert_eq!(config.wave_speed, 3.5);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn active_voices_roundtrips_within_frame_budget() {
        let mut voices = [VoiceInfo::default(); MAX_ACTIVE_VOICES];
        for (i, v) in voices.iter_mut().enumerate() {
            v.instance_id = i as u64 + 1;
            v.note = 60 + i as u8;
            v.frequency = 40.0 + i as f32;
            v.wave_speed = 2.0;
            v.source_pos = (0.5, 1.0);
            v.amplitude = 0.5;
        }
        let status = ServerStatus::ActiveVoices {
            timestamp_us: 1,
            sample_rate: 1500.0,
            count: MAX_ACTIVE_VOICES as u8,
            voices,
        };
        let mut buf = Vec::new();
        encode_frame(&status, &mut buf).unwrap();
        // A full ActiveVoices frame must fit the framing budget.
        assert!(buf.len() <= MAX_FRAME_SIZE, "frame {} > max {}", buf.len(), MAX_FRAME_SIZE);
        let mut dec = FrameDecoder::new();
        dec.extend(&buf);
        match dec.next_frame::<ServerStatus>().unwrap().unwrap() {
            ServerStatus::ActiveVoices { count, voices, .. } => {
                assert_eq!(count, MAX_ACTIVE_VOICES as u8);
                assert_eq!(voices[3].note, 63);
                assert_eq!(voices[3].instance_id, 4);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
