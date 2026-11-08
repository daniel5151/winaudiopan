//! WASAPI audio endpoint management, stereo panning calculations, and audio
//! session volume control.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use windows::Win32::Media::Audio::AudioSessionStateActive;
use windows::Win32::Media::Audio::IAudioSessionControl2;
use windows::Win32::Media::Audio::IAudioSessionManager2;
use windows::Win32::Media::Audio::IChannelAudioVolume;
use windows::Win32::Media::Audio::IMMDevice;
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::Media::Audio::MMDeviceEnumerator;
use windows::Win32::Media::Audio::eMultimedia;
use windows::Win32::Media::Audio::eRender;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Com::COINIT_MULTITHREADED;
use windows::Win32::System::Com::CoCreateInstance;
use windows::Win32::System::Com::CoInitializeEx;
use windows::Win32::System::Com::CoUninitialize;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
use windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN;
use windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN;
use windows::core::Interface;

use crate::claims::ClaimTracker;
use crate::window::ProcessTreeCache;
use crate::window::WindowInfo;
use crate::window::WindowTracker;
use crate::window::get_window_monitor_bounds;
use crate::window::get_windows_by_pid;
use crate::window::resolve_windows_for_pid;

/// Maximum stereo panning strength factor (0.0 = completely center, 1.0 = hard
/// 100% pan).
const PAN_STRENGTH: f64 = 1.0;

/// Polling interval for tracking window positions and updating audio panning.
const POLL_INTERVAL: Duration = Duration::from_millis(75);

/// Minimum volume change threshold before dispatching a COM update.
const VOLUME_EPSILON: f32 = 0.015;

/// RAII wrapper for COM thread initialization.
pub struct ComGuard;

impl ComGuard {
    /// Initializes COM for multithreaded operation.
    pub fn new() -> anyhow::Result<Self> {
        // SAFETY: Initializing COM for the current worker thread with
        // multithreaded concurrency.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.ok().context("Failed to initialize COM on thread")?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: Balances the preceding successful CoInitializeEx call.
        unsafe { CoUninitialize() };
    }
}

/// Helper iterating over all 2-channel audio session volume controls.
fn for_each_session_volume(
    session_manager: &IAudioSessionManager2,
    mut callback: impl FnMut(&IChannelAudioVolume),
) {
    // SAFETY: Enumerating audio sessions.
    let enumerator = match unsafe { session_manager.GetSessionEnumerator() } {
        Ok(e) => e,
        Err(_) => return,
    };

    // SAFETY: Querying session count.
    let count = match unsafe { enumerator.GetCount() } {
        Ok(c) => c,
        Err(_) => return,
    };

    for i in 0..count {
        // SAFETY: Fetching session at index.
        let Ok(control) = (unsafe { enumerator.GetSession(i) }) else {
            continue;
        };
        let Ok(channel_vol) = control.cast::<IChannelAudioVolume>() else {
            continue;
        };

        // SAFETY: Querying channel count.
        let channel_count = match unsafe { channel_vol.GetChannelCount() } {
            Ok(c) => c,
            Err(_) => continue,
        };

        if channel_count == 2 {
            callback(&channel_vol);
        }
    }
}

/// Helper iterating over active 2-channel stereo audio sessions with owning
/// PID.
fn for_each_active_session(
    session_manager: &IAudioSessionManager2,
    self_pid: u32,
    mut callback: impl FnMut(u32, &IChannelAudioVolume),
) {
    // SAFETY: Enumerating active sessions.
    let enumerator = match unsafe { session_manager.GetSessionEnumerator() } {
        Ok(e) => e,
        Err(_) => return,
    };

    // SAFETY: Reading number of sessions.
    let count = match unsafe { enumerator.GetCount() } {
        Ok(c) => c,
        Err(_) => return,
    };

    for i in 0..count {
        // SAFETY: Getting session control interface.
        let Ok(session_control) = (unsafe { enumerator.GetSession(i) }) else {
            continue;
        };
        let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>() else {
            continue;
        };

        // SAFETY: Checking session playback state.
        let Ok(state) = (unsafe { session_control2.GetState() }) else {
            continue;
        };
        if state != AudioSessionStateActive {
            continue;
        }

        // SAFETY: Fetching process ID responsible for this audio session.
        let Ok(pid) = (unsafe { session_control2.GetProcessId() }) else {
            continue;
        };
        if pid == 0 || pid == self_pid {
            continue;
        }

        let Ok(channel_volume) = session_control.cast::<IChannelAudioVolume>() else {
            continue;
        };

        // SAFETY: Querying session channel count.
        let channel_count = match unsafe { channel_volume.GetChannelCount() } {
            Ok(c) => c,
            Err(_) => continue,
        };
        if channel_count != 2 {
            continue;
        }

        callback(pid, &channel_volume);
    }
}

