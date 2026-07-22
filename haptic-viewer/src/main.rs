//! Primary Haptic GUI: managed-server supervisor, phase visualiser, and test
//! console.
//!
//! Attaches to an existing haptic server or starts a supervised child process,
//! then connects over the same Unix-socket observer protocol and renders the
//! configured transducer layout as coloured circles. Each circle's OKLCH hue
//! is derived from the server's Hilbert analytic signal for the final summed
//! logical output, relative to a selected active source oscillator: zero phase
//! difference maps to blue, and phase lag rotates the hue around the wheel.
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
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use haptic_protocol::{
    encode_frame, ClientRole, FrameDecoder, HapticCommand, InstanceConfig, MpeData, Parameter,
    ServerStatus, SpatialScaleMode, StimulusType, PROTOCOL_VERSION, SOCKET_PATH,
};
use parking_lot::Mutex;

const TRANSDUCERS: usize = 32;
const TRANSDUCER_RADIUS_M: f32 = 0.09;
const DISPLAY_EDGE_PADDING_PX: f32 = 8.0;

/// OKLCH hue of sRGB blue: the reference for zero phase difference.
const ZERO_PHASE_HUE_DEG: f32 = 264.0;

/// A measured field older than this is considered disconnected/stale.
const OUTPUT_STALE: Duration = Duration::from_millis(300);
const OUTPUT_SILENCE_THRESHOLD: f32 = 1.0e-4;
const SILENT_SNAPSHOTS_TO_RELEASE_REFERENCE: u8 = 3;
const REFERENCE_FILTER_TAIL_HOLD_S: f32 = 0.25;

/// MIDI channel used for the viewer's test note (avoids the low channels
/// a DAW/MPE zone will typically use first).
const TEST_CHANNEL: u8 = 15;

/// Minimum interval between outgoing MPE position updates.
const MPE_SEND_INTERVAL: Duration = Duration::from_millis(8);

const MAX_SERVER_LOG_LINES: usize = 200;
const MANAGED_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Unified application / server supervision
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppOptions {
    socket_path: String,
    connect_only: bool,
    server_bin: Option<PathBuf>,
    config_path: PathBuf,
    dummy_audio: bool,
    test_tone: bool,
}

fn parse_options(
    args: impl IntoIterator<Item = String>,
    environment_socket: Option<String>,
    environment_server_bin: Option<String>,
    process_id: u32,
) -> Result<AppOptions, String> {
    let mut connect_only = false;
    let mut server_bin = environment_server_bin.map(PathBuf::from);
    let mut config_path = PathBuf::from("haptic.toml");
    let mut dummy_audio = false;
    let mut test_tone = false;
    let mut socket_path = None;
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--connect-only" => connect_only = true,
            "--headless" | "--dummy-audio" => dummy_audio = true,
            "--test-tone" => test_tone = true,
            "--server-bin" => {
                server_bin = Some(PathBuf::from(next_option_value(&mut args, "--server-bin")?));
            }
            "--config" => {
                config_path = PathBuf::from(next_option_value(&mut args, "--config")?);
            }
            "--socket" => socket_path = Some(next_option_value(&mut args, "--socket")?),
            unknown => return Err(format!("unknown argument {unknown}; use --help")),
        }
    }

    let socket_path = socket_path.or(environment_socket).unwrap_or_else(|| {
        if dummy_audio {
            format!("/tmp/haptic-vst-app-{process_id}.sock")
        } else {
            SOCKET_PATH.to_string()
        }
    });

    Ok(AppOptions {
        socket_path,
        connect_only,
        server_bin,
        config_path,
        dummy_audio,
        test_tone,
    })
}

fn next_option_value(
    args: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{option} requires a value"))
}

