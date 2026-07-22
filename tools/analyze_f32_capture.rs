//! Dependency-free spectral analysis for interleaved little-endian f32 captures.
//!
//! Build and run, for example:
//!   rustc --edition 2021 -O tools/analyze_f32_capture.rs -o target/analyze-f32-capture
//!   target/analyze-f32-capture capture.f32 48000 32 2.0 262144

use std::{env, fs, process};

#[derive(Clone, Copy, Default)]
struct Stats {
    sum: f64,
    sum_sq: f64,
    peak: f64,
    diff_sum_sq: f64,
    diff_peak: f64,
    peak_frame: usize,
    samples: usize,
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert!(n.is_power_of_two());
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let angle = -std::f64::consts::TAU / len as f64;
        let (wlen_im, wlen_re) = angle.sin_cos();
        for start in (0..n).step_by(len) {
            let mut w_re = 1.0;
            let mut w_im = 0.0;
            for offset in 0..len / 2 {
                let even = start + offset;
                let odd = even + len / 2;
                let v_re = re[odd] * w_re - im[odd] * w_im;
                let v_im = re[odd] * w_im + im[odd] * w_re;
                let u_re = re[even];
                let u_im = im[even];
                re[even] = u_re + v_re;
                im[even] = u_im + v_im;
                re[odd] = u_re - v_re;
                im[odd] = u_im - v_im;
                let next_re = w_re * wlen_re - w_im * wlen_im;
                w_im = w_re * wlen_im + w_im * wlen_re;
                w_re = next_re;
            }
        }
        len <<= 1;
    }
}

fn sample(bytes: &[u8], frame: usize, channel: usize, channels: usize) -> f64 {
    let offset = (frame * channels + channel) * 4;
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as f64
}

