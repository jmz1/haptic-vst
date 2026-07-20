use nih_plug::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use parking_lot::Mutex;
use haptic_protocol::{HapticCommand, MpeData, Parameter, StimulusType};

mod ipc_client;
mod editor;

use ipc_client::IpcClient;

const MIDI_CHANNELS: usize = 16;
const CC_TIMBRE: u8 = 74; // MPE Y-axis / slide

#[derive(Enum, PartialEq, Clone, Copy)]
pub enum StimulusTypeParam {
    #[name = "Wave"]
    Wave,
    #[name = "Standing Wave"]
    Standing,
}

impl From<StimulusTypeParam> for StimulusType {
    fn from(value: StimulusTypeParam) -> Self {
        match value {
            StimulusTypeParam::Wave => StimulusType::Wave,
            StimulusTypeParam::Standing => StimulusType::Standing,
        }
    }
}

pub struct HapticPlugin {
    params: Arc<HapticParams>,
    ipc_client: Arc<Mutex<Option<IpcClient>>>,
    /// Last known MPE state per MIDI channel. Each incoming event updates
    /// one dimension; the full merged struct is sent so the server never
    /// sees defaults overwrite dimensions carried by other message types.
    mpe_state: [MpeData; MIDI_CHANNELS],
    // Last parameter values pushed to the server (None = never sent)
    last_sent_wave_speed: Option<f32>,
    last_sent_stimulus_type: Option<StimulusTypeParam>,
}

#[derive(Params)]
pub struct HapticParams {
    #[id = "wave_speed"]
    pub wave_speed: FloatParam,
    #[id = "stim_type"]
    pub stimulus_type: EnumParam<StimulusTypeParam>,
}

impl Default for HapticPlugin {
    fn default() -> Self {
        nih_log!("Creating new HapticPlugin instance");
        Self {
            params: Arc::new(HapticParams::default()),
            ipc_client: Arc::new(Mutex::new(None)),
            mpe_state: [MpeData::default(); MIDI_CHANNELS],
            last_sent_wave_speed: None,
            last_sent_stimulus_type: None,
        }
    }
}

impl Default for HapticParams {
    fn default() -> Self {
        Self {
            wave_speed: FloatParam::new(
                "Wave Speed",
                100.0,
                FloatRange::Skewed { min: 20.0, max: 500.0, factor: FloatRange::skew_factor(-1.0) },
            )
            .with_unit(" m/s")
            .with_step_size(1.0),
            stimulus_type: EnumParam::new("Stimulus Type", StimulusTypeParam::Wave),
        }
    }
}

fn send_or_log(client: &IpcClient, cmd: HapticCommand) -> bool {
    match client.send_command(cmd) {
        Ok(()) => true,
        Err(e) => {
            nih_log!("Failed to send command to haptic server: {}", e);
            false
        }
    }
}

impl Plugin for HapticPlugin {
    const NAME: &'static str = "Haptic Controller";
    const VENDOR: &'static str = "Haptic Research";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = "0.1.0";

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: None,
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(&mut self, audio_io_layout: &AudioIOLayout, buffer_config: &BufferConfig, _context: &mut impl InitContext<Self>) -> bool {
        nih_log!("Initializing HapticPlugin");
        nih_log!("Audio layout: main_output_channels={:?}", audio_io_layout.main_output_channels);
        nih_log!("Buffer config: sample_rate={}, max_buffer_size={}", buffer_config.sample_rate, buffer_config.max_buffer_size);

        // Try to connect to IPC server
        match IpcClient::connect() {
            Ok(client) => {
                *self.ipc_client.lock() = Some(client);
                nih_log!("Successfully connected to haptic server");
            }
            Err(e) => {
                nih_log!("Failed to connect to haptic server: {}", e);
                nih_log!("Plugin will continue without server connection");
            }
        }

        // Force a parameter push on the first process() after (re)connecting
        self.last_sent_wave_speed = None;
        self.last_sent_stimulus_type = None;

        nih_log!("HapticPlugin initialization complete");
        true
    }

