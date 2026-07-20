//! Standalone phase visualiser for the haptic server.
//!
//! Connects to the server's Unix socket as a read-only status client and
//! renders the configured transducer layout as coloured circles. In
//! per-note mode each circle's OKLCH hue encodes the phase of the most
//! recent delay-line voice at that transducer, relative to the source
//! oscillator: zero phase difference maps to blue, and phase lag rotates
//! the hue around the wheel. Lightness/chroma follow local amplitude, with
//! chroma clamped into the sRGB gamut so the sweep never clips.
//!
//! Rendering repaints continuously under vsync, so a 120 Hz display
//! renders at 120 fps (see the on-screen fps counter).

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use haptic_protocol::{FrameDecoder, ServerStatus, SOCKET_PATH};
use parking_lot::Mutex;

const TRANSDUCERS: usize = 32;

/// OKLCH hue of sRGB blue: the reference for zero phase difference.
const ZERO_PHASE_HUE_DEG: f32 = 264.0;

/// A voice older than this is considered ended (the server stops sending
/// snapshots when nothing is active).
const VOICE_STALE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Shared state between the socket reader thread and the UI thread
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct LayoutView {
    positions: [(f32, f32); TRANSDUCERS],
    gains: [f32; TRANSDUCERS],
    table_m: (f32, f32),
}

#[derive(Clone, Copy)]
struct VoiceView {
    note: u8,
    frequency: f32,
    wave_speed: f32,
    source_pos: (f32, f32),
    amplitude: f32,
    sample_rate: f32,
    delay_samples: [f32; TRANSDUCERS],
}

#[derive(Default)]
struct Shared {
    connected: bool,
    layout: Option<LayoutView>,
    voice: Option<(VoiceView, Instant)>,
    voice_rate: RateCounter,
}

#[derive(Default)]
struct RateCounter {
    stamps: VecDeque<Instant>,
}

impl RateCounter {
    fn tick(&mut self) {
        let now = Instant::now();
        self.stamps.push_back(now);
        self.trim(now);
    }

    fn rate(&mut self) -> usize {
        self.trim(Instant::now());
        self.stamps.len()
    }

