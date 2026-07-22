use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Wire-protocol version carried by the mandatory `Hello` handshake.
///
/// Bincode encodes enum variants by declaration order, so protocol changes
/// are coordinated and versioned. A server must reject a client whose version
/// does not exactly match this value before accepting any other command.
pub const PROTOCOL_VERSION: u16 = 4;

/// Shared numeric limits used by every producer and the server validator.
pub const MIDI_CHANNEL_COUNT: u8 = 16;
pub const MIN_WAVE_SPEED: f32 = 0.1;
pub const MAX_WAVE_SPEED: f32 = 100.0;
pub const MIN_WAVELENGTH_M: f32 = 0.00125;
pub const MAX_WAVELENGTH_M: f32 = 50.0;
pub const MIN_ATTEN_D0_M: f32 = 0.01;
pub const MAX_ATTEN_D0_M: f32 = 10.0;
pub const MIN_ATTEN_EXPONENT: f32 = 0.0;
pub const MAX_ATTEN_EXPONENT: f32 = 4.0;
pub const DEFAULT_WAVE_SPEED: f32 = 20.0;
pub const DEFAULT_WAVELENGTH_M: f32 = 0.2;
pub const DEFAULT_ATTEN_D0_M: f32 = 2.0;
pub const DEFAULT_ATTEN_EXPONENT: f32 = 1.0;
/// MIDI 33 / Ableton A0 is 55 Hz without transposition.
pub const DEFAULT_TEST_NOTE: u8 = 33;

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct MpeData {
    pub pressure: f32,   // 0.0-1.0
    pub pitch_bend: f32, // -1.0 to 1.0
    pub timbre: f32,     // 0.0-1.0
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
    TravellingWave,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpatialScaleMode {
    #[default]
    Speed,
    Wavelength,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct DistanceDecay {
    pub d0_m: f32,
    pub exponent: f32,
}

impl Default for DistanceDecay {
    fn default() -> Self {
        Self {
            d0_m: DEFAULT_ATTEN_D0_M,
            exponent: DEFAULT_ATTEN_EXPONENT,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TravellingWaveConfig {
    pub scale_mode: SpatialScaleMode,
    pub wave_speed: f32,
    pub wavelength_m: f32,
}

impl Default for TravellingWaveConfig {
    fn default() -> Self {
        Self {
            scale_mode: SpatialScaleMode::Speed,
            wave_speed: DEFAULT_WAVE_SPEED,
            wavelength_m: DEFAULT_WAVELENGTH_M,
        }
    }
}

/// Shared radial distance gain used by the engine and model-level tests.
#[inline]
pub fn distance_gain(distance_m: f32, decay: DistanceDecay) -> f32 {
    let base = 1.0 + distance_m.max(0.0) / decay.d0_m.max(MIN_ATTEN_D0_M);
    let exponent = decay.exponent.clamp(MIN_ATTEN_EXPONENT, MAX_ATTEN_EXPONENT);
    if exponent == 0.0 {
        1.0
    } else if exponent == 1.0 {
        1.0 / base
    } else {
        base.powf(-exponent)
    }
}

#[inline]
pub fn effective_wavelength(
    frequency: f32,
    wave_speed: f32,
    mode: SpatialScaleMode,
    wavelength_m: f32,
) -> f32 {
    match mode {
        SpatialScaleMode::Speed => {
            wave_speed.clamp(MIN_WAVE_SPEED, MAX_WAVE_SPEED) / frequency.max(f32::MIN_POSITIVE)
        }
        SpatialScaleMode::Wavelength => wavelength_m.clamp(MIN_WAVELENGTH_M, MAX_WAVELENGTH_M),
    }
}

/// Relative complex radial field for an instantaneous travelling wave.
/// The source oscillator is the zero-phase reference, so propagation appears
/// as a negative spatial phase rotation.
#[inline]
pub fn travelling_wave_relative_phasor(
    distance_m: f32,
    wavelength_m: f32,
    decay: DistanceDecay,
) -> (f32, f32) {
    let phase = std::f32::consts::TAU * distance_m.max(0.0) / wavelength_m.max(MIN_WAVELENGTH_M);
    let gain = distance_gain(distance_m, decay);
    (gain * phase.cos(), -gain * phase.sin())
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
    pub travelling_wave: TravellingWaveConfig,
    pub distance_decay: DistanceDecay,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            stimulus_type: StimulusType::Wave,
            wave_speed: DEFAULT_WAVE_SPEED,
            travelling_wave: TravellingWaveConfig::default(),
            distance_decay: DistanceDecay::default(),
        }
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
    MonitorRoute {
        output: u8,
        source: u8,
    },
    TravellingWaveScaleMode(SpatialScaleMode),
    TravellingWaveWavelength(f32),
    AttenuationD0(f32),
    AttenuationExponent(f32),
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

/// Maximum concurrently-active oscillator references carried in an
/// `OutputState` status (wave and travelling-wave pools combined).
pub const MAX_ACTIVE_VOICES: usize = 16;

/// Compact per-voice state accompanying the measured output field. Geometry
/// remains useful for source cursors and labels, while `reference_phase` lets
/// the viewer compare the final summed output with any active source
/// oscillator. It is aligned to the analytic samples in the same `OutputState`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct VoiceInfo {
    pub instance_id: u64,
    pub seq: u64,
    pub note: u8,
    pub note_type: StimulusType,
    pub frequency: f32,
    pub wave_speed: f32,
    pub scale_mode: SpatialScaleMode,
    pub wavelength_m: f32,
    pub atten_d0_m: f32,
    pub atten_exponent: f32,
    /// Effective source position. Velocity-limited for Wave; direct for TW.
    pub source_pos: (f32, f32),
    /// Position MPE is asking the source to move to.
    pub requested_pos: (f32, f32),
    pub amplitude: f32,
    /// Analytic phase, in radians, of this voice's source sine oscillator at
    /// the time represented by `OutputState::analytic`.
    pub reference_phase: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum HapticCommand {
    /// Handshake: registers this connection's instance identity and initial
    /// note-type config. Sent once on connect, before any notes. The server
    /// binds `instance_id` to the connection and stamps every later command
    /// from it, so notes/MPE need not carry the id themselves.
    Hello {
        protocol_version: u16,
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
    Panic, // Stop all
}

// The fixed OutputState arrays deliberately keep the wire schema bounded and
// mirrors the allocation-free audio-thread snapshot. Boxing it would add a
// per-status allocation and make the schema's ownership less explicit.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerStatus {
    /// Confirms that the server accepted and registered the connection's
    /// mandatory `Hello`. Clients must not report themselves connected until
    /// this arrives; a successful socket write alone does not mean the server
    /// accepted the identity or protocol version.
    HelloAccepted {
        protocol_version: u16,
        instance_id: u64,
    },
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
    /// Hilbert analytic signal measured from the final bounded sum of all
    /// voices on every logical transducer, after device-rate reconstruction
    /// and before physical monitor routing. `count` entries of `voices` are
    /// valid oscillator references; selecting one never changes `analytic`.
    OutputState {
        timestamp_us: u64,
        device_sample_rate: f32,
        /// Device-frame index at which the newest Hilbert output was
        /// evaluated. The analytic signal and every `reference_phase` include
        /// the same reconstruction and Hilbert group delay.
        sample_index: u64,
        /// False while the Hilbert history is initially filling.
        valid: bool,
        analytic: [(f32, f32); 32],
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
            FrameError::Oversized(n) => {
                write!(f, "frame length {} exceeds maximum {}", n, MAX_FRAME_SIZE)
            }
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
        Self {
            buf: Vec::with_capacity(MAX_FRAME_SIZE),
        }
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
            mpe: MpeData {
                pressure: 0.7,
                pitch_bend: -0.25,
                timbre: 0.5,
            },
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
        assert!(matches!(
            dec.next_frame::<HapticCommand>(),
            Err(FrameError::Oversized(_))
        ));
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

        assert!(matches!(
            dec.next_frame::<HapticCommand>(),
            Err(FrameError::Deserialize(_))
        ));
        assert_note(&dec.next_frame::<HapticCommand>().unwrap().unwrap(), 48);
    }

    #[test]
    fn hello_roundtrips() {
        let hello = HapticCommand::Hello {
            protocol_version: PROTOCOL_VERSION,
            instance_id: 0xDEAD_BEEF_1234_5678,
            role: ClientRole::Observer,
            config: InstanceConfig {
                stimulus_type: StimulusType::TravellingWave,
                wave_speed: 3.5,
                travelling_wave: TravellingWaveConfig {
                    scale_mode: SpatialScaleMode::Wavelength,
                    wave_speed: 3.5,
                    wavelength_m: 0.125,
                },
                distance_decay: DistanceDecay {
                    d0_m: 0.75,
                    exponent: 1.5,
                },
            },
        };
        let mut buf = Vec::new();
        encode_frame(&hello, &mut buf).unwrap();
        let mut dec = FrameDecoder::new();
        dec.extend(&buf);
        match dec.next_frame::<HapticCommand>().unwrap().unwrap() {
            HapticCommand::Hello {
                protocol_version,
                instance_id,
                role,
                config,
            } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(instance_id, 0xDEAD_BEEF_1234_5678);
                assert_eq!(role, ClientRole::Observer);
                assert_eq!(config.stimulus_type, StimulusType::TravellingWave);
                assert_eq!(config.wave_speed, 3.5);
                assert_eq!(
                    config.travelling_wave.scale_mode,
                    SpatialScaleMode::Wavelength
                );
                assert_eq!(config.travelling_wave.wavelength_m, 0.125);
                assert_eq!(config.distance_decay.d0_m, 0.75);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn output_state_roundtrips_within_frame_budget() {
        let mut voices = [VoiceInfo::default(); MAX_ACTIVE_VOICES];
        for (i, v) in voices.iter_mut().enumerate() {
            v.instance_id = i as u64 + 1;
            v.note = 60 + i as u8;
            v.frequency = 40.0 + i as f32;
            v.wave_speed = 2.0;
            v.source_pos = (0.5, 1.0);
            v.amplitude = 0.5;
        }
        let status = ServerStatus::OutputState {
            timestamp_us: 1,
            device_sample_rate: 48_000.0,
            sample_index: 1234,
            valid: true,
            analytic: [(0.25, -0.5); 32],
            count: MAX_ACTIVE_VOICES as u8,
            voices,
        };
        let mut buf = Vec::new();
        encode_frame(&status, &mut buf).unwrap();
        // A full OutputState frame must fit the framing budget.
        assert!(
            buf.len() <= MAX_FRAME_SIZE,
            "frame {} > max {}",
            buf.len(),
            MAX_FRAME_SIZE
        );
        let mut dec = FrameDecoder::new();
        dec.extend(&buf);
        match dec.next_frame::<ServerStatus>().unwrap().unwrap() {
            ServerStatus::OutputState {
                count,
                voices,
                analytic,
                ..
            } => {
                assert_eq!(count, MAX_ACTIVE_VOICES as u8);
                assert_eq!(voices[3].note, 63);
                assert_eq!(voices[3].instance_id, 4);
                assert_eq!(analytic[0], (0.25, -0.5));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn default_distance_decay_uses_two_metre_knee() {
        let decay = DistanceDecay::default();
        for distance in [0.0, 0.125, 0.5, 1.0, 2.25] {
            let expected = 1.0 / (1.0 + distance / 2.0);
            assert!((distance_gain(distance, decay) - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn effective_wavelength_uses_selected_representation() {
        assert!(
            (effective_wavelength(100.0, 20.0, SpatialScaleMode::Speed, 1.0) - 0.2).abs() < 1e-6
        );
        assert!(
            (effective_wavelength(50.0, 20.0, SpatialScaleMode::Wavelength, 0.125) - 0.125).abs()
                < 1e-6
        );
    }

    #[test]
    fn travelling_wave_phasor_has_expected_phase_and_gain() {
        let (re, im) = travelling_wave_relative_phasor(0.125, 0.5, DistanceDecay::default());
        assert!(re.abs() < 1e-6);
        assert!((im + 16.0 / 17.0).abs() < 1e-6);
    }
}
