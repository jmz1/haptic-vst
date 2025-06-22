use nih_plug::prelude::*;
use std::sync::Arc;
use parking_lot::Mutex;
use haptic_protocol::{HapticCommand, MpeData};

mod ipc_client;
mod editor;

use ipc_client::IpcClient;

struct HapticPlugin {
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
    
    type SysExMessage = ();
    type BackgroundTask = ();
    
    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }
    
    fn initialize(&mut self, _audio_io_layout: &AudioIOLayout, _buffer_config: &BufferConfig, _context: &mut impl InitContext<Self>) -> bool {
        // Try to connect to IPC server
        match IpcClient::connect() {
            Ok(client) => {
                *self.ipc_client.lock() = Some(client);
                nih_log!("Connected to haptic server");
            }
            Err(e) => {
                nih_log!("Failed to connect to haptic server: {}", e);
            }
        }
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
            while let Some(event) = context.next_event() {
                let timing_offset = event.timing() as u64 * 1000; // Convert samples to microseconds (rough estimate)
                let timestamp_us = base_timestamp + timing_offset;
                
                match event {
                    NoteEvent::NoteOn { note, velocity, channel, .. } => {
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
                        let _ = client.send_command(cmd);
                    }
                    NoteEvent::NoteOff { note, channel, .. } => {
                        let cmd = HapticCommand::NoteOff {
                            timestamp_us,
                            note,
                            channel: channel as u8,
                        };
                        let _ = client.send_command(cmd);
                    }
                    NoteEvent::PolyPressure { pressure, channel, .. } => {
                        let cmd = HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: MpeData {
                                pressure,
                                pitch_bend: 0.0, // TODO: Track pitch bend separately
                                timbre: 0.5,     // TODO: Track timbre separately
                            },
                        };
                        let _ = client.send_command(cmd);
                    }
                    NoteEvent::MidiPitchBend { channel, value, .. } => {
                        let cmd = HapticCommand::MpeUpdate {
                            timestamp_us,
                            channel: channel as u8,
                            mpe: MpeData {
                                pressure: 1.0,   // TODO: Track pressure separately
                                pitch_bend: value,
                                timbre: 0.5,     // TODO: Track timbre separately
                            },
                        };
                        let _ = client.send_command(cmd);
                    }
                    _ => {}
                }
            }
            
            // Note: Wave speed is now calculated on the server from note velocity
            // No plugin parameters to process currently
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

nih_export_clap!(HapticPlugin);
nih_export_vst3!(HapticPlugin);