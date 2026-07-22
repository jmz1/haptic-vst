use haptic_protocol::{
    DistanceDecay, HapticCommand, InstanceConfig, MpeData, Parameter, SpatialScaleMode,
    StimulusType, TravellingWaveConfig,
};
use nih_plug::prelude::*;
use std::sync::Arc;

mod editor;
mod ipc_client;

use ipc_client::{Diagnostics, IpcClient};

const MIDI_CHANNELS: usize = 16;
const CC_TIMBRE: u8 = 74; // MPE Y-axis / slide
pub const BUILD_HASH: &str = env!("HAPTIC_BUILD_HASH");

#[derive(Enum, PartialEq, Clone, Copy)]
pub enum StimulusTypeParam {
    #[name = "Wave"]
    Wave,
    #[name = "Travelling Wave (TW)"]
    TravellingWave,
}

impl From<StimulusTypeParam> for StimulusType {
    fn from(value: StimulusTypeParam) -> Self {
        match value {
            StimulusTypeParam::Wave => StimulusType::Wave,
            StimulusTypeParam::TravellingWave => StimulusType::TravellingWave,
        }
    }
}

#[derive(Enum, PartialEq, Clone, Copy)]
pub enum SpatialScaleModeParam {
    #[name = "Speed"]
    Speed,
    #[name = "Fixed wavelength"]
    Wavelength,
}

impl From<SpatialScaleModeParam> for SpatialScaleMode {
    fn from(value: SpatialScaleModeParam) -> Self {
        match value {
            SpatialScaleModeParam::Speed => SpatialScaleMode::Speed,
            SpatialScaleModeParam::Wavelength => SpatialScaleMode::Wavelength,
        }
    }
}

pub struct HapticPlugin {
    params: Arc<HapticParams>,
    /// Reconnecting, write-only IPC client. Always present; its manager thread
    /// keeps the connection up in the background.
    ipc_client: Arc<IpcClient>,
    /// Live diagnostics shared with the editor (incoming MIDI, send failures,
    /// connection generation).
    diag: Arc<Diagnostics>,
    /// Stable identity for this plugin instance, generated once at
    /// construction and sent in the `Hello` handshake. The server keys this
    /// instance's note-type config and voice identity on it, so concurrent
    /// instances never contend over a shared global or collide on notes.
    instance_id: u64,
    /// Last known MPE state per MIDI channel. Each incoming event updates
    /// one dimension; the full merged struct is sent so the server never
    /// sees defaults overwrite dimensions carried by other message types.
    mpe_state: [MpeData; MIDI_CHANNELS],
    // Last parameter values pushed to the server (None = never sent)
    last_sent_wave_speed: Option<f32>,
    last_sent_stimulus_type: Option<StimulusTypeParam>,
    last_sent_scale_mode: Option<SpatialScaleModeParam>,
    last_sent_wavelength: Option<f32>,
    last_sent_atten_d0: Option<f32>,
    last_sent_atten_exponent: Option<f32>,
}

/// A process-unique, non-zero instance id (0 is the server's default-instance
/// fallback). Combines wall-clock nanos, the pid, and a per-process counter so
/// several instances constructed in the same host process stay distinct.
fn new_instance_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((std::process::id() as u64) << 32);
    if id == 0 {
        1
    } else {
        id
    }
}

#[derive(Params)]
pub struct HapticParams {
    #[id = "wave_speed"]
    pub wave_speed: FloatParam,
    #[id = "stim_type"]
    pub stimulus_type: EnumParam<StimulusTypeParam>,
    #[id = "tw_scale_mode"]
    pub tw_scale_mode: EnumParam<SpatialScaleModeParam>,
    #[id = "tw_wavelength"]
    pub tw_wavelength: FloatParam,
    #[id = "atten_d0"]
    pub atten_d0: FloatParam,
    #[id = "atten_p"]
    pub atten_exponent: FloatParam,
}

