//! Standalone phase visualiser and test console for the haptic server.
//!
//! Connects to the server's Unix socket and renders the configured
//! transducer layout as coloured circles. In per-note mode each circle's
//! OKLCH hue encodes the phase of the most recent delay-line voice at that
//! transducer, relative to the source oscillator: zero phase difference
//! maps to blue, and phase lag rotates the hue around the wheel.
//! Lightness/chroma follow local amplitude, with chroma clamped into the
//! sRGB gamut so the sweep never clips.
//!
//! The viewer is also a test console: it can start/stop a test note, set
//! its parameters, move the source by dragging on the table (or orbit it
//! automatically), and route any logical channel to the audio device's
//! physical outputs by clicking circles (left-click → output 1/L,
//! right-click → output 2/R).
//!
//! Two source cursors are drawn: a ring at the MPE-requested position and
//! a cross at the effective source, which the engine velocity-limits to a
//! fraction of the wave speed so the source never outruns its own waves.
//!
//! Rendering repaints continuously under vsync, so a 120 Hz display
//! renders at 120 fps (see the on-screen fps counter).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use haptic_protocol::{
    encode_frame, travelling_wave_relative_phasor, ClientRole, DistanceDecay, FrameDecoder,
    HapticCommand, InstanceConfig, MpeData, Parameter, ServerStatus, SpatialScaleMode,
    StimulusType, PROTOCOL_VERSION, SOCKET_PATH,
};
use parking_lot::Mutex;

const TRANSDUCERS: usize = 32;

/// Wave-speed floor mirroring the engine's `MIN_WAVE_SPEED`, used when the
/// viewer reconstructs per-transducer delays geometrically from a voice's
/// source position and wave speed.
const WAVE_SPEED_FLOOR: f32 = 0.25;

/// OKLCH hue of sRGB blue: the reference for zero phase difference.
const ZERO_PHASE_HUE_DEG: f32 = 264.0;

/// A voice older than this is considered ended (the server stops sending
/// snapshots when nothing is active).
const VOICE_STALE: Duration = Duration::from_millis(300);

/// MIDI channel used for the viewer's test note (avoids the low channels
/// a DAW/MPE zone will typically use first).
const TEST_CHANNEL: u8 = 15;

/// Minimum interval between outgoing MPE position updates.
const MPE_SEND_INTERVAL: Duration = Duration::from_millis(8);

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
    instance_id: u64,
    note: u8,
    note_type: StimulusType,
    frequency: f32,
    wave_speed: f32,
    scale_mode: SpatialScaleMode,
    wavelength_m: f32,
    decay: DistanceDecay,
    /// Effective (velocity-limited) source the delay lines radiate from.
    source_pos: (f32, f32),
    /// Where MPE is asking the source to be.
    requested_pos: (f32, f32),
    amplitude: f32,
}

#[derive(Clone, Copy)]
struct RoutingView {
    device_channels: u16,
    routes: [u8; TRANSDUCERS],
}

#[derive(Default)]
struct Shared {
    connected: bool,
    /// Writable clone of the socket for sending commands from the UI.
    writer: Option<UnixStream>,
    layout: Option<LayoutView>,
    /// All active voices from the last `ActiveVoices` broadcast, with the
    /// time it arrived (for staleness).
    voices: Option<(Vec<VoiceView>, Instant)>,
    routing: Option<RoutingView>,
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
        while self
            .stamps
            .front()
            .is_some_and(|t| now - *t > Duration::from_secs(1))
        {
            self.stamps.pop_front();
        }
    }
}