    fn process(&mut self, buffer: &mut Buffer, _aux: &mut AuxiliaryBuffers, context: &mut impl ProcessContext<Self>) -> ProcessStatus {
        // Get current timestamp in microseconds
        let base_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;

        let client_guard = self.ipc_client.lock();
        if let Some(ref client) = *client_guard {
            // Push parameter changes (and initial values) to the server
            let wave_speed = self.params.wave_speed.value();
            if self.last_sent_wave_speed != Some(wave_speed) {
                let cmd = HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::WaveSpeed(wave_speed),
                };
                if send_or_log(client, cmd) {
                    self.last_sent_wave_speed = Some(wave_speed);
                }
            }
            let stimulus_type = self.params.stimulus_type.value();
            if self.last_sent_stimulus_type != Some(stimulus_type) {
                let cmd = HapticCommand::SetParameter {
                    timestamp_us: base_timestamp,
                    parameter: Parameter::StimulusType(stimulus_type.into()),
                };
                if send_or_log(client, cmd) {
                    self.last_sent_stimulus_type = Some(stimulus_type);
                }
            }

            // Process MIDI events, merging each into the per-channel MPE cache
            while let Some(event) = context.next_event() {
                let timing_offset = event.timing() as u64 * 1000; // Convert samples to microseconds (rough estimate)
                let timestamp_us = base_timestamp + timing_offset;

                match event {
                    NoteEvent::NoteOn { note, velocity, channel, .. } => {
                        let ch = (channel as usize) % MIDI_CHANNELS;
                        // Strike velocity seeds pressure so non-MPE keyboards
                        // (which never send pressure) still produce output.
                        self.mpe_state[ch].pressure = velocity;
                        nih_log!("NoteOn: note={}, velocity={:.3}, channel={}, mpe={:?}", note, velocity, channel, self.mpe_state[ch]);
                        send_or_log(client, HapticCommand::NoteOn {
                            timestamp_us,
                            note,
                            velocity: (velocity * 127.0) as u8,
                            channel: channel as u8,
                            mpe: self.mpe_state[ch],
                        });
                    }
                    NoteEvent::NoteOff { note, channel, .. } => {
                        nih_log!("NoteOff: note={}, channel={}", note, channel);
                        send_or_log(client, HapticCommand::NoteOff {
                            timestamp_us,
                            note,
                            channel: channel as u8,
                        });
                    }
                    NoteEvent::PolyPressure { pressure, channel, .. }
                    | NoteEvent::MidiChannelPressure { pressure, channel, .. } => {
                        let ch = (channel as usize) % MIDI_CHANNELS;
                        self.mpe_state[ch].pressure = pressure;
                        send_or_log(client, HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: self.mpe_state[ch],
                        });
                    }
                    NoteEvent::MidiPitchBend { channel, value, .. } => {
                        let ch = (channel as usize) % MIDI_CHANNELS;
                        // nih-plug pitch bend is [0, 1] with 0.5 centered;
                        // protocol wants [-1, 1]
                        self.mpe_state[ch].pitch_bend = value * 2.0 - 1.0;
                        send_or_log(client, HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: self.mpe_state[ch],
                        });
                    }
                    NoteEvent::MidiCC { channel, cc, value, .. } if cc == CC_TIMBRE => {
                        let ch = (channel as usize) % MIDI_CHANNELS;
                        self.mpe_state[ch].timbre = value;
                        send_or_log(client, HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: self.mpe_state[ch],
                        });
                    }
                    _ => {}
                }
            }
        } else {
            // Log when no IPC client is available, but only occasionally to avoid spam
            static LOG_COUNTER: AtomicU32 = AtomicU32::new(0);
            if LOG_COUNTER.fetch_add(1, Ordering::Relaxed) % 4800 == 0 {
                nih_log!("No IPC client available for MIDI processing (logged occasionally)");
            }
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
        let params = self.params.clone();
        let ipc_client = self.ipc_client.clone();
        editor::create(params, ipc_client)
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
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth];
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
    nih_log!("Plugin vendor: {}", HapticPlugin::VENDOR);
    nih_log!("Log output: {}", std::env::var("NIH_LOG").unwrap_or_else(|_| "STDERR".to_string()));
}
