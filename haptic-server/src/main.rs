use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

mod audio;
mod config;
mod engine;
mod ipc;

use config::TransducerLayout;
use engine::StimulusEngine;

const DEFAULT_CONFIG_PATH: &str = "haptic.toml";

#[derive(Debug, PartialEq)]
struct ServerOptions {
    test_tone: bool,
    dummy_audio: bool,
    managed_lifetime_stdin: bool,
    config_path: PathBuf,
    socket_path: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Starting Haptic VST Server");

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return Ok(());
    }
    let options = parse_options(
        args,
        std::env::var("HAPTIC_SOCKET_PATH").ok(),
        std::process::id(),
    )?;
    let config_path = options.config_path.clone();
    if options.dummy_audio {
        eprintln!("Headless test mode: physical audio devices will not be opened");
    }
    eprintln!("Server socket: {}", options.socket_path);

    // Load the transducer layout: a missing file falls back to the built-in
    // default (4x8 grid over 1m x 2m); a present-but-invalid file is a hard
    // error so a typo can't silently drive the wrong layout.
    let layout = if config_path.exists() {
        match config::load_layout(&config_path) {
            Ok(layout) => {
                eprintln!("Loaded transducer layout from {}", config_path.display());
                layout
            }
            Err(e) => {
                eprintln!("Invalid config {}: {}", config_path.display(), e);
                return Err(e.into());
            }
        }
    } else {
        eprintln!(
            "No config at {}, using default layout (4x8 grid over 1m x 2m)",
            config_path.display()
        );
        TransducerLayout::default()
    };

    // Create shared shutdown flag
    let running = Arc::new(AtomicBool::new(true));

    // A supervising GUI starts the server with piped stdin and this flag. If
    // the supervisor exits normally or crashes, the OS closes the pipe and the
    // server shuts down rather than continuing to drive hardware as an orphan.
    if options.managed_lifetime_stdin {
        let managed_running = running.clone();
        thread::spawn(move || managed_lifetime_stdin(managed_running));
    }

    // Create stimulus engine - the IPC thread gets the command producer and
    // voice-snapshot consumer, the config watcher gets the layout producer
    let (engine, command_producer, engine_layout_producer, voice_consumer) =
        StimulusEngine::new(layout);

    // Levels path: audio callback → IPC thread → connected clients
    let (levels_producer, levels_consumer) = rtrb::RingBuffer::new(256);

    // Layout path to the IPC thread (for broadcast to clients); the engine
    // has its own ring, so the watcher feeds both
    let (ipc_layout_producer, ipc_layout_consumer) = rtrb::RingBuffer::new(4);

    // Device output channel count, published by the audio loop once the
    // device is opened, broadcast to clients by the IPC thread
    let device_channels = Arc::new(AtomicU16::new(0));

    // Start IPC listener thread
    let device_channels_for_ipc = device_channels.clone();
    let ipc_handle = {
        let running = running.clone();
        let socket_path = options.socket_path.clone();
        thread::spawn(move || {
            if let Err(e) = ipc::listen_loop(
                &socket_path,
                running,
                command_producer,
                levels_consumer,
                voice_consumer,
                layout,
                ipc_layout_consumer,
                device_channels_for_ipc,
            ) {
                eprintln!("IPC error: {}", e);
            }
        })
    };

    // Config watcher: hot-reload the layout when the file's mtime changes
    let watcher_handle = {
        let running = running.clone();
        thread::spawn(move || {
            config_watcher(
                running,
                config_path,
                engine_layout_producer,
                ipc_layout_producer,
            )
        })
    };

    // Set up signal handler for graceful shutdown
    let running_for_signal = running.clone();
    ctrlc::set_handler(move || {
        eprintln!("Received interrupt signal, shutting down...");
        running_for_signal.store(false, Ordering::Relaxed);
    })?;

    // Run audio loop on main thread (highest priority)
    if options.dummy_audio {
        audio::run_dummy_audio_loop(
            engine,
            running.clone(),
            options.test_tone,
            levels_producer,
            device_channels,
        );
    } else if let Err(e) = audio::run_audio_loop(
        engine,
        running.clone(),
        options.test_tone,
        levels_producer,
        device_channels,
    ) {
        eprintln!("Audio error: {}", e);
    }

    // Cleanup
    running.store(false, Ordering::Relaxed);
    ipc_handle.join().ok();
    watcher_handle.join().ok();

    eprintln!("Haptic VST Server stopped");
    Ok(())
}

