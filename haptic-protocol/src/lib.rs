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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StimulusType {
    Wave,
    Standing,
}

/// Engine parameters settable from clients (plugin or viewer).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum Parameter {
    /// Wave propagation speed in m/s.
    WaveSpeed(f32),
    /// Which stimulus pool new notes are allocated from.
    StimulusType(StimulusType),
    /// Connect physical device output `output` to logical transducer
    /// channel `source` — lets a stereo test device audition any of the
    /// 32 logical channels. Default routing is identity.
    MonitorRoute { output: u8, source: u8 },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum HapticCommand {
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
    /// Snapshot of the most recently started active delay-line (wave)
    /// voice, for phase visualisation. `delay_samples` are the actual
    /// per-transducer propagation delays in use; relative phase at
    /// transducer i is 2*pi * frequency * delay_samples[i] / sample_rate.
    VoiceState {
        timestamp_us: u64,
        seq: u64,
        note: u8,
        frequency: f32,
        wave_speed: f32,
        source_pos: (f32, f32),
        amplitude: f32,
        sample_rate: f32,
        delay_samples: [f32; 32],
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
}