/// Send a command over the shared writer; on failure the connection is
/// considered dead (the reader thread will re-establish it).
fn send_command(shared: &Mutex<Shared>, cmd: &HapticCommand) {
    let mut frame = Vec::with_capacity(64);
    if encode_frame(cmd, &mut frame).is_err() {
        return;
    }
    let mut state = shared.lock();
    if let Some(writer) = state.writer.as_mut() {
        if writer.write_all(&frame).is_err() {
            state.writer = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Socket reader
// ---------------------------------------------------------------------------

fn reader_thread(shared: Arc<Mutex<Shared>>, instance_id: u64, socket_path: String) {
    loop {
        let Ok(mut stream) = UnixStream::connect(&socket_path) else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        // Handshake as an Observer so the server sends us the status stream
        // (controllers receive none). Our own test notes are stamped with this
        // same instance_id server-side, so the viewer's wave-speed slider sets
        // its own config and never contends with a plugin's.
        {
            let mut hello = Vec::with_capacity(64);
            if encode_frame(
                &HapticCommand::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    instance_id,
                    role: ClientRole::Observer,
                    config: InstanceConfig::default(),
                },
                &mut hello,
            )
            .is_err()
                || stream.write_all(&hello).is_err()
            {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        }
        let mut decoder = FrameDecoder::new();
        let mut buffer = [0u8; 8192];
        let mut pending_writer = stream.try_clone().ok();
        let mut accepted = false;
        'connection: loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break 'connection,
                Ok(n) => {
                    decoder.extend(&buffer[..n]);
                    loop {
                        match decoder.next_frame::<ServerStatus>() {
                            Ok(Some(ServerStatus::HelloAccepted {
                                protocol_version,
                                instance_id: accepted_id,
                            })) if protocol_version == PROTOCOL_VERSION
                                && accepted_id == instance_id =>
                            {
                                accepted = true;
                                let mut state = shared.lock();
                                state.connected = true;
                                state.writer = pending_writer.take();
                            }
                            Ok(Some(msg)) if accepted => apply_message(&shared, msg),
                            Ok(Some(_)) => break 'connection,
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
        state.writer = None;
        state.voices = None;
        drop(state);
        thread::sleep(Duration::from_millis(500));
    }
}

fn apply_message(shared: &Mutex<Shared>, msg: ServerStatus) {
    match msg {
        ServerStatus::Layout {
            positions,
            gains,
            table_m,
        } => {
            shared.lock().layout = Some(LayoutView {
                positions,
                gains,
                table_m,
            });
        }
        ServerStatus::MonitorRouting {
            device_channels,
            routes,
        } => {
            shared.lock().routing = Some(RoutingView {
                device_channels,
                routes,
            });
        }
        ServerStatus::ActiveVoices { count, voices, .. } => {
            let list: Vec<VoiceView> = voices
                .iter()
                .take(count as usize)
                .map(|v| VoiceView {
                    instance_id: v.instance_id,
                    note: v.note,
                    note_type: v.note_type,
                    frequency: v.frequency,
                    wave_speed: v.wave_speed,
                    scale_mode: v.scale_mode,
                    wavelength_m: v.wavelength_m,
                    decay: DistanceDecay {
                        d0_m: v.atten_d0_m,
                        exponent: v.atten_exponent,
                    },
                    source_pos: v.source_pos,
                    requested_pos: v.requested_pos,
                    amplitude: v.amplitude,
                })
                .collect();
            let mut state = shared.lock();
            state.voices = Some((list, Instant::now()));
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
    (-EPS..=1.0 + EPS).contains(&r)
        && (-EPS..=1.0 + EPS).contains(&g)
        && (-EPS..=1.0 + EPS).contains(&b)
}

fn linear_to_srgb_u8(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
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
// Test stimulus state
// ---------------------------------------------------------------------------

struct TestControls {
    playing: Option<u8>, // note currently sounding
    note: u8,
    velocity: u8,
    stimulus_type: StimulusType,
    wave_speed: f32,
    scale_mode: SpatialScaleMode,
    wavelength_m: f32,
    atten_d0_m: f32,
    atten_exponent: f32,
    orbit: bool,
    orbit_period_s: f32,
    orbit_phase: f32,
    /// Desired source position in metres.
    source: (f32, f32),
    last_mpe_sent: Instant,
}

impl Default for TestControls {
    fn default() -> Self {
        Self {
            playing: None,
            note: 60,
            velocity: 100,
            stimulus_type: StimulusType::Wave,
            wave_speed: 1.0,
            scale_mode: SpatialScaleMode::Speed,
            wavelength_m: 0.2,
            atten_d0_m: haptic_protocol::DEFAULT_ATTEN_D0_M,
            atten_exponent: haptic_protocol::DEFAULT_ATTEN_EXPONENT,
            orbit: false,
            orbit_period_s: 6.0,
            orbit_phase: 0.0,
            source: (0.5, 1.0),
            last_mpe_sent: Instant::now(),
        }
    }
}

impl TestControls {
    fn mpe(&self, table: (f32, f32)) -> MpeData {
        MpeData {
            pressure: 1.0,
            pitch_bend: (2.0 * self.source.0 / table.0.max(1e-6) - 1.0).clamp(-1.0, 1.0),
            timbre: (self.source.1 / table.1.max(1e-6)).clamp(0.0, 1.0),
        }
    }

    fn start(&mut self, shared: &Mutex<Shared>, table: (f32, f32)) {
        self.send_config(shared);
        send_command(
            shared,
            &HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter: Parameter::StimulusType(self.stimulus_type),
            },
        );
        send_command(
            shared,
            &HapticCommand::NoteOn {
                timestamp_us: 0,
                note: self.note,
                velocity: self.velocity,
                channel: TEST_CHANNEL,
                mpe: self.mpe(table),
            },
        );
        self.playing = Some(self.note);
    }

    fn send_config(&self, shared: &Mutex<Shared>) {
        send_command(
            shared,
            &HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter: Parameter::WaveSpeed(self.wave_speed),
            },
        );
        send_command(
            shared,
            &HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter: Parameter::TravellingWaveScaleMode(self.scale_mode),
            },
        );
        send_command(
            shared,
            &HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter: Parameter::TravellingWaveWavelength(self.wavelength_m),
            },
        );
        send_command(
            shared,
            &HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter: Parameter::AttenuationD0(self.atten_d0_m),
            },
        );
        send_command(
            shared,
            &HapticCommand::SetParameter {
                timestamp_us: 0,
                parameter: Parameter::AttenuationExponent(self.atten_exponent),
            },
        );
    }

    fn stop(&mut self, shared: &Mutex<Shared>) {
        if let Some(note) = self.playing.take() {
            send_command(
                shared,
                &HapticCommand::NoteOff {
                    timestamp_us: 0,
                    note,
                    channel: TEST_CHANNEL,
                },
            );
        }
    }

    fn send_position(&mut self, shared: &Mutex<Shared>, table: (f32, f32)) {
        if self.playing.is_some() && self.last_mpe_sent.elapsed() >= MPE_SEND_INTERVAL {
            self.last_mpe_sent = Instant::now();
            send_command(
                shared,
                &HapticCommand::MpeUpdate {
                    timestamp_us: 0,
                    channel: TEST_CHANNEL,
                    mpe: self.mpe(table),
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// Which active voices the field visualisation includes. The viewer is the
/// one observer of whole-server state, so by default it sums every voice;
/// filtering narrows to a single controller instance.
#[derive(Clone, Copy, PartialEq)]
enum VoiceFilter {
    /// Sum the field of every active voice (whole-system state).
    All,
    /// Only voices owned by this instance.
    Instance(u64),
}

struct ViewerApp {
    shared: Arc<Mutex<Shared>>,
    fps: RateCounter,
    test: TestControls,
    /// Which voices the field visualisation includes.
    filter: VoiceFilter,
    /// Parameter values in effect for the sounding note, to retrigger when
    /// the user lands on new slider values.
    sounding_params: (u8, u8, StimulusType, f32),
    last_live_config: (f32, SpatialScaleMode, f32, f32, f32),
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn note_name(note: u8) -> String {
    format!("{}{}", NOTE_NAMES[note as usize % 12], note as i16 / 12 - 1)
}

/// What the table area reported this frame.
#[derive(Default)]
struct TableInteraction {
    route_to_output: Option<(u8, usize)>, // (physical output, logical channel)
    drag_world: Option<(f32, f32)>,
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Continuous repaint: under vsync this renders at the display's
        // refresh rate (120 fps on a 120 Hz display).
        ctx.request_repaint();
        self.fps.tick();
        let fps = self.fps.rate();

        let (connected, layout, voices, routing, voice_rate) = {
            let mut state = self.shared.lock();
            let rate = state.voice_rate.rate();
            let voices = state
                .voices
                .as_ref()
                .filter(|(_, at)| at.elapsed() < VOICE_STALE)
                .map(|(v, _)| v.clone())
                .unwrap_or_default();
            (state.connected, state.layout, voices, state.routing, rate)
        };
        let table = layout.map(|l| l.table_m).unwrap_or((1.0, 2.0));

        // Distinct instances currently sounding, for the filter picker.
        let mut instances: Vec<u64> = voices.iter().map(|v| v.instance_id).collect();
        instances.sort_unstable();
        instances.dedup();
        // Drop a stale instance filter back to All if that instance went quiet.
        if let VoiceFilter::Instance(id) = self.filter {
            if !instances.contains(&id) {
                self.filter = VoiceFilter::All;
            }
        }
        let shown: Vec<VoiceView> = voices
            .iter()
            .copied()
            .filter(|v| match self.filter {
                VoiceFilter::All => true,
                VoiceFilter::Instance(id) => v.instance_id == id,
            })
            .collect();

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if connected {
                    ui.colored_label(egui::Color32::from_rgb(64, 200, 120), "● connected");
                } else {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "● waiting for server");
                }
                if let Some(r) = routing {
                    ui.separator();
                    ui.label(format!("device: {} ch", r.device_channels));
                }
                ui.separator();
                ui.label(match voices.len() {
                    0 => "no active voices".to_string(),
                    n => format!("{n} voice(s), {} instance(s)", instances.len()),
                });
                if shown.len() == 1 {
                    let v = &shown[0];
                    ui.separator();
                    match v.note_type {
                        StimulusType::Wave => ui.label(format!(
                            "Wave · note {} ({:.1} Hz)  c = {:.0} m/s  λ = {:.2} m",
                            note_name(v.note),
                            v.frequency,
                            v.wave_speed,
                            v.wave_speed / v.frequency,
                        )),
                        StimulusType::TravellingWave => ui.label(format!(
                            "TW ({}) · note {} ({:.1} Hz)  c = {:.2} m/s  λ = {:.4} m",
                            match v.scale_mode {
                                SpatialScaleMode::Speed => "fixed speed",
                                SpatialScaleMode::Wavelength => "fixed wavelength",
                            },
                            note_name(v.note),
                            v.frequency,
                            v.wave_speed,
                            v.wavelength_m,
                        )),
                    };
                }
                ui.separator();
                ui.label(format!("{fps} fps / {voice_rate} msg/s"));
                ui.separator();
                ui.weak("geometric preview · voices shown phase-aligned");
            });
        });

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            self.filter_ui(ui, &instances);
            ui.add_enabled_ui(connected, |ui| self.test_controls_ui(ui, table));
            ui.label("drag on table: move source (○ requested, ✚ effective)   ·   left-click a cell: monitor on output 1 (L)   ·   right-click: output 2 (R)");
        });

        let mut interaction = TableInteraction::default();
        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(layout) = layout else {
                ui.centered_and_justified(|ui| {
                    ui.label("waiting for layout broadcast…");
                });
                return;
            };
            interaction = draw_table(ui, &layout, &shown, routing.as_ref());
        });

        // Apply table interactions
        if let Some((output, source)) = interaction.route_to_output {
            send_command(
                &self.shared,
                &HapticCommand::SetParameter {
                    timestamp_us: 0,
                    parameter: Parameter::MonitorRoute {
                        output,
                        source: source as u8,
                    },
                },
            );
        }
        if let Some(world) = interaction.drag_world {
            self.test.orbit = false;
            self.test.source = (world.0.clamp(0.0, table.0), world.1.clamp(0.0, table.1));
            self.test.send_position(&self.shared, table);
        }

        // Orbit: circle the source around the table centre
        if self.test.orbit && self.test.playing.is_some() {
            let dt = ctx.input(|i| i.stable_dt).min(0.1);
            self.test.orbit_phase += std::f32::consts::TAU * dt / self.test.orbit_period_s.max(0.5);
            let radius = 0.35 * table.0.min(table.1);
            self.test.source = (
                0.5 * table.0 + radius * self.test.orbit_phase.cos(),
                0.5 * table.1 + radius * self.test.orbit_phase.sin(),
            );
            self.test.send_position(&self.shared, table);
        }

        // Retrigger when slider values settle on something new mid-note
        let desired = (
            self.test.note,
            self.test.velocity,
            self.test.stimulus_type,
            if self.test.stimulus_type == StimulusType::Wave {
                self.test.wave_speed
            } else {
                0.0
            },
        );
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        if self.test.playing.is_some() && desired != self.sounding_params && !pointer_down {
            self.test.stop(&self.shared);
            self.test.start(&self.shared, table);
            self.sounding_params = desired;
        }
        let live_config = (
            self.test.wave_speed,
            self.test.scale_mode,
            self.test.wavelength_m,
            self.test.atten_d0_m,
            self.test.atten_exponent,
        );
        if live_config != self.last_live_config {
            self.test.send_config(&self.shared);
            self.last_live_config = live_config;
        }
        if !connected {
            self.test.playing = None;
        }
    }
}