impl Default for HapticPlugin {
    fn default() -> Self {
        nih_log!("Creating new HapticPlugin instance (build {})", BUILD_HASH);
        let params = Arc::new(HapticParams::default());
        let instance_id = new_instance_id();
        let diag = Arc::new(Diagnostics::new(instance_id));
        let initial_config = InstanceConfig {
            stimulus_type: params.stimulus_type.value().into(),
            wave_speed: params.wave_speed.value(),
            travelling_wave: TravellingWaveConfig {
                scale_mode: params.tw_scale_mode.value().into(),
                wave_speed: params.wave_speed.value(),
                wavelength_m: params.tw_wavelength.value(),
            },
            distance_decay: DistanceDecay {
                d0_m: params.atten_d0.value(),
                exponent: params.atten_exponent.value(),
            },
        };
        let ipc_client = Arc::new(IpcClient::spawn(instance_id, initial_config, diag.clone()));
        Self {
            params,
            ipc_client,
            diag,
            instance_id,
            mpe_state: [MpeData::default(); MIDI_CHANNELS],
            last_sent_wave_speed: None,
            last_sent_stimulus_type: None,
            last_sent_scale_mode: None,
            last_sent_wavelength: None,
            last_sent_atten_d0: None,
            last_sent_atten_exponent: None,
        }
    }
}