/// Computes stereo balance volumes `[left, right]` given window bounds and
/// screen bounds using an edge-aware panning formula scaled by available travel
/// range.
pub fn compute_pan_volumes(
    window: Option<&WindowInfo>,
    fallback_screen_left: i32,
    fallback_screen_width: i32,
) -> (f32, f32) {
    let pan = match window {
        Some(win) if !win.is_minimized => {
            let (monitor_left, monitor_width) =
                get_window_monitor_bounds(win.hwnd, fallback_screen_left, fallback_screen_width);

            let width = ((win.rect.right - win.rect.left) as f64).max(0.0);
            let screen_w = (monitor_width.max(1)) as f64;
            let norm_w = (width / screen_w).max(0.0);

            // If the window fills nearly the entire monitor (or spans wider),
            // treat as centered
            if norm_w >= 0.96 {
                0.0
            } else {
                let win_center_x = win.rect.left as f64 + (width / 2.0);
                let norm_c = (win_center_x - monitor_left as f64) / screen_w;
                // Scale normalized center offset by the remaining travel margin
                // (1.0 - norm_w)
                let available_travel = 1.0 - norm_w;
                let raw_pan = (2.0 * (norm_c - 0.5)) / available_travel;
                let clamped = raw_pan.clamp(-1.0, 1.0);
                // Snap threshold so windows touching or nearly touching bezels
                // hit 100% hard isolation
                if clamped >= 0.97 {
                    1.0
                } else if clamped <= -0.97 {
                    -1.0
                } else {
                    clamped
                }
            }
        }
        // Minimized windows or audio processes without a window default to
        // centered (pan = 0.0)
        _ => 0.0,
    };

    let effective_pan = (pan * PAN_STRENGTH).clamp(-1.0, 1.0);

    // Linear stereo balance pan: preserves 100% volume on centered and panning
    // ear
    if effective_pan <= 0.0 {
        // Panned left: left is full, right is reduced
        (1.0, (1.0 + effective_pan) as f32)
    } else {
        // Panned right: right is full, left is reduced
        ((1.0 - effective_pan) as f32, 1.0)
    }
}

/// Resets stereo balance for all audio sessions back to centered `[1.0, 1.0]`.
pub fn reset_all_sessions(session_manager: &IAudioSessionManager2) {
    let default_volumes = [1.0f32, 1.0f32];
    for_each_session_volume(session_manager, |channel_vol| {
        // SAFETY: Restoring balanced stereo levels.
        unsafe {
            let _ = channel_vol.SetAllVolumes(&default_volumes, std::ptr::null());
        }
    });
}