    fn trim(&mut self, now: Instant) {
        while self.stamps.front().is_some_and(|t| now - *t > Duration::from_secs(1)) {
            self.stamps.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------
// Socket reader
// ---------------------------------------------------------------------------

fn reader_thread(shared: Arc<Mutex<Shared>>) {
    loop {
        let Ok(mut stream) = UnixStream::connect(SOCKET_PATH) else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        shared.lock().connected = true;

        let mut decoder = FrameDecoder::new();
        let mut buffer = [0u8; 8192];
        'connection: loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break 'connection,
                Ok(n) => {
                    decoder.extend(&buffer[..n]);
                    loop {
                        match decoder.next_frame::<ServerStatus>() {
                            Ok(Some(msg)) => apply_message(&shared, msg),
                            Ok(None) => break,
                            // Framing errors are unrecoverable mid-stream;
                            // reconnect to resynchronise
                            Err(_) => break 'connection,
                        }
                    }
                }
            }
        }

        let mut state = shared.lock();
        state.connected = false;
        state.voice = None;
        drop(state);
        thread::sleep(Duration::from_millis(500));
    }
}

fn apply_message(shared: &Mutex<Shared>, msg: ServerStatus) {
    match msg {
        ServerStatus::Layout { positions, gains, table_m } => {
            shared.lock().layout = Some(LayoutView { positions, gains, table_m });
        }
        ServerStatus::VoiceState {
            note,
            frequency,
            wave_speed,
            source_pos,
            amplitude,
            sample_rate,
            delay_samples,
            ..
        } => {
            let mut state = shared.lock();
            state.voice = Some((
                VoiceView { note, frequency, wave_speed, source_pos, amplitude, sample_rate, delay_samples },
                Instant::now(),
            ));
            state.voice_rate.tick();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// OKLCH -> sRGB with gamut-clamped chroma
// ---------------------------------------------------------------------------

fn oklab_to_linear_srgb(l: f32, a: f32, b: f32) -> (f32, f32, f32) {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    (
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
    )
}

fn in_gamut((r, g, b): (f32, f32, f32)) -> bool {
    const EPS: f32 = 1e-4;
    (-EPS..=1.0 + EPS).contains(&r) && (-EPS..=1.0 + EPS).contains(&g) && (-EPS..=1.0 + EPS).contains(&b)
}

fn linear_to_srgb_u8(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0 + 0.5) as u8
}

/// Convert OKLCH to sRGB, reducing chroma (never hue or lightness) until
/// the colour fits the gamut — a full hue sweep therefore cannot clip.
fn oklch_to_color(l: f32, c: f32, h_deg: f32) -> egui::Color32 {
    let h = h_deg.to_radians();
    let (sin_h, cos_h) = h.sin_cos();
    let lab = |chroma: f32| oklab_to_linear_srgb(l, chroma * cos_h, chroma * sin_h);

    let mut rgb = lab(c);
    if !in_gamut(rgb) {
        let (mut lo, mut hi) = (0.0f32, c);
        for _ in 0..12 {
            let mid = 0.5 * (lo + hi);
            if in_gamut(lab(mid)) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        rgb = lab(lo);
    }
    egui::Color32::from_rgb(
        linear_to_srgb_u8(rgb.0),
        linear_to_srgb_u8(rgb.1),
        linear_to_srgb_u8(rgb.2),
    )
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct ViewerApp {
    shared: Arc<Mutex<Shared>>,
    fps: RateCounter,
}

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

fn note_name(note: u8) -> String {
    format!("{}{}", NOTE_NAMES[note as usize % 12], note as i16 / 12 - 1)
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Continuous repaint: under vsync this renders at the display's
        // refresh rate (120 fps on a 120 Hz display).
        ctx.request_repaint();
        self.fps.tick();
        let fps = self.fps.rate();

        let (connected, layout, voice, voice_rate) = {
            let mut state = self.shared.lock();
            let rate = state.voice_rate.rate();
            (state.connected, state.layout, state.voice, rate)
        };
        let fresh_voice = voice.and_then(|(v, at)| (at.elapsed() < VOICE_STALE).then_some(v));

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if connected {
                    ui.colored_label(egui::Color32::from_rgb(64, 200, 120), "● connected");
                } else {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "● waiting for server");
                }
                ui.separator();
                match fresh_voice {
                    Some(v) => ui.label(format!(
                        "note {} ({:.1} Hz)   c = {:.0} m/s   λ = {:.2} m   amp {:.2}",
                        note_name(v.note),
                        v.frequency,
                        v.wave_speed,
                        v.wave_speed / v.frequency,
                        v.amplitude,
                    )),
                    None => ui.label("no active voice"),
                };
                ui.separator();
                ui.label(format!("{fps} fps / {voice_rate} msg/s"));
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(layout) = layout else {
                ui.centered_and_justified(|ui| {
                    ui.label("waiting for layout broadcast…");
                });
                return;
            };
            draw_table(ui, &layout, fresh_voice.as_ref());
        });
    }
}

fn draw_table(ui: &mut egui::Ui, layout: &LayoutView, voice: Option<&VoiceView>) {
    let avail = ui.available_rect_before_wrap().shrink(12.0);
    let painter = ui.painter_at(ui.available_rect_before_wrap());

    // World bounds: the table extent, padded so overridden transducers and
    // the MPE source excursion stay visible
    let pad = 0.10;
    let (mut min_x, mut min_y) = (-pad, -pad);
    let (mut max_x, mut max_y) = (layout.table_m.0 + pad, layout.table_m.1 + pad);
    for &(x, y) in layout.positions.iter() {
        min_x = min_x.min(x - pad);
        min_y = min_y.min(y - pad);
        max_x = max_x.max(x + pad);
        max_y = max_y.max(y + pad);
    }
    let world_w = max_x - min_x;
    let world_h = max_y - min_y;
    let scale = (avail.width() / world_w).min(avail.height() / world_h);
    let origin = egui::pos2(
        avail.center().x - 0.5 * world_w * scale,
        avail.center().y - 0.5 * world_h * scale,
    );
    let to_screen =
        |x: f32, y: f32| egui::pos2(origin.x + (x - min_x) * scale, origin.y + (y - min_y) * scale);

    // Table outline
    let table_rect = egui::Rect::from_two_pos(
        to_screen(0.0, 0.0),
        to_screen(layout.table_m.0, layout.table_m.1),
    );
    painter.rect_stroke(
        table_rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
        egui::StrokeKind::Middle,
    );

    // Transducer circles
    let radius = (0.09 * scale).clamp(5.0, 40.0);
    for (i, &(x, y)) in layout.positions.iter().enumerate() {
        let center = to_screen(x, y);
        let color = match voice {
            Some(v) => {
                // Relative phase at this transducer: the delay the engine's
                // delay line is applying, as a fraction of the wave period
                let dphi = std::f32::consts::TAU * v.frequency * v.delay_samples[i] / v.sample_rate;
                let hue = ZERO_PHASE_HUE_DEG + dphi.to_degrees();

                // Local amplitude: source level x distance attenuation
                // (matching the engine) x configured gain
                let distance = v.delay_samples[i] / v.sample_rate * v.wave_speed;
                let amp = v.amplitude * layout.gains[i] / (1.0 + 2.0 * distance);
                let vis = (amp * 2.0).clamp(0.0, 1.0).sqrt();

                let lightness = 0.25 + 0.50 * vis;
                let chroma = 0.03 + 0.14 * vis;
                oklch_to_color(lightness, chroma, hue)
            }
            None => egui::Color32::from_gray(60),
        };
        painter.circle_filled(center, radius, color);
        painter.circle_stroke(center, radius, egui::Stroke::new(1.0, egui::Color32::from_gray(30)));

        if radius > 9.0 {
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                format!("{}", i),
                egui::FontId::monospace((radius * 0.7).min(12.0)),
                egui::Color32::from_gray(220),
            );
        }
    }

    // MPE-driven source position
    if let Some(v) = voice {
        let src = to_screen(v.source_pos.0, v.source_pos.1);
        let arm = 7.0;
        let stroke = egui::Stroke::new(2.0, egui::Color32::WHITE);
        painter.line_segment([src - egui::vec2(arm, 0.0), src + egui::vec2(arm, 0.0)], stroke);
        painter.line_segment([src - egui::vec2(0.0, arm), src + egui::vec2(0.0, arm)], stroke);
    }
}

fn main() -> eframe::Result<()> {
    let shared = Arc::new(Mutex::new(Shared::default()));
    {
        let shared = shared.clone();
        thread::spawn(move || reader_thread(shared));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 980.0])
            .with_title("Haptic Viewer"),
        vsync: true,
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native(
        "Haptic Viewer",
        options,
        Box::new(|_cc| Ok(Box::new(ViewerApp { shared, fps: RateCounter::default() }))),
    )
}
