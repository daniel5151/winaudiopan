//! WinAudioPan - Dynamically pan audio per-application based on window
//! on-screen position.

#![windows_subsystem = "windows"]

mod audio;
mod claims;
mod console;
mod tray;
mod window;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use clap::Parser;

use crate::audio::run_audio_loop;
use crate::claims::ClaimTracker;
use crate::claims::start_claim_server;
use crate::tray::TrayContext;

/// Command line arguments for WinAudioPan.
#[derive(Parser, Debug)]
#[command(
    name = "winaudiopan",
    author,
    version,
    about = "Dynamic window-based audio panning for Windows",
    long_about = None
)]
struct Cli {
    /// Force allocate and display a console log window.
    #[arg(long)]
    console: bool,

    /// Run in the foreground without a system tray icon.
    #[arg(long, alias = "cli")]
    no_tray: bool,
}

fn main() {
    // Attempt to attach to parent console so CLI invocations (including --help
    // or --version) output to terminal.
    let _ = console::init(false);

    let cli = Cli::parse();

    // If --console was explicitly requested, allocate a dedicated console
    // window if not already attached.
    if cli.console {
        let _ = console::init(true);
    }

    let mut builder = pretty_env_logger::formatted_builder();
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        builder.parse_filters(&rust_log);
    } else {
        builder.filter_level(log::LevelFilter::Info);
    }
    builder.init();

    log::info!("Starting WinAudioPan v{}...", env!("CARGO_PKG_VERSION"));

    let running = Arc::new(AtomicBool::new(true));
    let enabled = Arc::new(AtomicBool::new(true));
    let reset_requested = Arc::new(AtomicBool::new(false));
    let claim_tracker = Arc::new(ClaimTracker::default());

    let r_sig = running.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        log::info!("Termination signal received. Shutting down...");
        r_sig.store(false, Ordering::SeqCst);
        tray::request_tray_exit();
    }) {
        log::warn!("Could not set Ctrl+C signal handler: {err}");
    }

    if let Err(err) = start_claim_server(claim_tracker.clone(), running.clone()) {
        log::warn!("Failed to initialize userscript coordination server: {err}");
    }

    if cli.no_tray {
        log::info!("Running in foreground CLI mode (no system tray). Press Ctrl+C to exit.");
        if let Err(err) = run_audio_loop(running, enabled, reset_requested, claim_tracker) {
            eprintln!("Audio tracking error: {err}");
            std::process::exit(1);
        }
    } else {
        let r_thread = running.clone();
        let e_thread = enabled.clone();
        let res_thread = reset_requested.clone();
        let c_thread = claim_tracker.clone();

        let audio_thread = thread::spawn(move || {
            if let Err(err) = run_audio_loop(r_thread, e_thread, res_thread, c_thread) {
                log::error!("Audio tracking loop terminated with error: {err}");
            }
        });

        let tray_context = TrayContext {
            running: running.clone(),
            enabled: enabled.clone(),
            reset_requested: reset_requested.clone(),
            claim_tracker: claim_tracker.clone(),
        };

        if let Err(err) = tray::run_tray(tray_context) {
            log::error!("System tray loop encountered an error: {err}");
        }

        running.store(false, Ordering::SeqCst);
        let _ = audio_thread.join();
    }
}