impl ViewerApp {
    /// Filter picker: sum the whole system, or narrow to one controller
    /// instance. Instances are labelled by a short hash of their id.
    fn filter_ui(&mut self, ui: &mut egui::Ui, instances: &[u64]) {
        ui.horizontal(|ui| {
            ui.label("visualise:");
            let label = match self.filter {
                VoiceFilter::All => "all instances (summed)".to_string(),
                VoiceFilter::Instance(id) => format!("instance {:04x}", id & 0xffff),
            };
            egui::ComboBox::from_id_salt("voice_filter")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.filter,
                        VoiceFilter::All,
                        "all instances (summed)",
                    );
                    for &id in instances {
                        ui.selectable_value(
                            &mut self.filter,
                            VoiceFilter::Instance(id),
                            format!("instance {:04x}", id & 0xffff),
                        );
                    }
                });
        });
    }

    fn test_controls_ui(&mut self, ui: &mut egui::Ui, table: (f32, f32)) {
        // Keep the console usable at the default 620 px window width. These
        // controls used to occupy one long row, which clipped at wave speed.
        ui.horizontal(|ui| {
            let playing = self.test.playing.is_some();
            let button = if playing {
                "■ stop test note"
            } else {
                "▶ start test note"
            };
            if ui.button(button).clicked() {
                if playing {
                    self.test.stop(&self.shared);
                } else {
                    self.test.start(&self.shared, table);
                    self.sounding_params = (
                        self.test.note,
                        self.test.velocity,
                        self.test.stimulus_type,
                        if self.test.stimulus_type == StimulusType::Wave {
                            self.test.wave_speed
                        } else {
                            0.0
                        },
                    );
                }
            }
            ui.separator();
            ui.label("note");
            ui.add(
                egui::Slider::new(&mut self.test.note, 24..=96)
                    .custom_formatter(|v, _| note_name(v as u8)),
            );
        });
        ui.horizontal(|ui| {
            ui.label("type");
            egui::ComboBox::from_id_salt("test_stimulus_type")
                .selected_text(match self.test.stimulus_type {
                    StimulusType::Wave => "Wave",
                    StimulusType::TravellingWave => "TW",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.test.stimulus_type, StimulusType::Wave, "Wave");
                    ui.selectable_value(
                        &mut self.test.stimulus_type,
                        StimulusType::TravellingWave,
                        "Travelling Wave (TW)",
                    );
                });
            ui.separator();
            ui.label("velocity");
            ui.add(egui::Slider::new(&mut self.test.velocity, 1..=127));
        });
        ui.horizontal(|ui| {
            ui.separator();
            ui.label("wave speed");
            ui.add_enabled(
                self.test.stimulus_type == StimulusType::Wave
                    || self.test.scale_mode == SpatialScaleMode::Speed,
                egui::Slider::new(
                    &mut self.test.wave_speed,
                    haptic_protocol::MIN_WAVE_SPEED..=haptic_protocol::MAX_WAVE_SPEED,
                )
                .logarithmic(true)
                .suffix(" m/s"),
            );
        });
        if self.test.stimulus_type == StimulusType::TravellingWave {
            ui.horizontal(|ui| {
                ui.label("TW scale");
                ui.selectable_value(&mut self.test.scale_mode, SpatialScaleMode::Speed, "speed");
                ui.selectable_value(
                    &mut self.test.scale_mode,
                    SpatialScaleMode::Wavelength,
                    "fixed wavelength",
                );
                ui.add_enabled(
                    self.test.scale_mode == SpatialScaleMode::Wavelength,
                    egui::Slider::new(
                        &mut self.test.wavelength_m,
                        haptic_protocol::MIN_WAVELENGTH_M..=haptic_protocol::MAX_WAVELENGTH_M,
                    )
                    .logarithmic(true)
                    .suffix(" m"),
                );
            });
        }
        ui.horizontal(|ui| {
            ui.label("decay knee");
            ui.add(
                egui::Slider::new(
                    &mut self.test.atten_d0_m,
                    haptic_protocol::MIN_ATTEN_D0_M..=haptic_protocol::MAX_ATTEN_D0_M,
                )
                .logarithmic(true)
                .suffix(" m"),
            );
            ui.separator();
            ui.label("exponent");
            ui.add(egui::Slider::new(
                &mut self.test.atten_exponent,
                haptic_protocol::MIN_ATTEN_EXPONENT..=haptic_protocol::MAX_ATTEN_EXPONENT,
            ));
        });
        ui.horizontal(|ui| {
            ui.label("motion");
            ui.separator();
            ui.checkbox(&mut self.test.orbit, "orbit");
            ui.add(
                egui::Slider::new(&mut self.test.orbit_period_s, 1.0..=30.0)
                    .logarithmic(true)
                    .suffix(" s"),
            );
        });
    }
}

