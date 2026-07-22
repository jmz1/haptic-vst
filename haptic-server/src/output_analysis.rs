//! Fixed-capacity Hilbert analysis of the final logical device output.
//!
//! The engine feeds this module samples only after reconstruction and final
//! safety bounding. Analysis is decimated to approximately 1.5 kHz: this is
//! comfortably above the 20--200 Hz haptic band while keeping the 32-channel
//! FIR bounded enough for the audio callback. Every analysed value is still an
//! actual sample from the device-rate logical stream, not a geometric model.

use std::f32::consts::PI;

pub const HILBERT_TAPS: usize = 255;
pub const HILBERT_DELAY_SAMPLES: usize = (HILBERT_TAPS - 1) / 2;
const TARGET_ANALYSIS_RATE: f32 = 1_500.0;

#[derive(Clone, Copy)]
pub struct AnalyticFrame<const CHANNELS: usize> {
    pub sample_index: u64,
    pub samples: [(f32, f32); CHANNELS],
}

/// Odd-symmetric, Blackman-windowed ideal Hilbert transformer. The matching
/// real component is delayed by the FIR's integer group delay.
pub struct OutputAnalyzer<const CHANNELS: usize> {
    history: Box<[[f32; CHANNELS]; HILBERT_TAPS]>,
    coefficients: [f32; HILBERT_TAPS],
    history_pos: usize,
    samples_seen: usize,
    device_sample_rate: f32,
    decimation: usize,
    latest: Option<AnalyticFrame<CHANNELS>>,
}

impl<const CHANNELS: usize> OutputAnalyzer<CHANNELS> {
    pub fn new() -> Self {
        Self {
            history: Box::new([[0.0; CHANNELS]; HILBERT_TAPS]),
            coefficients: design_hilbert_fir(),
            history_pos: 0,
            samples_seen: 0,
            device_sample_rate: 0.0,
            decimation: 1,
            latest: None,
        }
    }

    /// Consume one final, bounded logical device frame. This is callback-safe:
    /// all storage was allocated at construction and the work is fixed-size.
    pub fn process(
        &mut self,
        samples: &[f32; CHANNELS],
        sample_index: u64,
        device_sample_rate: f32,
    ) {
        self.configure_rate(device_sample_rate);
        if sample_index % self.decimation as u64 != 0 {
            return;
        }

        self.history_pos = (self.history_pos + HILBERT_TAPS - 1) % HILBERT_TAPS;
        self.history[self.history_pos] = *samples;
        self.samples_seen = self.samples_seen.saturating_add(1);

        let delayed_pos = (self.history_pos + HILBERT_DELAY_SAMPLES) % HILBERT_TAPS;
        let mut analytic = [(0.0f32, 0.0f32); CHANNELS];
        for (channel, output) in analytic.iter_mut().enumerate() {
            let real = self.history[delayed_pos][channel];
            let mut imaginary = 0.0f32;
            for (tap, &coefficient) in self.coefficients.iter().enumerate() {
                let pos = (self.history_pos + tap) % HILBERT_TAPS;
                imaginary += coefficient * self.history[pos][channel];
            }
            *output = (real, imaginary);
        }

        self.latest = Some(AnalyticFrame {
            sample_index,
            samples: analytic,
        });
    }

    pub fn latest(&self) -> Option<AnalyticFrame<CHANNELS>> {
        (self.samples_seen >= HILBERT_TAPS)
            .then_some(self.latest)
            .flatten()
    }

    pub fn decimation(&self) -> usize {
        self.decimation
    }

    fn configure_rate(&mut self, device_sample_rate: f32) {
        if self.device_sample_rate == device_sample_rate {
            return;
        }
        self.device_sample_rate = device_sample_rate;
        self.decimation = (device_sample_rate / TARGET_ANALYSIS_RATE).round().max(1.0) as usize;
        self.history.fill([0.0; CHANNELS]);
        self.history_pos = 0;
        self.samples_seen = 0;
        self.latest = None;
    }
}

fn design_hilbert_fir() -> [f32; HILBERT_TAPS] {
    let mut coefficients = [0.0f32; HILBERT_TAPS];
    let center = HILBERT_DELAY_SAMPLES as isize;
    for (index, coefficient) in coefficients.iter_mut().enumerate() {
        let offset = index as isize - center;
        if offset != 0 && offset % 2 != 0 {
            let phase = 2.0 * PI * index as f32 / (HILBERT_TAPS - 1) as f32;
            let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            *coefficient = 2.0 * window / (PI * offset as f32);
        }
    }
    coefficients
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_becomes_phase_aligned_analytic_signal() {
        const SAMPLE_RATE: f32 = 48_000.0;
        const FREQUENCY: f32 = 65.406;
        let mut analyzer = OutputAnalyzer::<1>::new();
        for sample_index in 0..48_000u64 {
            let phase = std::f32::consts::TAU * FREQUENCY * sample_index as f32 / SAMPLE_RATE;
            analyzer.process(&[phase.sin()], sample_index, SAMPLE_RATE);
        }

        let output = analyzer.latest().expect("Hilbert history should be full");
        let represented_index =
            output.sample_index - (HILBERT_DELAY_SAMPLES * analyzer.decimation()) as u64;
        let expected = std::f32::consts::TAU * FREQUENCY * represented_index as f32 / SAMPLE_RATE
            - std::f32::consts::FRAC_PI_2;
        let (real, imaginary) = output.samples[0];
        let error = (imaginary.atan2(real) - expected + PI).rem_euclid(2.0 * PI) - PI;
        assert!(error.abs() < 0.01, "analytic phase error {error} rad");
        assert!((real.hypot(imaginary) - 1.0).abs() < 0.01);
    }

    #[test]
    fn reports_nothing_until_the_full_history_is_available() {
        let mut analyzer = OutputAnalyzer::<1>::new();
        for sample_index in 0..(HILBERT_TAPS as u64 - 1) * 32 {
            analyzer.process(&[0.5], sample_index, 48_000.0);
        }
        assert!(analyzer.latest().is_none());
        analyzer.process(&[0.5], (HILBERT_TAPS as u64 - 1) * 32, 48_000.0);
        assert!(analyzer.latest().is_some());
    }
}