fn print_usage() {
    eprintln!(
        "Usage: haptic-server [--config PATH] [--test-tone] [--headless|--dummy-audio] [--socket PATH] [--managed-lifetime-stdin]\n\
         \n\
         --headless, --dummy-audio  Use a timed 48 kHz/32-channel memory sink; no hardware.\n\
         --socket PATH              Override the Unix socket/lock namespace.\n\
         --managed-lifetime-stdin   Exit when a supervising process closes stdin.\n\
         HAPTIC_SOCKET_PATH         Environment alternative to --socket.\n\
         \n\
         Headless mode defaults to /tmp/haptic-vst-test-<pid>.sock and therefore\n\
         never contends with the production /tmp/haptic-vst.sock endpoint."
    );
}

fn parse_options(
    args: impl IntoIterator<Item = String>,
    environment_socket: Option<String>,
    process_id: u32,
) -> Result<ServerOptions, Box<dyn std::error::Error>> {
    let mut test_tone = false;
    let mut dummy_audio = false;
    let mut managed_lifetime_stdin = false;
    let mut config_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    let mut socket_path = None;
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--test-tone" => test_tone = true,
            "--headless" | "--dummy-audio" => dummy_audio = true,
            "--managed-lifetime-stdin" => managed_lifetime_stdin = true,
            "--config" => {
                config_path = PathBuf::from(next_option_value(&mut args, "--config")?);
            }
            "--socket" => socket_path = Some(next_option_value(&mut args, "--socket")?),
            unknown => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown argument {unknown}; use --help"),
                )
                .into());
            }
        }
    }

    let socket_path = socket_path.or(environment_socket).unwrap_or_else(|| {
        if dummy_audio {
            format!("/tmp/haptic-vst-test-{process_id}.sock")
        } else {
            haptic_protocol::SOCKET_PATH.to_string()
        }
    });

    Ok(ServerOptions {
        test_tone,
        dummy_audio,
        managed_lifetime_stdin,
        config_path,
        socket_path,
    })
}

fn managed_lifetime_stdin(running: Arc<AtomicBool>) {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => {
                eprintln!("Managed supervisor closed; shutting down server");
                running.store(false, Ordering::Relaxed);
                return;
            }
            Ok(_) => {
                // Input is not a command protocol. Reading and ignoring bytes
                // simply keeps EOF as the lifetime signal.
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                eprintln!("Managed supervisor pipe failed ({error}); shutting down server");
                running.store(false, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn next_option_value(
    args: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{option} requires a value"),
            )
            .into()
        })
}

/// Poll the config file's mtime (~1 Hz); on change, parse it off the audio
/// thread and push the new layout into the engine's layout ring. Parse
/// errors leave the current layout running.
fn config_watcher(
    running: Arc<AtomicBool>,
    path: PathBuf,
    mut engine_producer: rtrb::Producer<TransducerLayout>,
    mut ipc_producer: rtrb::Producer<TransducerLayout>,
) {
    let mtime_of = |path: &std::path::Path| -> Option<SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    };

    let mut last_mtime = mtime_of(&path);
    while running.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(1000));
        let mtime = mtime_of(&path);
        if mtime.is_some() && mtime != last_mtime {
            last_mtime = mtime;
            match config::load_layout(&path) {
                Ok(layout) => {
                    if engine_producer.push(layout).is_ok() {
                        eprintln!("Config reloaded from {}", path.display());
                    }
                    let _ = ipc_producer.push(layout);
                }
                Err(e) => {
                    eprintln!("Config reload failed (keeping current layout): {}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_mode_uses_real_audio_and_production_socket() {
        let options = parse_options(Vec::new(), None, 123).unwrap();
        assert!(!options.dummy_audio);
        assert!(!options.managed_lifetime_stdin);
        assert_eq!(options.socket_path, haptic_protocol::SOCKET_PATH);
    }

    #[test]
    fn headless_mode_uses_dummy_audio_and_per_process_test_socket() {
        let options = parse_options(vec!["--headless".into()], None, 123).unwrap();
        assert!(options.dummy_audio);
        assert_eq!(options.socket_path, "/tmp/haptic-vst-test-123.sock");
    }

    #[test]
    fn explicit_test_socket_overrides_the_mode_default() {
        let options = parse_options(
            vec![
                "--dummy-audio".into(),
                "--socket".into(),
                "/tmp/haptic-vst-test.sock".into(),
            ],
            Some("/tmp/from-environment.sock".into()),
            123,
        )
        .unwrap();
        assert!(options.dummy_audio);
        assert_eq!(options.socket_path, "/tmp/haptic-vst-test.sock");
    }

    #[test]
    fn managed_lifetime_is_explicit_and_independent_of_audio_profile() {
        let options = parse_options(vec!["--managed-lifetime-stdin".into()], None, 123).unwrap();
        assert!(options.managed_lifetime_stdin);
        assert!(!options.dummy_audio);
        assert_eq!(options.socket_path, haptic_protocol::SOCKET_PATH);
    }
}