fn db_ratio(numerator: f64, denominator: f64) -> f64 {
    if numerator <= 0.0 || denominator <= 0.0 {
        f64::NEG_INFINITY
    } else {
        10.0 * (numerator / denominator).log10()
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 6 {
        eprintln!(
            "usage: {} INPUT.f32 [sample_rate=48000] [channels=32] [start_s=2] [fft_n=262144]",
            args[0]
        );
        process::exit(2);
    }
    let path = &args[1];
    let sample_rate: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(48_000.0);
    let channels: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(32);
    let start_s: f64 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(2.0);
    let fft_n: usize = args.get(5).and_then(|v| v.parse().ok()).unwrap_or(262_144);
    if channels == 0 || !fft_n.is_power_of_two() {
        eprintln!("channels must be nonzero and fft_n must be a power of two");
        process::exit(2);
    }

    let bytes = fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        process::exit(1);
    });
    let frame_bytes = channels * 4;
    if bytes.len() % frame_bytes != 0 {
        eprintln!("file length is not a whole number of {channels}-channel f32 frames");
        process::exit(1);
    }
    let frames = bytes.len() / frame_bytes;
    let start = (start_s * sample_rate).round() as usize;
    if start >= frames || start + fft_n > frames {
        eprintln!(
            "capture has {frames} frames; requested FFT [{start}, {})",
            start + fft_n
        );
        process::exit(1);
    }

    let mut stats = vec![Stats::default(); channels];
    let mut previous = vec![0.0; channels];
    for frame in start..frames {
        for channel in 0..channels {
            let value = sample(&bytes, frame, channel, channels);
            let stat = &mut stats[channel];
            stat.sum += value;
            stat.sum_sq += value * value;
            if value.abs() > stat.peak {
                stat.peak = value.abs();
                stat.peak_frame = frame;
            }
            if frame > start {
                let diff = value - previous[channel];
                stat.diff_sum_sq += diff * diff;
                stat.diff_peak = stat.diff_peak.max(diff.abs());
            }
            stat.samples += 1;
            previous[channel] = value;
        }
    }

    let mut spectra = Vec::with_capacity(channels);
    let mut aggregate = vec![0.0f64; fft_n / 2 + 1];
    for channel in 0..channels {
        let mut re = vec![0.0; fft_n];
        let mut im = vec![0.0; fft_n];
        for (i, value) in re.iter_mut().enumerate() {
            let window = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (fft_n - 1) as f64).cos();
            *value = sample(&bytes, start + i, channel, channels) * window;
        }
        fft(&mut re, &mut im);
        let power: Vec<f64> = re[..=fft_n / 2]
            .iter()
            .zip(&im[..=fft_n / 2])
            .map(|(r, i)| r * r + i * i)
            .collect();
        for (sum, value) in aggregate.iter_mut().zip(&power) {
            *sum += value;
        }
        spectra.push(power);
    }

    let bin_hz = sample_rate / fft_n as f64;
    let band_power = |power: &[f64], lo: f64, hi: f64| -> f64 {
        power
            .iter()
            .enumerate()
            .filter(|(bin, _)| {
                let hz = *bin as f64 * bin_hz;
                hz >= lo && hz < hi
            })
            .map(|(_, value)| value)
            .sum()
    };
    let peak_bin = |power: &[f64], lo: f64, hi: f64| -> usize {
        power
            .iter()
            .enumerate()
            .filter(|(bin, _)| {
                let hz = *bin as f64 * bin_hz;
                hz >= lo && hz < hi
            })
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(bin, _)| bin)
            .unwrap_or(0)
    };

    println!("capture={path}");
    println!(
        "frames={frames} duration_s={:.6} analysis_start_s={start_s:.6} fft_n={fft_n} bin_hz={bin_hz:.9}",
        frames as f64 / sample_rate
    );
    println!("channel,rms,peak,peak_time_s,crest_db,dc,diff_rms,diff_peak,peak_hz,lt20_db,band20_200_db,band200_750_db,band750_2000_db,ge2000_db");
    for channel in 0..channels {
        let s = stats[channel];
        let rms = (s.sum_sq / s.samples as f64).sqrt();
        let diff_rms = (s.diff_sum_sq / (s.samples - 1).max(1) as f64).sqrt();
        let power = &spectra[channel];
        let total = power.iter().sum::<f64>();
        let peak = peak_bin(power, 20.0, 200.0);
        println!(
            "{channel},{rms:.9},{:.9},{:.6},{:.3},{:.9},{diff_rms:.9},{:.9},{:.6},{:.3},{:.3},{:.3},{:.3},{:.3}",
            s.peak,
            s.peak_frame as f64 / sample_rate,
            20.0 * (s.peak / rms.max(f64::MIN_POSITIVE)).log10(),
            s.sum / s.samples as f64,
            s.diff_peak,
            peak as f64 * bin_hz,
            db_ratio(band_power(power, 0.0, 20.0), total),
            db_ratio(band_power(power, 20.0, 200.0), total),
            db_ratio(band_power(power, 200.0, 750.0), total),
            db_ratio(band_power(power, 750.0, 2000.0), total),
            db_ratio(band_power(power, 2000.0, sample_rate / 2.0 + bin_hz), total),
        );
    }

    let total = aggregate.iter().sum::<f64>();
    println!("aggregate_band_db_relative_total:");
    for (label, lo, hi) in [
        ("lt20", 0.0, 20.0),
        ("20_45", 20.0, 45.0),
        ("45_110", 45.0, 110.0),
        ("110_200", 110.0, 200.0),
        ("131_200", 131.0, 200.0),
        ("20_200", 20.0, 200.0),
        ("200_750", 200.0, 750.0),
        ("750_2000", 750.0, 2000.0),
        ("ge2000", 2000.0, sample_rate / 2.0 + bin_hz),
    ] {
        println!(
            "{label}={:.6}",
            db_ratio(band_power(&aggregate, lo, hi), total)
        );
    }
    let aggregate_peak = aggregate.iter().copied().fold(0.0, f64::max);
    println!("aggregate_strongest_bin_hz_db_relative_peak:");
    for (label, lo, hi) in [
        ("20_110", 20.0, 110.0),
        ("110_200", 110.0, 200.0),
        ("200_750", 200.0, 750.0),
        ("750_2000", 750.0, 2000.0),
    ] {
        let bin = peak_bin(&aggregate, lo, hi);
        println!(
            "{label}={:.6},{:.3}",
            bin as f64 * bin_hz,
            db_ratio(aggregate[bin], aggregate_peak)
        );
    }

    let selected = stats
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.sum_sq.total_cmp(&b.1.sum_sq))
        .map(|(channel, _)| channel)
        .unwrap();
    let power = &spectra[selected];
    let reference = power.iter().copied().fold(0.0, f64::max);
    let mut peaks: Vec<(usize, f64)> = (1..power.len() - 1)
        .filter(|&bin| {
            let hz = bin as f64 * bin_hz;
            hz >= 20.0
                && hz <= 2000.0
                && power[bin] > power[bin - 1]
                && power[bin] >= power[bin + 1]
        })
        .map(|bin| (bin, power[bin]))
        .collect();
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("selected_channel={selected} strongest_local_peaks_hz_db_relative_capture_peak:");
    for (bin, value) in peaks.into_iter().take(24) {
        println!(
            "{:.6},{:.3}",
            bin as f64 * bin_hz,
            db_ratio(value, reference)
        );
    }
}