fn print_usage() {
    eprintln!(
        "Usage: haptic-viewer [--connect-only] [--server-bin PATH] [--config PATH] [--headless|--dummy-audio] [--test-tone] [--socket PATH]\n\
         \n\
         By default the application attaches to a live server at the selected\n\
         endpoint or starts and supervises a sibling haptic-server executable.\n\
         \n\
         --connect-only           Never start or stop a server.\n\
         --server-bin PATH        Server executable to start when needed.\n\
         --config PATH            Layout passed to a managed server.\n\
         --headless, --dummy-audio  Start a managed 48 kHz memory sink.\n\
         --test-tone              Start a managed server in hardware test-tone mode.\n\
         --socket PATH            Server endpoint for attachment and launch.\n\
         HAPTIC_SERVER_BIN        Environment alternative to --server-bin.\n\
         HAPTIC_SOCKET_PATH       Environment alternative to --socket."
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerMode {
    ConnectOnly,
    External,
    Managed,
    Stopped,
    Failed,
}

struct ServerSupervisor {
    options: AppOptions,
    mode: ServerMode,
    child: Option<Child>,
    lifetime: Option<ChildStdin>,
    logs: Arc<Mutex<VecDeque<String>>>,
}

impl ServerSupervisor {
    fn new(options: AppOptions) -> Self {
        let mut supervisor = Self {
            mode: if options.connect_only {
                ServerMode::ConnectOnly
            } else {
                ServerMode::Stopped
            },
            options,
            child: None,
            lifetime: None,
            logs: Arc::new(Mutex::new(VecDeque::new())),
        };

        if supervisor.options.connect_only {
            push_server_log(
                &supervisor.logs,
                "Connect-only mode: waiting for an external server".to_string(),
            );
        } else if server_reachable(&supervisor.options.socket_path) {
            supervisor.mode = ServerMode::External;
            push_server_log(
                &supervisor.logs,
                format!(
                    "Attached to external server at {}",
                    supervisor.options.socket_path
                ),
            );
        } else {
            supervisor.start_managed();
        }

        supervisor
    }

    fn start_managed(&mut self) {
        if self.child.is_some() || self.options.connect_only {
            return;
        }
        if server_reachable(&self.options.socket_path) {
            self.mode = ServerMode::External;
            push_server_log(
                &self.logs,
                format!("Using external server at {}", self.options.socket_path),
            );
            return;
        }

        let Some(server_bin) = resolve_server_binary(self.options.server_bin.as_ref()) else {
            self.mode = ServerMode::Failed;
            push_server_log(
                &self.logs,
                "Could not find haptic-server. Build it beside haptic-viewer, set HAPTIC_SERVER_BIN, or pass --server-bin PATH."
                    .to_string(),
            );
            return;
        };

        let mut command = Command::new(&server_bin);
        command
            .arg("--managed-lifetime-stdin")
            .arg("--socket")
            .arg(&self.options.socket_path)
            .arg("--config")
            .arg(&self.options.config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.options.dummy_audio {
            command.arg("--headless");
        }
        if self.options.test_tone {
            command.arg("--test-tone");
        }

        push_server_log(
            &self.logs,
            format!("Starting managed server: {}", server_bin.display()),
        );
        match command.spawn() {
            Ok(mut child) => {
                self.lifetime = child.stdin.take();
                if let Some(stdout) = child.stdout.take() {
                    capture_server_output(stdout, self.logs.clone());
                }
                if let Some(stderr) = child.stderr.take() {
                    capture_server_output(stderr, self.logs.clone());
                }
                self.child = Some(child);
                self.mode = ServerMode::Managed;
            }
            Err(error) => {
                self.mode = ServerMode::Failed;
                push_server_log(
                    &self.logs,
                    format!("Failed to start {}: {error}", server_bin.display()),
                );
            }
        }
    }

    fn poll(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                push_server_log(&self.logs, format!("Managed server exited: {status}"));
                self.child = None;
                self.lifetime = None;
                self.mode = if server_reachable(&self.options.socket_path) {
                    ServerMode::External
                } else {
                    ServerMode::Failed
                };
            }
            Ok(None) => {}
            Err(error) => {
                push_server_log(
                    &self.logs,
                    format!("Could not poll managed server: {error}"),
                );
                self.lifetime = None;
                if let Some(mut child) = self.child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                self.mode = ServerMode::Failed;
            }
        }
    }

    fn stop_managed(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        push_server_log(&self.logs, "Stopping managed server".to_string());
        self.lifetime = None;

        let deadline = Instant::now() + MANAGED_SHUTDOWN_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    push_server_log(&self.logs, format!("Managed server stopped: {status}"));
                    self.mode = ServerMode::Stopped;
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                Ok(None) | Err(_) => break,
            }
        }

        push_server_log(
            &self.logs,
            "Managed server did not stop after stdin EOF; terminating it".to_string(),
        );
        let _ = child.kill();
        let _ = child.wait();
        self.mode = ServerMode::Stopped;
    }

    fn restart_managed(&mut self) {
        self.stop_managed();
        self.start_managed();
    }

    fn ui(&mut self, ui: &mut egui::Ui, connected: bool) {
        self.poll();
        ui.horizontal(|ui| {
            let (colour, label) = match self.mode {
                ServerMode::Managed if connected => {
                    (egui::Color32::from_rgb(64, 200, 120), "managed")
                }
                ServerMode::Managed => (egui::Color32::from_rgb(220, 170, 60), "starting"),
                ServerMode::External if connected => {
                    (egui::Color32::from_rgb(80, 160, 230), "external")
                }
                ServerMode::External => (
                    egui::Color32::from_rgb(220, 170, 60),
                    "external unavailable",
                ),
                ServerMode::ConnectOnly if connected => {
                    (egui::Color32::from_rgb(80, 160, 230), "attached")
                }
                ServerMode::ConnectOnly => (egui::Color32::from_rgb(150, 150, 150), "waiting"),
                ServerMode::Stopped => (egui::Color32::from_rgb(150, 150, 150), "stopped"),
                ServerMode::Failed => (egui::Color32::from_rgb(220, 80, 80), "server failed"),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.mode {
                    ServerMode::Managed => {
                        if ui.button("stop").clicked() {
                            self.stop_managed();
                        }
                        if ui.button("restart").clicked() {
                            self.restart_managed();
                        }
                    }
                    ServerMode::Stopped | ServerMode::Failed => {
                        if ui.button("start server").clicked() {
                            self.start_managed();
                        }
                    }
                    ServerMode::ConnectOnly | ServerMode::External => {}
                }

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.colored_label(colour, format!("● {label}"));
                    ui.separator();
                    ui.add(
                        egui::Label::new(egui::RichText::new(&self.options.socket_path).weak())
                            .truncate(),
                    )
                    .on_hover_text(&self.options.socket_path);
                });
            });
        });

        egui::CollapsingHeader::new("server log")
            .default_open(matches!(self.mode, ServerMode::Failed))
            .show(ui, |ui| {
                let logs = self.logs.lock();
                for line in logs.iter().rev().take(12).rev() {
                    ui.add(egui::Label::new(egui::RichText::new(line).monospace()).truncate())
                        .on_hover_text(line);
                }
            });
    }
}