/// Main audio tracking loop: monitors desktop windows and applies WASAPI
/// stereo panning to active audio sessions.
pub fn run_audio_loop(
    running: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    reset_requested: Arc<AtomicBool>,
    claim_tracker: Arc<ClaimTracker>,
) -> anyhow::Result<()> {
    let _com = ComGuard::new()?;

    // SAFETY: Creating MMDeviceEnumerator instance to access default audio
    // endpoint.
    let device_enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("Failed to create MMDeviceEnumerator")?;

    // SAFETY: Querying default audio playback endpoint.
    let default_device: IMMDevice =
        unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
            .context("Failed to get default audio endpoint")?;

    // SAFETY: Activating IAudioSessionManager2 interface on the default audio
    // endpoint.
    let session_manager: IAudioSessionManager2 =
        unsafe { default_device.Activate(CLSCTX_ALL, None) }
            .context("Failed to activate IAudioSessionManager2")?;

    log::info!("Connected to Windows Audio Session Manager.");

    let self_pid = std::process::id();
    let mut tracker = WindowTracker::default();
    let mut process_tree = ProcessTreeCache::new();

    while running.load(Ordering::SeqCst) {
        // Check if an immediate audio balance reset was requested via tray menu
        if reset_requested.swap(false, Ordering::SeqCst) {
            log::info!("Resetting all audio session channels to default balance...");
            reset_all_sessions(&session_manager);
        }

        // If panning is currently disabled by the user, sleep and continue
        if !enabled.load(Ordering::SeqCst) {
            thread::sleep(POLL_INTERVAL);
            continue;
        }

        // SAFETY: Querying virtual desktop coordinates to support single and
        // multi-monitor setups.
        let screen_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        // SAFETY: Querying virtual desktop span width.
        let screen_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        // SAFETY: Querying current foreground window handle.
        let foreground_hwnd = unsafe { GetForegroundWindow() };

        let windows_by_pid = get_windows_by_pid();
        process_tree.refresh_if_stale();

        for_each_active_session(&session_manager, self_pid, |pid, channel_volume| {
            process_tree.refresh_if_missing(pid);
            let exe_name = process_tree
                .get(pid)
                .map(|n| n.exe_name.as_str())
                .unwrap_or("unknown");

            // Check if this application is currently claimed by an in-tab
            // userscript
            if claim_tracker.is_claimed(exe_name) {
                let mut current_volumes = [0.0f32; 2];
                // SAFETY: Reading channel volumes to ensure balanced master
                // volume.
                if unsafe { channel_volume.GetAllVolumes(&mut current_volumes) }.is_ok() {
                    let diff_l = (current_volumes[0] - 1.0).abs();
                    let diff_r = (current_volumes[1] - 1.0).abs();
                    if diff_l > VOLUME_EPSILON || diff_r > VOLUME_EPSILON {
                        let default_volumes = [1.0f32, 1.0f32];
                        // SAFETY: Restoring 1.0 balance while userscript has
                        // control.
                        if unsafe {
                            channel_volume.SetAllVolumes(&default_volumes, std::ptr::null())
                        }
                        .is_ok()
                        {
                            log::info!(
                                "PID {pid} ({exe_name}) claimed by userscript -> Reset master audio balance to [1.0, 1.0]"
                            );
                        }
                    }
                }
                return;
            }

            let candidate_windows = resolve_windows_for_pid(pid, &windows_by_pid, &process_tree);
            let window = candidate_windows
                .and_then(|candidates| tracker.choose_window(pid, candidates, foreground_hwnd));

            let (target_l, target_r) = compute_pan_volumes(window, screen_left, screen_width);

            let mut current_volumes = [0.0f32; 2];
            // SAFETY: Reading current channel volume levels.
            if unsafe { channel_volume.GetAllVolumes(&mut current_volumes) }.is_ok() {
                let diff_l = (current_volumes[0] - target_l).abs();
                let diff_r = (current_volumes[1] - target_r).abs();

                if diff_l > VOLUME_EPSILON || diff_r > VOLUME_EPSILON {
                    let new_volumes = [target_l, target_r];
                    // SAFETY: Setting left and right channel audio levels.
                    if unsafe { channel_volume.SetAllVolumes(&new_volumes, std::ptr::null()) }
                        .is_ok()
                    {
                        let win_title = window
                            .map(|w| {
                                if w.is_minimized {
                                    "[Minimized]"
                                } else if w.title.is_empty() {
                                    "[Untitled Window]"
                                } else {
                                    w.title.as_str()
                                }
                            })
                            .unwrap_or("[No Window / Centered]");

                        log::info!(
                            "PID {pid} ({exe_name}) '{win_title}' -> L: {:.2}, R: {:.2}",
                            target_l,
                            target_r
                        );
                    }
                }
            }
        });

        tracker.update_rects(windows_by_pid.values().flatten());
        thread::sleep(POLL_INTERVAL);
    }

    log::info!("Exiting: Resetting all audio session channels to default balance...");
    reset_all_sessions(&session_manager);
    log::info!("Done. Goodbye!");

    Ok(())
}