impl Default for HapticParams {
    fn default() -> Self {
        Self {
            wave_speed: FloatParam::new(
                "Wave Speed",
                20.0,
                // Match the engine's usable range (MIN_WAVE_SPEED..MAX_WAVE_SPEED).
                // Heavily skewed toward the low end: the strong-Doppler regime the
                // system is built for lives below ~20 m/s, and was previously
                // unreachable from the host (old floor was 20 m/s).
                FloatRange::Skewed {
                    min: 0.25,
                    max: 1000.0,
                    factor: FloatRange::skew_factor(-2.5),
                },
            )
            .with_unit(" m/s")
            .with_step_size(0.01),
            stimulus_type: EnumParam::new("Stimulus Type", StimulusTypeParam::Wave),
            tw_scale_mode: EnumParam::new("TW Scale Mode", SpatialScaleModeParam::Speed),
            tw_wavelength: FloatParam::new(
                "TW Wavelength",
                haptic_protocol::DEFAULT_WAVELENGTH_M,
                FloatRange::Skewed {
                    min: haptic_protocol::MIN_WAVELENGTH_M,
                    max: haptic_protocol::MAX_WAVELENGTH_M,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_unit(" m")
            .with_step_size(0.0001),
            atten_d0: FloatParam::new(
                "Attenuation Knee",
                haptic_protocol::DEFAULT_ATTEN_D0_M,
                FloatRange::Skewed {
                    min: haptic_protocol::MIN_ATTEN_D0_M,
                    max: haptic_protocol::MAX_ATTEN_D0_M,
                    factor: FloatRange::skew_factor(-1.5),
                },
            )
            .with_unit(" m")
            .with_step_size(0.001),
            atten_exponent: FloatParam::new(
                "Attenuation Exponent",
                haptic_protocol::DEFAULT_ATTEN_EXPONENT,
                FloatRange::Linear {
                    min: haptic_protocol::MIN_ATTEN_EXPONENT,
                    max: haptic_protocol::MAX_ATTEN_EXPONENT,
                },
            )
            .with_step_size(0.01),
        }
    }
}

impl Plugin for HapticPlugin {
    const NAME: &'static str = "Haptic Controller";
    const VENDOR: &'static str = "Haptic Research";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.1.0";

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        nih_log!("Initializing HapticPlugin (instance {})", self.instance_id);
        nih_log!(
            "Client build: {}, protocol: {}",
            BUILD_HASH,
            haptic_protocol::PROTOCOL_VERSION
        );
        nih_log!(
            "Audio layout: main_output_channels={:?}",
            audio_io_layout.main_output_channels
        );
        nih_log!(
            "Buffer config: sample_rate={}, max_buffer_size={}",
            buffer_config.sample_rate,
            buffer_config.max_buffer_size
        );

        // The reconnecting IPC manager (spawned in Default) owns the connection
        // and re-handshakes on every (re)connect; nothing to do here but force
        // a fresh parameter push on the next process().
        self.last_sent_wave_speed = None;
        self.last_sent_stimulus_type = None;
        self.last_sent_scale_mode = None;
        self.last_sent_wavelength = None;
        self.last_sent_atten_d0 = None;
        self.last_sent_atten_exponent = None;
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Commands are currently applied at the next server callback boundary.
        // Avoid a wall-clock syscall here; sample-accurate scheduling needs an
        // explicit transport-time design rather than approximate epoch times.
        let base_timestamp = 0;
        let client = &self.ipc_client;

        // Parameter changes patch this instance's config: a live SetParameter
        // (applied immediately by the server) and an update to the config the
        // manager re-sends in Hello on the next reconnect.
        let wave_speed = self.params.wave_speed.value();
        let stimulus_type = self.params.stimulus_type.value();
        let scale_mode = self.params.tw_scale_mode.value();
        let wavelength = self.params.tw_wavelength.value();
        let atten_d0 = self.params.atten_d0.value();
        let atten_exponent = self.params.atten_exponent.value();
        if self.last_sent_wave_speed != Some(wave_speed)
            || self.last_sent_stimulus_type != Some(stimulus_type)
            || self.last_sent_scale_mode != Some(scale_mode)
            || self.last_sent_wavelength != Some(wavelength)
            || self.last_sent_atten_d0 != Some(atten_d0)
            || self.last_sent_atten_exponent != Some(atten_exponent)
        {
            client.set_config(InstanceConfig {
                stimulus_type: stimulus_type.into(),
                wave_speed,
                travelling_wave: TravellingWaveConfig {
                    scale_mode: scale_mode.into(),
                    wave_speed,
                    wavelength_m: wavelength,
                },
                distance_decay: DistanceDecay {
                    d0_m: atten_d0,
                    exponent: atten_exponent,
                },
            });
        }
        if self.last_sent_wave_speed != Some(wave_speed)
            && client
                .send_command(HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::WaveSpeed(wave_speed),
                })
                .is_ok()
        {
            self.last_sent_wave_speed = Some(wave_speed);
        }
        if self.last_sent_stimulus_type != Some(stimulus_type)
            && client
                .send_command(HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::StimulusType(stimulus_type.into()),
                })
                .is_ok()
        {
            self.last_sent_stimulus_type = Some(stimulus_type);
        }
        if self.last_sent_scale_mode != Some(scale_mode)
            && client
                .send_command(HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::TravellingWaveScaleMode(scale_mode.into()),
                })
                .is_ok()
        {
            self.last_sent_scale_mode = Some(scale_mode);
        }
        if self.last_sent_wavelength != Some(wavelength)
            && client
                .send_command(HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::TravellingWaveWavelength(wavelength),
                })
                .is_ok()
        {
            self.last_sent_wavelength = Some(wavelength);
        }
        if self.last_sent_atten_d0 != Some(atten_d0)
            && client
                .send_command(HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::AttenuationD0(atten_d0),
                })
                .is_ok()
        {
            self.last_sent_atten_d0 = Some(atten_d0);
        }
        if self.last_sent_atten_exponent != Some(atten_exponent)
            && client
                .send_command(HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::AttenuationExponent(atten_exponent),
                })
                .is_ok()
        {
            self.last_sent_atten_exponent = Some(atten_exponent);
        }

        // Process MIDI events, merging each into the per-channel MPE cache.
        // Diagnostics are published with relaxed atomics once per block.
        let mut on = 0u64;
        let mut off = 0u64;
        let mut mpe = 0u64;
        let mut dropped = 0u64;
        while let Some(event) = context.next_event() {
            let timestamp_us = base_timestamp;

            match event {
                NoteEvent::NoteOn {
                    note,
                    velocity,
                    channel,
                    ..
                } => {
                    let ch = (channel as usize) % MIDI_CHANNELS;
                    // Velocity already controls source amplitude in the engine.
                    // Seed pressure at unity so a non-MPE keyboard is linear in
                    // velocity rather than unintentionally velocity-squared.
                    self.mpe_state[ch].pressure = 1.0;
                    let ok = client
                        .send_command(HapticCommand::NoteOn {
                            timestamp_us,
                            note,
                            velocity: if velocity.is_finite() {
                                (velocity.clamp(0.0, 1.0) * 127.0).round() as u8
                            } else {
                                0
                            },
                            channel,
                            mpe: self.mpe_state[ch],
                        })
                        .is_ok();
                    on += 1;
                    if !ok {
                        dropped += 1;
                    }
                }
                NoteEvent::NoteOff { note, channel, .. } => {
                    let ok = client
                        .send_command(HapticCommand::NoteOff {
                            timestamp_us,
                            note,
                            channel,
                        })
                        .is_ok();
                    off += 1;
                    if !ok {
                        dropped += 1;
                    }
                }
                NoteEvent::PolyPressure {
                    pressure, channel, ..
                }
                | NoteEvent::MidiChannelPressure {
                    pressure, channel, ..
                } => {
                    let ch = (channel as usize) % MIDI_CHANNELS;
                    if pressure.is_finite() {
                        self.mpe_state[ch].pressure = pressure.clamp(0.0, 1.0);
                    }
                    if client
                        .send_command(HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel,
                            mpe: self.mpe_state[ch],
                        })
                        .is_err()
                    {
                        dropped += 1;
                    }
                    mpe += 1;
                }
                NoteEvent::MidiPitchBend { channel, value, .. } => {
                    let ch = (channel as usize) % MIDI_CHANNELS;
                    // nih-plug pitch bend is [0, 1] with 0.5 centered; protocol wants [-1, 1]
                    if value.is_finite() {
                        self.mpe_state[ch].pitch_bend = (value * 2.0 - 1.0).clamp(-1.0, 1.0);
                    }
                    if client
                        .send_command(HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel,
                            mpe: self.mpe_state[ch],
                        })
                        .is_err()
                    {
                        dropped += 1;
                    }
                    mpe += 1;
                }
                NoteEvent::MidiCC {
                    channel, cc, value, ..
                } if cc == CC_TIMBRE => {
                    let ch = (channel as usize) % MIDI_CHANNELS;
                    if value.is_finite() {
                        self.mpe_state[ch].timbre = value.clamp(0.0, 1.0);
                    }
                    if client
                        .send_command(HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel,
                            mpe: self.mpe_state[ch],
                        })
                        .is_err()
                    {
                        dropped += 1;
                    }
                    mpe += 1;
                }
                _ => {}
            }
        }
        if on + off + mpe + dropped > 0 {
            self.diag.record(on, off, mpe, dropped);
        }

        // Clear audio output (we don't generate audio)
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                *sample = 0.0;
            }
        }

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(
            self.params.clone(),
            self.ipc_client.clone(),
            self.diag.clone(),
        )
    }
}

