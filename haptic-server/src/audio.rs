use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::Mutex;
use crate::engine::StimulusEngine;

pub fn run_audio_loop(
    engine: StimulusEngine, 
    running: Arc<AtomicBool>
) -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    
    // Find device with 32+ channels
    let device = host.output_devices()?
        .find(|d| {
            if let Ok(mut configs) = d.supported_output_configs() {
                configs.any(|c| c.channels() >= 32)
            } else {
                false
            }
        })
        .unwrap_or_else(|| {
            // Fallback to default device for testing
            eprintln!("Warning: No 32-channel device found, using default device");
            host.default_output_device().expect("No output device available")
        });
    
    let mut config = device.default_output_config()?;
    
    // Try to set to 32 channels if supported
    if let Ok(supported_configs) = device.supported_output_configs() {
        for supported_config in supported_configs {
            if supported_config.channels() >= 32 {
                config = supported_config.with_max_sample_rate();
                break;
            }
        }
    }
    
    eprintln!("Using audio device: {}", device.name().unwrap_or_else(|_| "Unknown".to_string()));
    eprintln!("Sample rate: {} Hz", config.sample_rate().0);
    eprintln!("Channels: {}", config.channels());
    eprintln!("Buffer size: {:?}", config.buffer_size());
    
    let sample_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;
    
    // Wrap engine in thread-safe container
    let engine = Arc::new(Mutex::new(engine));
    let engine_clone = engine.clone();
    
    // Build output stream
    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let frames = data.len() / channels;
            
            for frame in 0..frames {
                let mut output = [0.0f32; 32];
                if let Some(mut engine_guard) = engine_clone.try_lock() {
                    engine_guard.process(&mut output, sample_rate);
                }
                
                // Copy to interleaved output, handling different channel counts
                for ch in 0..channels.min(32) {
                    let idx = frame * channels + ch;
                    if idx < data.len() {
                        data[idx] = output[ch];
                    }
                }
                
                // Fill remaining channels if device has more than 32
                for ch in 32..channels {
                    let idx = frame * channels + ch;
                    if idx < data.len() {
                        data[idx] = 0.0;
                    }
                }
            }
        },
        |err| eprintln!("Audio stream error: {}", err),
        None
    )?;
    
    stream.play()?;
    eprintln!("Audio stream started");
    
    // Keep alive until shutdown
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    
    eprintln!("Audio stream stopping");
    Ok(())
}