/// Approximate complex field at a transducer. Wave propagation phase and
/// attenuation are reconstructed geometrically for both stimulus types.
/// Voice snapshots do not carry synchronized oscillator phase or delay-line
/// history, so multiple voices are intentionally shown phase-aligned rather
/// than presented as exact server-output interference.
fn field_at(pos: (f32, f32), voices: &[VoiceView]) -> (f32, f32) {
    let (mut re, mut im) = (0.0f32, 0.0f32);
    for v in voices {
        let dx = pos.0 - v.source_pos.0;
        let dy = pos.1 - v.source_pos.1;
        let dist = (dx * dx + dy * dy).sqrt();
        let wavelength = match v.note_type {
            StimulusType::Wave => v.wave_speed.max(WAVE_SPEED_FLOOR) / v.frequency,
            StimulusType::TravellingWave => v.wavelength_m,
        };
        let (voice_re, voice_im) = travelling_wave_relative_phasor(dist, wavelength, v.decay);
        re += v.amplitude * voice_re;
        im += v.amplitude * voice_im;
    }
    (re, im)
}

fn draw_table(
    ui: &mut egui::Ui,
    layout: &LayoutView,
    voices: &[VoiceView],
    routing: Option<&RoutingView>,
) -> TableInteraction {
    let mut interaction = TableInteraction::default();
    let size = ui.available_size();
    let (response, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
    let avail = response.rect.shrink(12.0);

    // World bounds: the table extent, padded so overridden transducers and
    // the source marker stay visible
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
    let to_screen = |x: f32, y: f32| {
        egui::pos2(
            origin.x + (x - min_x) * scale,
            origin.y + (y - min_y) * scale,
        )
    };
    let to_world = |p: egui::Pos2| {
        (
            (p.x - origin.x) / scale + min_x,
            (p.y - origin.y) / scale + min_y,
        )
    };

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
    let pointer = response.interact_pointer_pos();
    let circle_under = |p: egui::Pos2| {
        layout
            .positions
            .iter()
            .enumerate()
            .map(|(i, &(x, y))| (i, to_screen(x, y)))
            .find(|(_, c)| c.distance(p) <= radius + 2.0)
            .map(|(i, _)| i)
    };

    for (i, &(x, y)) in layout.positions.iter().enumerate() {
        let center = to_screen(x, y);
        let color = if voices.is_empty() {
            egui::Color32::from_gray(60)
        } else {
            // Summed field: hue = resultant phase, lightness/chroma = resultant
            // magnitude (× this transducer's configured gain). For a single
            // voice this reduces to its relative phase and local amplitude.
            let (re, im) = field_at((x, y), voices);
            let hue = ZERO_PHASE_HUE_DEG + im.atan2(re).to_degrees();
            let amp = (re * re + im * im).sqrt() * layout.gains[i];
            let vis = (amp * 2.0).clamp(0.0, 1.0).sqrt();
            let lightness = 0.25 + 0.50 * vis;
            let chroma = 0.03 + 0.14 * vis;
            oklch_to_color(lightness, chroma, hue)
        };
        painter.circle_filled(center, radius, color);
        painter.circle_stroke(
            center,
            radius,
            egui::Stroke::new(1.0, egui::Color32::from_gray(30)),
        );

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

    // Monitor-routing badges: which physical output plays which circle
    if let Some(r) = routing {
        let outputs = (r.device_channels as usize).min(TRANSDUCERS).min(4);
        for output in 0..outputs {
            let source = r.routes[output] as usize % TRANSDUCERS;
            let (x, y) = layout.positions[source];
            let center = to_screen(x, y) + egui::vec2(radius * 0.9, -radius * 0.9);
            let label = match output {
                0 => "L".to_string(),
                1 => "R".to_string(),
                n => format!("{}", n + 1),
            };
            painter.circle_filled(center, 7.0, egui::Color32::from_gray(235));
            painter.text(
                center,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(10.0),
                egui::Color32::BLACK,
            );
        }
    }

    // Source cursors, one per visualised voice: the ring is the MPE-requested
    // position, the cross is the effective (velocity-limited) source the delay
    // lines radiate from; a tether joins them while the source is catching up.
    for v in voices {
        let src = to_screen(v.source_pos.0, v.source_pos.1);
        let req = to_screen(v.requested_pos.0, v.requested_pos.1);
        if req.distance(src) > 1.0 {
            painter.line_segment(
                [src, req],
                egui::Stroke::new(1.0, egui::Color32::from_gray(140)),
            );
        }
        painter.circle_stroke(
            req,
            6.0,
            egui::Stroke::new(1.5, egui::Color32::from_gray(180)),
        );
        let arm = 7.0;
        let stroke = egui::Stroke::new(2.0, egui::Color32::WHITE);
        painter.line_segment(
            [src - egui::vec2(arm, 0.0), src + egui::vec2(arm, 0.0)],
            stroke,
        );
        painter.line_segment(
            [src - egui::vec2(0.0, arm), src + egui::vec2(0.0, arm)],
            stroke,
        );
    }

    // Interactions: clicks on circles select monitor routing; drags move
    // the test source
    if let Some(p) = pointer {
        if response.clicked() {
            if let Some(i) = circle_under(p) {
                interaction.route_to_output = Some((0, i));
            }
        }
        if response.secondary_clicked() {
            if let Some(i) = circle_under(p) {
                interaction.route_to_output = Some((1, i));
            }
        }
        if response.dragged_by(egui::PointerButton::Primary) {
            interaction.drag_world = Some(to_world(p));
        }
    }

    interaction
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let socket_path = args
        .iter()
        .position(|argument| argument == "--socket")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .or_else(|| std::env::var("HAPTIC_SOCKET_PATH").ok())
        .unwrap_or_else(|| SOCKET_PATH.to_string());
    eprintln!("Viewer socket: {socket_path}");
    let shared = Arc::new(Mutex::new(Shared::default()));
    // Stable, non-zero identity for this viewer's own (test-console) instance.
    let instance_id = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        nanos ^ ((std::process::id() as u64) << 32) | 1
    };
    {
        let shared = shared.clone();
        thread::spawn(move || reader_thread(shared, instance_id, socket_path));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 1020.0])
            .with_title("Haptic Viewer"),
        vsync: true,
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native(
        "Haptic Viewer",
        options,
        Box::new(|_cc| {
            Ok(Box::new(ViewerApp {
                shared,
                fps: RateCounter::default(),
                test: TestControls::default(),
                filter: VoiceFilter::All,
                sounding_params: (60, 100, StimulusType::Wave, 20.0),
                last_live_config: (
                    1.0,
                    SpatialScaleMode::Speed,
                    0.2,
                    haptic_protocol::DEFAULT_ATTEN_D0_M,
                    haptic_protocol::DEFAULT_ATTEN_EXPONENT,
                ),
            }))
        }),
    )
}
