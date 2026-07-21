use crate::engine::{StimulusEngine, TRANSDUCER_COUNT};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, SupportedStreamConfig, SupportedStreamConfigRange};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

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
        let counts: Vec<u64> = self
            .hist
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
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
const PREFERRED_SAMPLE_RATE: u32 = 48_000;

/// Pick the supported f32 configuration closest to 48 kHz. For a device with
/// 32+ channel support, retain that requirement; for a fallback device, retain
/// its default channel count rather than unexpectedly opening a different
/// speaker layout.
fn preferred_output_config(
    ranges: impl IntoIterator<Item = SupportedStreamConfigRange>,
    default_channels: u16,
    require_multichannel: bool,
) -> Option<SupportedStreamConfig> {
    ranges
        .into_iter()
        .filter(|range| range.sample_format() == SampleFormat::F32)
        .filter(|range| {
            if require_multichannel {
                range.channels() >= TRANSDUCER_COUNT as u16
            } else {
                range.channels() == default_channels
            }
        })
        .map(|range| {
            let rate =
                PREFERRED_SAMPLE_RATE.clamp(range.min_sample_rate().0, range.max_sample_rate().0);
            let distance = rate.abs_diff(PREFERRED_SAMPLE_RATE);
            let channels = range.channels();
            (distance, channels, range.with_sample_rate(SampleRate(rate)))
        })
        .min_by_key(|(distance, channels, _)| (*distance, *channels))
        .map(|(_, _, config)| config)
}

impl TestTone {
    fn new() -> Self {
        Self {
            phase: 0.0,
            frames_into_burst: 0,
            current_channel: 0,
        }
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
    device_channels: Arc<AtomicU16>,
) -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();

    // Find device with 32+ channels
    let device = host
        .output_devices()?
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
            host.default_output_device()
                .expect("No output device available")
        });

    let default_config = device.default_output_config()?;
    let has_multichannel = device
        .supported_output_configs()
        .map(|mut configs| configs.any(|config| config.channels() >= TRANSDUCER_COUNT as u16))
        .unwrap_or(false);
    let config = device
        .supported_output_configs()
        .ok()
        .and_then(|configs| {
            preferred_output_config(configs, default_config.channels(), has_multichannel)
        })
        .unwrap_or(default_config);

    if config.sample_format() != SampleFormat::F32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "selected output configuration uses {:?} samples; f32 is required",
                config.sample_format()
            ),
        )
        .into());
    }

    eprintln!(
        "Using audio device: {}",
        device.name().unwrap_or_else(|_| "Unknown".to_string())
    );
    eprintln!("Sample rate: {} Hz", config.sample_rate().0);
    if config.sample_rate().0 != PREFERRED_SAMPLE_RATE {
        eprintln!(
            "Warning: device does not expose 48 kHz for the selected channel layout; using {} Hz",
            config.sample_rate().0
        );
    }
    eprintln!("Channels: {}", config.channels());
    eprintln!("Buffer size: {:?}", config.buffer_size());
    if test_tone {
        eprintln!(
            "TEST TONE mode: {} Hz bursts cycling across channels",
            TEST_TONE_FREQ
        );
    }

    let sample_rate = config.sample_rate().0 as f32;
    let channels = config.channels() as usize;
    device_channels.store(config.channels(), Ordering::Relaxed);

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
            stats_for_errors
                .stream_errors
                .fetch_add(1, Ordering::Relaxed);
            eprintln!("Audio stream error: {}", err);
        },
        None,
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