impl Drop for ServerSupervisor {
    fn drop(&mut self) {
        self.stop_managed();
    }
}

fn server_reachable(socket_path: &str) -> bool {
    UnixStream::connect(socket_path).is_ok()
}

fn resolve_server_binary(explicit: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return path.is_file().then(|| path.clone());
    }

    let name = format!("haptic-server{}", std::env::consts::EXE_SUFFIX);
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            let sibling = parent.join(&name);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let workspace_candidate = PathBuf::from("target").join(profile).join(name);
    workspace_candidate.is_file().then_some(workspace_candidate)
}

fn capture_server_output(reader: impl Read + Send + 'static, logs: Arc<Mutex<VecDeque<String>>>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => push_server_log(&logs, line),
                Err(error) => {
                    push_server_log(&logs, format!("Could not read server output: {error}"));
                    break;
                }
            }
        }
    });
}

fn push_server_log(logs: &Mutex<VecDeque<String>>, line: String) {
    let mut logs = logs.lock();
    logs.push_back(line);
    while logs.len() > MAX_SERVER_LOG_LINES {
        logs.pop_front();
    }
}

// ---------------------------------------------------------------------------
// Shared state between the socket reader thread and the UI thread
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct LayoutView {
    positions: [(f32, f32); TRANSDUCERS],
    table_m: (f32, f32),
}

#[derive(Clone, Copy)]
struct VoiceView {
    instance_id: u64,
    seq: u64,
    note: u8,
    frequency: f32,
    /// Effective (velocity-limited) source the delay lines radiate from.
    source_pos: (f32, f32),
    /// Where MPE is asking the source to be.
    requested_pos: (f32, f32),
    amplitude: f32,
    reference_phase: f32,
}

#[derive(Clone)]
struct OutputView {
    device_sample_rate: f32,
    sample_index: u64,
    valid: bool,
    analytic: [(f32, f32); TRANSDUCERS],
    voices: Vec<VoiceView>,
    received_at: Instant,
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
    /// Measured final output and its synchronized oscillator references.
    output: Option<OutputView>,
    routing: Option<RoutingView>,
    output_rate: RateCounter,
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
        state.output = None;
        drop(state);
        thread::sleep(Duration::from_millis(500));
    }
}