// impl ClapPlugin for HapticPlugin {
//     const CLAP_ID: &'static str = "com.haptic-research.haptic-vst";
//     const CLAP_DESCRIPTION: Option<&'static str> = Some("32-channel haptic stimulus controller");
//     const CLAP_MANUAL_URL: Option<&'static str> = None;
//     const CLAP_SUPPORT_URL: Option<&'static str> = None;
//     const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Synthesizer];
// }

impl Vst3Plugin for HapticPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"HapticStimCtrl01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth];
}

// Plugin exports with logging
// nih_export_clap!(HapticPlugin);
nih_export_vst3!(HapticPlugin);

// Initialize logging on library load
#[ctor::ctor]
fn init_logging() {
    // Set default log location if NIH_LOG is not set
    if std::env::var("NIH_LOG").is_err() {
        std::env::set_var("NIH_LOG", "/Users/jmz/tmp/log/haptic-vst.log");
    }

    nih_log!("Haptic VST plugin library loaded");
    nih_log!("Plugin version: {}", HapticPlugin::VERSION);
    nih_log!(
        "Client build: {}, protocol: {}",
        BUILD_HASH,
        haptic_protocol::PROTOCOL_VERSION
    );
    nih_log!("Plugin vendor: {}", HapticPlugin::VENDOR);
    nih_log!(
        "Log output: {}",
        std::env::var("NIH_LOG").unwrap_or_else(|_| "STDERR".to_string())
    );
}
