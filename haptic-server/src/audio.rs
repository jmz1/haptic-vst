use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use crate::engine::{StimulusEngine, TRANSDUCER_COUNT};

/// Lock-free callback statistics, written by the audio thread and read by
/// the monitor thread. Durations land in log2(ns) histogram buckets so
/// percentiles can be estimated without allocation on the audio side.
pub struct AudioStats {
    callbacks: AtomicU64,
    frames: AtomicU64,
    max_ns: AtomicU64,
    stream_errors: AtomicU64,
    hist: [AtomicU64; 64],
}

impl AudioStats {
    fn new() -> Self {
        Self {
            callbacks: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            stream_errors: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn record(&self, elapsed_ns: u64, frames: u64) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.frames.fetch_add(frames, Ordering::Relaxed);
        self.max_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
        let bucket = (64 - elapsed_ns.leading_zeros() as usize).min(63);
        self.hist[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Estimate the given percentile (0..1) from the histogram; the upper
    /// edge of the bucket is reported, so values are conservative.
    fn percentile_ns(&self, p: f64) -> u64 {
        let counts: Vec<u64> = self.hist.iter().map(|b| b.load(Ordering::Relaxed)).collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0;
        }
        let target = ((total as f64) * p).ceil() as u64;
        let mut seen = 0;
        for (bucket, &count) in counts.iter().enumerate() {
            seen += count;
            if seen >= target {
                return 1u64 << bucket;
            }
        }
        u64::MAX
    }

    fn report(&self, since_last: &mut (u64, u64)) -> String {
        let callbacks = self.callbacks.load(Ordering::Relaxed);
        let frames = self.frames.load(Ordering::Relaxed);
        let (prev_callbacks, prev_frames) = *since_last;
        *since_last = (callbacks, frames);
        format!(
            "audio: {} callbacks (+{}), {} frames (+{}), cb time p50={}us p99={}us max={}us, stream errors={}",
            callbacks,
            callbacks - prev_callbacks,
            frames,
            frames - prev_frames,
            self.percentile_ns(0.50) / 1000,
            self.percentile_ns(0.99) / 1000,
            self.max_ns.load(Ordering::Relaxed) / 1000,
            self.stream_errors.load(Ordering::Relaxed),
        )
    }
}

/// Channel-cycling test pattern for interface bring-up: a short sine burst
/// walks across all output channels so each transducer can be identified.
struct TestTone {
    phase: f32,
    frames_into_burst: u32,
    current_channel: usize,
}

const TEST_TONE_FREQ: f32 = 100.0;
const TEST_TONE_BURST_SECS: f32 = 0.5;
const TEST_TONE_LEVEL: f32 = 0.5;

impl TestTone {
    fn new() -> Self {
        Self { phase: 0.0, frames_into_burst: 0, current_channel: 0 }
    }

    fn process_block(&mut self, data: &mut [f32], channels: usize, sample_rate: f32) {
        let burst_frames = (TEST_TONE_BURST_SECS * sample_rate) as u32;
        let active_channels = channels.min(TRANSDUCER_COUNT);
        for frame in data.chunks_exact_mut(channels) {
            frame.fill(0.0);
            // Short fade at the burst edges to avoid clicks
            let t = self.frames_into_burst as f32 / sample_rate;
            let remaining = (burst_frames - self.frames_into_burst) as f32 / sample_rate;
            let env = (t * 100.0).min(1.0).min((remaining * 100.0).min(1.0));
            frame[self.current_channel] =
                (self.phase * 2.0 * std::f32::consts::PI).sin() * TEST_TONE_LEVEL * env;

            self.phase += TEST_TONE_FREQ / sample_rate;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            self.frames_into_burst += 1;
            if self.frames_into_burst >= burst_frames {
                self.frames_into_burst = 0;
                self.current_channel = (self.current_channel + 1) % active_channels;
            }
        }
    }
}

pub fn run_audio_loop(
    engine: StimulusEngine,
    running: Arc<AtomicBool>,
    test_tone: bool,
    mut levels_producer: rtrb::Producer<[f32; TRANSDUCER_COUNT]>,
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
    if test_tone {
        eprintln!("TEST TONE mode: {} Hz bursts cycling across channels", TEST_TONE_FREQ);
    }

    let sample_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;

    let stats = Arc::new(AudioStats::new());
    let stats_for_callback = stats.clone();
    let stats_for_errors = stats.clone();

    // The engine is owned by the audio callback: no locks anywhere on the
    // audio path. Commands arrive through the rtrb ring buffer drained once
    // per callback inside process_block.
    let mut engine = engine;
    let mut tone = TestTone::new();

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let start = Instant::now();
            let mut levels = [0.0f32; TRANSDUCER_COUNT];
            if test_tone {
                tone.process_block(data, channels, sample_rate);
                // Per-device-channel RMS for the tone pattern
                let frames = (data.len() / channels).max(1);
                for frame in data.chunks_exact(channels) {
                    for (ch, &sample) in frame.iter().take(TRANSDUCER_COUNT).enumerate() {
                        levels[ch] += sample * sample;
                    }
                }
                for level in levels.iter_mut() {
                    *level = (*level / frames as f32).sqrt();
                }
            } else {
                engine.process_block(data, channels, sample_rate, &mut levels);
            }
            // Best-effort: dropped level frames are fine, freshness wins
            let _ = levels_producer.push(levels);
            let frames = (data.len() / channels) as u64;
            stats_for_callback.record(start.elapsed().as_nanos() as u64, frames);
        },
        move |err| {
            stats_for_errors.stream_errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("Audio stream error: {}", err);
        },
        None
    )?;

    stream.play()?;
    eprintln!("Audio stream started");

    // Monitor loop: report callback health every 5 seconds
    let mut since_last = (0u64, 0u64);
    let mut last_report = Instant::now();
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if last_report.elapsed().as_secs() >= 5 {
            eprintln!("{}", stats.report(&mut since_last));
            last_report = Instant::now();
        }
    }

    eprintln!("Audio stream stopping");
    Ok(())
}