fn apply_message(shared: &Mutex<Shared>, msg: ServerStatus) {
    match msg {
        ServerStatus::Layout {
            positions,
            gains: _,
            table_m,
        } => {
            shared.lock().layout = Some(LayoutView { positions, table_m });
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
        ServerStatus::OutputState {
            device_sample_rate,
            sample_index,
            valid,
            analytic,
            count,
            voices,
            ..
        } => {
            let list: Vec<VoiceView> = voices
                .iter()
                .take(count as usize)
                .map(|v| VoiceView {
                    instance_id: v.instance_id,
                    seq: v.seq,
                    note: v.note,
                    frequency: v.frequency,
                    source_pos: v.source_pos,
                    requested_pos: v.requested_pos,
                    amplitude: v.amplitude,
                    reference_phase: v.reference_phase,
                })
                .collect();
            let mut state = shared.lock();
            state.output = Some(OutputView {
                device_sample_rate,
                sample_index,
                valid,
                analytic,
                voices: list,
                received_at: Instant::now(),
            });
            state.output_rate.tick();
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
            note: haptic_protocol::DEFAULT_TEST_NOTE,
            velocity: 100,
            stimulus_type: StimulusType::Wave,
            wave_speed: 5.0,
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

/// Rule for choosing the oscillator against which the same measured summed
/// output is compared. This never filters or otherwise changes the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceRule {
    StickyNewest,
    Oldest,
    Strongest,
    PreferViewerTest,
}

#[derive(Clone, Copy, Debug)]
struct SelectedReference {
    instance_id: u64,
    seq: u64,
    note: u8,
    frequency: f32,
    phase: f32,
    sample_index: u64,
    device_sample_rate: f32,
    amplitude: f32,
}

impl SelectedReference {
    fn from_voice(voice: &VoiceView, output: &OutputView) -> Self {
        Self {
            instance_id: voice.instance_id,
            seq: voice.seq,
            note: voice.note,
            frequency: voice.frequency,
            phase: voice.reference_phase,
            sample_index: output.sample_index,
            device_sample_rate: output.device_sample_rate,
            amplitude: voice.amplitude,
        }
    }

    fn advance_to(&mut self, output: &OutputView) {
        if self.device_sample_rate != output.device_sample_rate
            || output.sample_index < self.sample_index
        {
            self.sample_index = output.sample_index;
            self.device_sample_rate = output.device_sample_rate;
            return;
        }
        let frames = output.sample_index - self.sample_index;
        self.phase = (self.phase
            + std::f32::consts::TAU * self.frequency * frames as f32 / output.device_sample_rate)
            .rem_euclid(std::f32::consts::TAU);
        self.sample_index = output.sample_index;
        self.device_sample_rate = output.device_sample_rate;
    }
}

struct ViewerApp {
    shared: Arc<Mutex<Shared>>,
    server: ServerSupervisor,
    fps: RateCounter,
    test: TestControls,
    instance_id: u64,
    reference_rule: ReferenceRule,
    reference: Option<SelectedReference>,
    reference_missing_since: Option<u64>,
    silent_reference_snapshots: u8,
    /// Parameter values in effect for the sounding note, to retrigger when
    /// the user lands on new slider values.
    sounding_params: (u8, u8, StimulusType, f32),
    last_live_config: (f32, SpatialScaleMode, f32, f32, f32),
}

impl ViewerApp {
    fn update_reference(&mut self, output: &OutputView) {
        if let Some(reference) = self.reference.as_mut() {
            if let Some(voice) = output.voices.iter().find(|voice| {
                voice.instance_id == reference.instance_id && voice.seq == reference.seq
            }) {
                *reference = SelectedReference::from_voice(voice, output);
            } else {
                reference.advance_to(output);
            }
        }

        let reference_is_live = self.reference.is_some_and(|reference| {
            output.voices.iter().any(|voice| {
                voice.instance_id == reference.instance_id && voice.seq == reference.seq
            })
        });
        let output_is_silent = output
            .analytic
            .iter()
            .all(|&(real, imaginary)| real.hypot(imaginary) < OUTPUT_SILENCE_THRESHOLD);
        if self.reference.is_some() && !reference_is_live {
            let missing_since = *self
                .reference_missing_since
                .get_or_insert(output.sample_index);
            if output.valid && output_is_silent {
                self.silent_reference_snapshots = self.silent_reference_snapshots.saturating_add(1);
            } else {
                self.silent_reference_snapshots = 0;
            }
            let hold_elapsed = output.sample_index.saturating_sub(missing_since) as f32
                / output.device_sample_rate
                >= REFERENCE_FILTER_TAIL_HOLD_S;
            if hold_elapsed
                || self.silent_reference_snapshots >= SILENT_SNAPSHOTS_TO_RELEASE_REFERENCE
            {
                self.reference = None;
                self.reference_missing_since = None;
                self.silent_reference_snapshots = 0;
            }
        } else {
            self.reference_missing_since = None;
            self.silent_reference_snapshots = 0;
        }

        let candidate = match self.reference_rule {
            ReferenceRule::StickyNewest => {
                sticky_newest_candidate(&output.voices, self.reference.as_ref(), |_| true)
            }
            ReferenceRule::Oldest => {
                if self.reference.is_none() {
                    output.voices.iter().min_by_key(|voice| voice.seq)
                } else {
                    None
                }
            }
            ReferenceRule::Strongest => {
                let strongest = output.voices.iter().max_by(|a, b| {
                    a.amplitude
                        .partial_cmp(&b.amplitude)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                match (strongest, self.reference.as_ref()) {
                    (Some(voice), Some(current))
                        if voice.seq != current.seq
                            && voice.amplitude <= current.amplitude * 1.1 =>
                    {
                        None
                    }
                    (voice, _) => voice,
                }
            }
            ReferenceRule::PreferViewerTest => {
                let preferred =
                    sticky_newest_candidate(&output.voices, self.reference.as_ref(), |voice| {
                        voice.instance_id == self.instance_id
                    });
                preferred.or_else(|| {
                    if self
                        .reference
                        .is_some_and(|reference| reference.instance_id == self.instance_id)
                    {
                        None
                    } else {
                        sticky_newest_candidate(&output.voices, self.reference.as_ref(), |_| true)
                    }
                })
            }
        };

        if let Some(voice) = candidate {
            if self
                .reference
                .is_none_or(|reference| reference.seq != voice.seq)
            {
                self.reference = Some(SelectedReference::from_voice(voice, output));
                self.reference_missing_since = None;
                self.silent_reference_snapshots = 0;
            }
        }
    }
}

fn sticky_newest_candidate<'a>(
    voices: &'a [VoiceView],
    current: Option<&SelectedReference>,
    include: impl Fn(&VoiceView) -> bool,
) -> Option<&'a VoiceView> {
    let newest = voices
        .iter()
        .filter(|voice| include(voice))
        .max_by_key(|voice| voice.seq)?;
    match current {
        Some(current) if newest.seq <= current.seq => None,
        _ => Some(newest),
    }
}

fn relative_to_reference(analytic: (f32, f32), reference_phase: f32) -> (f32, f32) {
    let (real, imaginary) = analytic;
    let (cosine, sine) = (reference_phase.cos(), reference_phase.sin());
    // z * conjugate(reference)
    (
        real * cosine + imaginary * sine,
        imaginary * cosine - real * sine,
    )
}

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

fn note_name(note: u8) -> String {
    // Ableton Live's octave convention: MIDI note 60 is C3 (rather than C4
    // in scientific pitch notation), and MIDI note 0 is C-2.
    format!("{}{}", NOTE_NAMES[note as usize % 12], note as i16 / 12 - 2)
}

#[cfg(test)]
mod note_name_tests {
    use super::{note_name, relative_to_reference, OutputView, SelectedReference, TRANSDUCERS};

    #[test]
    fn uses_ableton_octave_numbers() {
        assert_eq!(note_name(0), "C-2");
        assert_eq!(note_name(haptic_protocol::DEFAULT_TEST_NOTE), "A0");
        assert_eq!(note_name(48), "C2");
        assert_eq!(note_name(60), "C3");
        assert_eq!(note_name(69), "A3");
    }

    #[test]
    fn reference_rotation_preserves_the_existing_phase_direction() {
        let reference_phase = 0.7f32;
        let lag = -0.4f32;
        let analytic_phase = reference_phase + lag;
        let relative = relative_to_reference(
            (analytic_phase.cos(), analytic_phase.sin()),
            reference_phase,
        );
        assert!((relative.1.atan2(relative.0) - lag).abs() < 1.0e-6);
    }

    #[test]
    fn missing_reference_oscillator_continues_on_the_output_clock() {
        let mut reference = SelectedReference {
            instance_id: 1,
            seq: 2,
            note: 36,
            frequency: 100.0,
            phase: 0.25,
            sample_index: 1_000,
            device_sample_rate: 48_000.0,
            amplitude: 0.5,
        };
        let output = OutputView {
            device_sample_rate: 48_000.0,
            sample_index: 1_480,
            valid: true,
            analytic: [(0.0, 0.0); TRANSDUCERS],
            voices: Vec::new(),
            received_at: std::time::Instant::now(),
        };
        reference.advance_to(&output);
        let expected = (0.25 + std::f32::consts::TAU).rem_euclid(std::f32::consts::TAU);
        assert!((reference.phase - expected).abs() < 1.0e-5);
    }
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

        let (connected, layout, output, routing, output_rate) = {
            let mut state = self.shared.lock();
            let rate = state.output_rate.rate();
            let output = state
                .output
                .as_ref()
                .filter(|output| output.received_at.elapsed() < OUTPUT_STALE)
                .cloned();
            (state.connected, state.layout, output, state.routing, rate)
        };
        let table = layout.map(|l| l.table_m).unwrap_or((1.0, 2.0));
        if let Some(output) = output.as_ref() {
            self.update_reference(output);
        }
        let voices = output
            .as_ref()
            .map(|output| output.voices.as_slice())
            .unwrap_or_default();

        // Distinct instances currently sounding, for status only. The field
        // itself is never filtered by voice or instance.
        let mut instances: Vec<u64> = voices.iter().map(|v| v.instance_id).collect();
        instances.sort_unstable();
        instances.dedup();
        let relative_analytic = output.as_ref().and_then(|output| {
            let reference = self.reference?;
            output.valid.then(|| {
                std::array::from_fn(|channel| {
                    relative_to_reference(output.analytic[channel], reference.phase)
                })
            })
        });

        egui::TopBottomPanel::top("server").show(ctx, |ui| {
            self.server.ui(ui, connected);
        });

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Keep FPS at the fixed right edge. Changes in message rate or
                    // activity text can only consume space to its left.
                    ui.label(format!("{output_rate} msg/s · {fps} fps"));
                    ui.separator();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        if connected {
                            ui.colored_label(egui::Color32::from_rgb(64, 200, 120), "● connected");
                        } else {
                            ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "● waiting");
                        }
                        ui.separator();
                        let activity = match voices.len() {
                            0 => "idle".to_string(),
                            1 => "1 voice".to_string(),
                            n => format!("{n} voices · {} instances", instances.len()),
                        };
                        let summary = routing.map_or(activity.clone(), |routing| {
                            format!("{} ch · {activity}", routing.device_channels)
                        });
                        ui.add(egui::Label::new(&summary).truncate())
                            .on_hover_text(&summary);
                    });
                });
            });
        });

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            self.reference_ui(ui);
            ui.add_enabled_ui(connected, |ui| self.test_controls_ui(ui, table));
        });

        let mut interaction = TableInteraction::default();
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(0))
            .show(ctx, |ui| {
                let Some(layout) = layout else {
                    ui.centered_and_justified(|ui| {
                        ui.label("waiting for layout broadcast…");
                    });
                    return;
                };
                interaction = draw_table(
                    ui,
                    &layout,
                    voices,
                    routing.as_ref(),
                    relative_analytic.as_ref(),
                );
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
    fn reference_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("reference");
            let label = match self.reference_rule {
                ReferenceRule::StickyNewest => "newest",
                ReferenceRule::Oldest => "oldest",
                ReferenceRule::Strongest => "strongest",
                ReferenceRule::PreferViewerTest => "prefer test",
            };
            let mut changed = false;
            egui::ComboBox::from_id_salt("reference_rule")
                .selected_text(label)
                .width(110.0)
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.reference_rule,
                            ReferenceRule::StickyNewest,
                            "sticky newest",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.reference_rule,
                            ReferenceRule::Oldest,
                            "oldest active",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.reference_rule,
                            ReferenceRule::Strongest,
                            "strongest active",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.reference_rule,
                            ReferenceRule::PreferViewerTest,
                            "prefer viewer test note",
                        )
                        .changed();
                })
                .response
                .on_hover_text("Select the oscillator used as the phase reference");
            if changed {
                self.reference = None;
                self.reference_missing_since = None;
                self.silent_reference_snapshots = 0;
            }
            if let Some(reference) = self.reference {
                ui.separator();
                ui.add(
                    egui::Label::new(format!(
                        "{} · {:.1} Hz · #{:04x}",
                        note_name(reference.note),
                        reference.frequency,
                        reference.instance_id & 0xffff,
                    ))
                    .truncate(),
                );
            }
        });
    }

    fn test_controls_ui(&mut self, ui: &mut egui::Ui, table: (f32, f32)) {
        ui.spacing_mut().item_spacing.y = 3.0;

        ui.columns(2, |columns| {
            let playing = self.test.playing.is_some();
            let button = if playing {
                "■ stop test note"
            } else {
                "▶ start test note"
            };
            let button_size = [
                columns[0].available_width(),
                columns[0].spacing().interact_size.y,
            ];
            if columns[0]
                .add_sized(button_size, egui::Button::new(button))
                .clicked()
            {
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
            let slider_size = [
                columns[1].available_width(),
                columns[1].spacing().interact_size.y,
            ];
            columns[1].add_sized(
                slider_size,
                egui::Slider::new(&mut self.test.note, 24..=96)
                    .text("note")
                    .custom_formatter(|v, _| note_name(v as u8)),
            );
        });

        ui.columns(2, |columns| {
            columns[0].horizontal(|ui| {
                ui.label("type");
                egui::ComboBox::from_id_salt("test_stimulus_type")
                    .selected_text(match self.test.stimulus_type {
                        StimulusType::Wave => "Wave",
                        StimulusType::TravellingWave => "TW",
                    })
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.test.stimulus_type,
                            StimulusType::Wave,
                            "Wave",
                        );
                        ui.selectable_value(
                            &mut self.test.stimulus_type,
                            StimulusType::TravellingWave,
                            "Travelling Wave (TW)",
                        );
                    });
            });
            let slider_size = [
                columns[1].available_width(),
                columns[1].spacing().interact_size.y,
            ];
            columns[1].add_sized(
                slider_size,
                egui::Slider::new(&mut self.test.velocity, 1..=127).text("velocity"),
            );
        });

        if self.test.stimulus_type == StimulusType::TravellingWave {
            ui.columns(2, |columns| {
                columns[0].horizontal(|ui| {
                    ui.label("scale");
                    egui::ComboBox::from_id_salt("test_scale_mode")
                        .selected_text(match self.test.scale_mode {
                            SpatialScaleMode::Speed => "speed",
                            SpatialScaleMode::Wavelength => "wavelength",
                        })
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.test.scale_mode,
                                SpatialScaleMode::Speed,
                                "speed",
                            );
                            ui.selectable_value(
                                &mut self.test.scale_mode,
                                SpatialScaleMode::Wavelength,
                                "fixed wavelength",
                            );
                        });
                });
                let slider_size = [
                    columns[1].available_width(),
                    columns[1].spacing().interact_size.y,
                ];
                match self.test.scale_mode {
                    SpatialScaleMode::Speed => {
                        columns[1].add_sized(
                            slider_size,
                            egui::Slider::new(
                                &mut self.test.wave_speed,
                                haptic_protocol::MIN_WAVE_SPEED..=haptic_protocol::MAX_WAVE_SPEED,
                            )
                            .logarithmic(true)
                            .suffix(" m/s")
                            .text("speed"),
                        );
                    }
                    SpatialScaleMode::Wavelength => {
                        columns[1].add_sized(
                            slider_size,
                            egui::Slider::new(
                                &mut self.test.wavelength_m,
                                haptic_protocol::MIN_WAVELENGTH_M
                                    ..=haptic_protocol::MAX_WAVELENGTH_M,
                            )
                            .logarithmic(true)
                            .suffix(" m")
                            .text("wavelength"),
                        );
                    }
                }
            });
        } else {
            ui.columns(2, |columns| {
                let speed_size = [
                    columns[0].available_width(),
                    columns[0].spacing().interact_size.y,
                ];
                columns[0].add_sized(
                    speed_size,
                    egui::Slider::new(
                        &mut self.test.wave_speed,
                        haptic_protocol::MIN_WAVE_SPEED..=haptic_protocol::MAX_WAVE_SPEED,
                    )
                    .logarithmic(true)
                    .suffix(" m/s")
                    .text("wave speed"),
                );
                columns[1].horizontal(|ui| {
                    ui.checkbox(&mut self.test.orbit, "orbit");
                    let slider_size = [ui.available_width(), ui.spacing().interact_size.y];
                    ui.add_enabled_ui(self.test.orbit, |ui| {
                        ui.add_sized(
                            slider_size,
                            egui::Slider::new(&mut self.test.orbit_period_s, 1.0..=30.0)
                                .logarithmic(true)
                                .suffix(" s")
                                .text("period"),
                        );
                    });
                });
            });
        }

        ui.columns(2, |columns| {
            let knee_size = [
                columns[0].available_width(),
                columns[0].spacing().interact_size.y,
            ];
            columns[0].add_sized(
                knee_size,
                egui::Slider::new(
                    &mut self.test.atten_d0_m,
                    haptic_protocol::MIN_ATTEN_D0_M..=haptic_protocol::MAX_ATTEN_D0_M,
                )
                .logarithmic(true)
                .suffix(" m")
                .text("decay knee"),
            );
            let exponent_size = [
                columns[1].available_width(),
                columns[1].spacing().interact_size.y,
            ];
            columns[1].add_sized(
                exponent_size,
                egui::Slider::new(
                    &mut self.test.atten_exponent,
                    haptic_protocol::MIN_ATTEN_EXPONENT..=haptic_protocol::MAX_ATTEN_EXPONENT,
                )
                .text("exponent"),
            );
        });

        if self.test.stimulus_type == StimulusType::TravellingWave {
            ui.columns(2, |columns| {
                columns[0].checkbox(&mut self.test.orbit, "orbit");
                let period_size = [
                    columns[1].available_width(),
                    columns[1].spacing().interact_size.y,
                ];
                columns[1].add_enabled_ui(self.test.orbit, |ui| {
                    ui.add_sized(
                        period_size,
                        egui::Slider::new(&mut self.test.orbit_period_s, 1.0..=30.0)
                            .logarithmic(true)
                            .suffix(" s")
                            .text("orbit period"),
                    )
                });
            });
        }
    }
}