/// Run the complete engine on a wall-clocked in-memory 32-channel sink. This
/// exercises command handling, DSP, levels, and voice snapshots without
/// opening or locking any physical audio device.
pub fn run_dummy_audio_loop(
    engine: StimulusEngine,
    running: Arc<AtomicBool>,
    test_tone: bool,
    mut levels_producer: rtrb::Producer<[f32; TRANSDUCER_COUNT]>,
    device_channels: Arc<AtomicU16>,
) {
    const DUMMY_CHANNELS: usize = TRANSDUCER_COUNT;
    const DUMMY_BLOCK_FRAMES: usize = 512;

    let sample_rate = PREFERRED_SAMPLE_RATE as f32;
    let block_period = std::time::Duration::from_secs_f64(
        DUMMY_BLOCK_FRAMES as f64 / PREFERRED_SAMPLE_RATE as f64,
    );
    let mut next_block = Instant::now();
    let mut data = vec![0.0f32; DUMMY_BLOCK_FRAMES * DUMMY_CHANNELS];
    let mut engine = engine;
    let mut tone = TestTone::new();
    let stats = AudioStats::new();
    let mut since_last = (0u64, 0u64);
    let mut last_report = Instant::now();

    device_channels.store(DUMMY_CHANNELS as u16, Ordering::Relaxed);
    eprintln!(
        "Dummy audio started: {} Hz, {} channels, {} frames/block",
        PREFERRED_SAMPLE_RATE, DUMMY_CHANNELS, DUMMY_BLOCK_FRAMES
    );

    while running.load(Ordering::Relaxed) {
        let started = Instant::now();
        let mut levels = [0.0f32; TRANSDUCER_COUNT];
        if test_tone {
            tone.process_block(&mut data, DUMMY_CHANNELS, sample_rate);
            for frame in data.chunks_exact(DUMMY_CHANNELS) {
                for (channel, &sample) in frame.iter().enumerate() {
                    levels[channel] += sample * sample;
                }
            }
            for level in &mut levels {
                *level = (*level / DUMMY_BLOCK_FRAMES as f32).sqrt();
            }
        } else {
            engine.process_block(&mut data, DUMMY_CHANNELS, sample_rate, &mut levels);
        }
        let _ = levels_producer.push(levels);
        stats.record(
            started.elapsed().as_nanos() as u64,
            DUMMY_BLOCK_FRAMES as u64,
        );

        if last_report.elapsed().as_secs() >= 5 {
            eprintln!("dummy {}", stats.report(&mut since_last));
            last_report = Instant::now();
        }

        next_block += block_period;
        let now = Instant::now();
        if next_block > now {
            std::thread::sleep(next_block - now);
        } else {
            // Do not run an unbounded catch-up burst after debugger pauses.
            next_block = now;
        }
    }

    eprintln!("Dummy audio stopping");
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::SupportedBufferSize;

    fn range(channels: u16, min: u32, max: u32) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            SampleRate(min),
            SampleRate(max),
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )
    }

    #[test]
    fn output_config_prefers_48k_and_smallest_suitable_channel_count() {
        let config = preferred_output_config(
            [range(64, 44_100, 96_000), range(32, 48_000, 48_000)],
            2,
            true,
        )
        .unwrap();
        assert_eq!(config.sample_rate(), SampleRate(48_000));
        assert_eq!(config.channels(), 32);
    }

    #[test]
    fn output_config_uses_closest_rate_when_48k_is_unavailable() {
        let config = preferred_output_config([range(2, 96_000, 192_000)], 2, false).unwrap();
        assert_eq!(config.sample_rate(), SampleRate(96_000));
        assert_eq!(config.channels(), 2);
    }

    #[test]
    fn dummy_audio_advances_engine_without_a_device() {
        let (engine, _commands, _layouts, _voices) =
            StimulusEngine::new(crate::config::TransducerLayout::default());
        let (levels_tx, mut levels_rx) = rtrb::RingBuffer::new(8);
        let running = Arc::new(AtomicBool::new(true));
        let device_channels = Arc::new(AtomicU16::new(0));
        let thread = {
            let running = running.clone();
            let device_channels = device_channels.clone();
            std::thread::spawn(move || {
                run_dummy_audio_loop(engine, running, false, levels_tx, device_channels)
            })
        };

        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while levels_rx.pop().is_err() {
            assert!(Instant::now() < deadline, "dummy audio produced no block");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        running.store(false, Ordering::Relaxed);
        thread.join().unwrap();
        assert_eq!(device_channels.load(Ordering::Relaxed), 32);
    }
}
