use nih_plug::prelude::*;
use std::sync::Arc;
use parking_lot::Mutex;
use haptic_protocol::{HapticCommand, MpeData};

mod ipc_client;
mod editor;

use ipc_client::IpcClient;

pub struct HapticPlugin {
    params: Arc<HapticParams>,
    ipc_client: Arc<Mutex<Option<IpcClient>>>,
}

#[derive(Params)]
struct HapticParams {
    // Placeholder for future plugin parameters
    // Wave speed is now calculated on the server from note velocity
}

impl Default for HapticPlugin {
    fn default() -> Self {
        nih_log!("Creating new HapticPlugin instance");
        Self {
            params: Arc::new(HapticParams::default()),
            ipc_client: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for HapticParams {
    fn default() -> Self {
        Self {
            // No parameters currently - wave speed is calculated on server
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
        nih_log!("Attempting to connect to haptic server");
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
        
        nih_log!("HapticPlugin initialization complete");
        true
    }
    
    fn process(&mut self, buffer: &mut Buffer, _aux: &mut AuxiliaryBuffers, context: &mut impl ProcessContext<Self>) -> ProcessStatus {
        // Get current timestamp in microseconds
        let base_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        
        // Get IPC client
        let client_guard = self.ipc_client.lock();
        if let Some(ref client) = *client_guard {
            // Process MIDI events
            let mut event_count = 0;
            while let Some(event) = context.next_event() {
                event_count += 1;
                let timing_offset = event.timing() as u64 * 1000; // Convert samples to microseconds (rough estimate)
                let timestamp_us = base_timestamp + timing_offset;
                
                nih_log!("Processing MIDI event #{}: {:?} at timing offset {}", event_count, event, event.timing());
                
                match event {
                    NoteEvent::NoteOn { note, velocity, channel, .. } => {
                        nih_log!("NoteOn: note={}, velocity={:.3}, channel={}", note, velocity, channel);
                        let cmd = HapticCommand::NoteOn {
                            timestamp_us,
                            note,
                            velocity: (velocity * 127.0) as u8,
                            channel: channel as u8,
                            mpe: MpeData { 
                                pressure: velocity, 
                                pitch_bend: 0.0, 
                                timbre: 0.5 
                            },
                        };
                        nih_log!("Sending HapticCommand::NoteOn with velocity_u8={}, timestamp={}", (velocity * 127.0) as u8, timestamp_us);
                        if let Err(e) = client.send_command(cmd) {
                            nih_log!("Failed to send NoteOn command: {}", e);
                        } else {
                            nih_log!("Successfully sent NoteOn command to haptic server");
                        }
                    }
                    NoteEvent::NoteOff { note, channel, .. } => {
                        nih_log!("NoteOff: note={}, channel={}", note, channel);
                        let cmd = HapticCommand::NoteOff {
                            timestamp_us,
                            note,
                            channel: channel as u8,
                        };
                        nih_log!("Sending HapticCommand::NoteOff with timestamp={}", timestamp_us);
                        if let Err(e) = client.send_command(cmd) {
                            nih_log!("Failed to send NoteOff command: {}", e);
                        } else {
                            nih_log!("Successfully sent NoteOff command to haptic server");
                        }
                    }
                    NoteEvent::PolyPressure { pressure, channel, .. } => {
                        nih_log!("PolyPressure: pressure={:.3}, channel={}", pressure, channel);
                        let cmd = HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: MpeData {
                                pressure,
                                pitch_bend: 0.0, // TODO: Track pitch bend separately
                                timbre: 0.5,     // TODO: Track timbre separately
                            },
                        };
                        nih_log!("Sending HapticCommand::MpeUpdate (pressure) with timestamp={}", timestamp_us);
                        if let Err(e) = client.send_command(cmd) {
                            nih_log!("Failed to send MpeUpdate (pressure) command: {}", e);
                        } else {
                            nih_log!("Successfully sent MpeUpdate (pressure) command to haptic server");
                        }
                    }
                    NoteEvent::MidiPitchBend { channel, value, .. } => {
                        nih_log!("MidiPitchBend: value={:.3}, channel={}", value, channel);
                        let cmd = HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: MpeData {
                                pressure: 1.0,   // TODO: Track pressure separately
                                pitch_bend: value,
                                timbre: 0.5,     // TODO: Track timbre separately
                            },
                        };
                        nih_log!("Sending HapticCommand::MpeUpdate (pitch bend) with timestamp={}", timestamp_us);
                        if let Err(e) = client.send_command(cmd) {
                            nih_log!("Failed to send MpeUpdate (pitch bend) command: {}", e);
                        } else {
                            nih_log!("Successfully sent MpeUpdate (pitch bend) command to haptic server");
                        }
                    }
                    _ => {
                        nih_log!("Unhandled MIDI event: {:?}", event);
                    }
                }
            }
            
            if event_count > 0 {
                nih_log!("Processed {} MIDI events in this buffer", event_count);
            }
            
            // Note: Wave speed is now calculated on the server from note velocity
            // No plugin parameters to process currently
        } else {
            // Log when no IPC client is available but only occasionally to avoid spam
            static mut LOG_COUNTER: u32 = 0;
            unsafe {
                LOG_COUNTER += 1;
                if LOG_COUNTER % 4800 == 0 { // Log every ~100ms at 48kHz
                    nih_log!("No IPC client available for MIDI processing (logged every ~100ms)");
                }
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

impl ClapPlugin for HapticPlugin {
    const CLAP_ID: &'static str = "com.haptic-research.haptic-vst";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("32-channel haptic stimulus controller");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Synthesizer];
}

impl Vst3Plugin for HapticPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"HapticStimCtrl01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth];
}

// Plugin exports with logging
nih_export_clap!(HapticPlugin);
nih_export_vst3!(HapticPlugin);

// Initialize logging on library load
#[ctor::ctor]
fn init_logging() {
    // Set default log location if NIH_LOG is not set
    if std::env::var("NIH_LOG").is_err() {
        std::env::set_var("NIH_LOG", "~/tmp/log/haptic-vst.log");
    }
    
    nih_log!("Haptic VST plugin library loaded");
    nih_log!("Plugin version: {}", HapticPlugin::VERSION);
    nih_log!("Plugin vendor: {}", HapticPlugin::VENDOR);
    nih_log!("Log output: {}", std::env::var("NIH_LOG").unwrap_or_else(|_| "STDERR".to_string()));
}