fn draw_table(
    ui: &mut egui::Ui,
    layout: &LayoutView,
    voices: &[VoiceView],
    routing: Option<&RoutingView>,
    relative_analytic: Option<&[(f32, f32); TRANSDUCERS]>,
) -> TableInteraction {
    let mut interaction = TableInteraction::default();
    let size = ui.available_size();
    let (response, painter) = ui.allocate_painter(size, egui::Sense::click_and_drag());
    // Eight screen pixels are enough to keep the source cross and outline
    // clear of the panel edge. Everything else should be usable display area.
    let avail = response.rect.shrink(DISPLAY_EDGE_PADDING_PX);

    // The configured table is the viewport. Only transducers outside it grow
    // the world bounds, and then only enough to keep their circles visible.
    let (min_x, min_y, max_x, max_y) = table_world_bounds(layout);
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
    let radius = (TRANSDUCER_RADIUS_M * scale).clamp(5.0, 40.0);
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
        let color = if let Some(relative_analytic) = relative_analytic {
            // Hue retains the original zero-phase-blue convention. Magnitude
            // is the measured final output and already includes layout gain.
            let (re, im) = relative_analytic[i];
            let hue = ZERO_PHASE_HUE_DEG + im.atan2(re).to_degrees();
            let amp = re.hypot(im);
            let vis = (amp * 2.0).clamp(0.0, 1.0).sqrt();
            let lightness = 0.25 + 0.50 * vis;
            let chroma = 0.03 + 0.14 * vis;
            oklch_to_color(lightness, chroma, hue)
        } else {
            egui::Color32::from_gray(60)
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

    response.on_hover_text(
        "Drag to move the test source (○ requested, ✚ effective).\n\
         Left-click a transducer to route output 1 (L); right-click for output 2 (R).",
    );

    interaction
}

fn table_world_bounds(layout: &LayoutView) -> (f32, f32, f32, f32) {
    let (mut min_x, mut min_y) = (0.0_f32, 0.0_f32);
    let (mut max_x, mut max_y) = layout.table_m;
    for &(x, y) in &layout.positions {
        min_x = min_x.min(x - TRANSDUCER_RADIUS_M);
        min_y = min_y.min(y - TRANSDUCER_RADIUS_M);
        max_x = max_x.max(x + TRANSDUCER_RADIUS_M);
        max_y = max_y.max(y + TRANSDUCER_RADIUS_M);
    }
    (min_x, min_y, max_x, max_y)
}

#[cfg(test)]
mod table_layout_tests {
    use super::{table_world_bounds, LayoutView, TRANSDUCERS, TRANSDUCER_RADIUS_M};

    #[test]
    fn table_bounds_add_no_padding_for_inset_transducers() {
        let layout = LayoutView {
            positions: [(0.5, 1.0); TRANSDUCERS],
            table_m: (1.0, 2.0),
        };
        assert_eq!(table_world_bounds(&layout), (0.0, 0.0, 1.0, 2.0));
    }

    #[test]
    fn table_bounds_keep_outside_transducers_visible() {
        let mut positions = [(0.5, 1.0); TRANSDUCERS];
        positions[0] = (-0.2, 2.3);
        let layout = LayoutView {
            positions,
            table_m: (1.0, 2.0),
        };
        let (min_x, min_y, max_x, max_y) = table_world_bounds(&layout);
        assert!((min_x - (-0.2 - TRANSDUCER_RADIUS_M)).abs() < f32::EPSILON);
        assert_eq!(min_y, 0.0);
        assert_eq!(max_x, 1.0);
        assert!((max_y - (2.3 + TRANSDUCER_RADIUS_M)).abs() < f32::EPSILON);
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        print_usage();
        return Ok(());
    }
    let options = parse_options(
        args,
        std::env::var("HAPTIC_SOCKET_PATH").ok(),
        std::env::var("HAPTIC_SERVER_BIN").ok(),
        std::process::id(),
    )
    .unwrap_or_else(|error| {
        eprintln!("{error}");
        print_usage();
        std::process::exit(2);
    });
    let socket_path = options.socket_path.clone();
    eprintln!("Viewer socket: {socket_path}");
    let server = ServerSupervisor::new(options);
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
            .with_min_inner_size([520.0, 720.0])
            .with_title("Haptic"),
        vsync: true,
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native(
        "Haptic",
        options,
        Box::new(|_cc| {
            Ok(Box::new(ViewerApp {
                shared,
                server,
                fps: RateCounter::default(),
                test: TestControls::default(),
                instance_id,
                reference_rule: ReferenceRule::StickyNewest,
                reference: None,
                reference_missing_since: None,
                silent_reference_snapshots: 0,
                sounding_params: (
                    haptic_protocol::DEFAULT_TEST_NOTE,
                    100,
                    StimulusType::Wave,
                    5.0,
                ),
                last_live_config: (
                    5.0,
                    SpatialScaleMode::Speed,
                    0.2,
                    haptic_protocol::DEFAULT_ATTEN_D0_M,
                    haptic_protocol::DEFAULT_ATTEN_EXPONENT,
                ),
            }))
        }),
    )
}

#[cfg(test)]
mod app_option_tests {
    use super::*;

    #[test]
    fn normal_mode_uses_production_socket_and_manages_a_server() {
        let options = parse_options(Vec::new(), None, None, 42).unwrap();
        assert_eq!(options.socket_path, SOCKET_PATH);
        assert!(!options.connect_only);
        assert!(!options.dummy_audio);
    }

    #[test]
    fn headless_mode_gets_an_isolated_application_socket() {
        let options = parse_options(vec!["--headless".into()], None, None, 42).unwrap();
        assert_eq!(options.socket_path, "/tmp/haptic-vst-app-42.sock");
        assert!(options.dummy_audio);
    }

    #[test]
    fn explicit_values_override_environment_defaults() {
        let options = parse_options(
            vec![
                "--connect-only".into(),
                "--socket".into(),
                "/tmp/explicit.sock".into(),
                "--server-bin".into(),
                "/tmp/explicit-server".into(),
                "--config".into(),
                "/tmp/layout.toml".into(),
            ],
            Some("/tmp/environment.sock".into()),
            Some("/tmp/environment-server".into()),
            42,
        )
        .unwrap();
        assert!(options.connect_only);
        assert_eq!(options.socket_path, "/tmp/explicit.sock");
        assert_eq!(
            options.server_bin,
            Some(PathBuf::from("/tmp/explicit-server"))
        );
        assert_eq!(options.config_path, PathBuf::from("/tmp/layout.toml"));
    }

    #[test]
    fn missing_option_value_is_rejected() {
        let error = parse_options(vec!["--server-bin".into()], None, None, 42).unwrap_err();
        assert_eq!(error, "--server-bin requires a value");
    }
}
