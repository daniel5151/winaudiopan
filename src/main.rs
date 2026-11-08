//! WinAudioPan - Dynamically pan audio per-application based on window
//! on-screen position.

#![windows_subsystem = "windows"]

mod console;
mod tray;

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::mem::size_of;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS;
use windows::Win32::Graphics::Dwm::DwmGetWindowAttribute;
use windows::Win32::Graphics::Gdi::GetMonitorInfoW;
use windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST;
use windows::Win32::Graphics::Gdi::MONITORINFO;
use windows::Win32::Graphics::Gdi::MonitorFromWindow;
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
use windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
use windows::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
use windows::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
use windows::Win32::System::Diagnostics::ToolHelp::Process32NextW;
use windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
use windows::Win32::UI::WindowsAndMessaging::GW_OWNER;
use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
use windows::Win32::UI::WindowsAndMessaging::GetWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextLengthW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::Win32::UI::WindowsAndMessaging::IsIconic;
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
use windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN;
use windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use windows::core::BOOL;
use windows::core::Interface;

use crate::console::ConsoleManager;
use crate::tray::TrayContext;

/// Maximum stereo panning strength factor (0.0 = completely center, 1.0 = hard
/// 100% pan).
const PAN_STRENGTH: f64 = 1.0;

/// Polling interval for tracking window positions and updating audio panning.
const POLL_INTERVAL: Duration = Duration::from_millis(75);

/// Minimum volume change threshold before dispatching a COM update.
const VOLUME_EPSILON: f32 = 0.015;

/// Local HTTP coordination server port for userscripts.
const SERVER_PORT: u16 = 38400;

/// Time-to-live duration for an active userscript claim heartbeat.
const CLAIM_TTL: Duration = Duration::from_millis(4000);

/// RAII wrapper for COM thread initialization.
struct ComGuard;

impl ComGuard {
    /// Initializes COM for multithreaded operation.
    fn new() -> anyhow::Result<Self> {
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

/// Tracks applications claimed by in-tab userscripts per tab ID.
#[derive(Default)]
pub struct ClaimTracker {
    /// Map of claimed application -> (map of tab_id -> last_heartbeat).
    claims: Mutex<HashMap<String, HashMap<String, Instant>>>,
}

impl ClaimTracker {
    /// Checks whether an executable name is currently claimed by any active
    /// tab.
    pub fn is_claimed(&self, exe_name: &str) -> bool {
        let exe_lower = exe_name.to_lowercase();
        let mut guard = self.claims.lock().unwrap();
        guard.retain(|_, tabs| {
            tabs.retain(|_, time| time.elapsed() < CLAIM_TTL);
            !tabs.is_empty()
        });
        guard.keys().any(|claimed| exe_lower.contains(claimed))
    }

    /// Checks whether any applications or browser tabs are currently active.
    pub fn has_any_claims(&self) -> bool {
        let mut guard = self.claims.lock().unwrap();
        guard.retain(|_, tabs| {
            tabs.retain(|_, time| time.elapsed() < CLAIM_TTL);
            !tabs.is_empty()
        });
        !guard.is_empty()
    }

    /// Records or refreshes a claim for a specific application tab.
    pub fn record_claim(&self, app: &str, tab_id: &str) {
        let mut guard = self.claims.lock().unwrap();
        let tabs = guard.entry(app.to_lowercase()).or_default();
        let was_empty = tabs.is_empty();
        tabs.insert(tab_id.to_string(), Instant::now());
        if was_empty {
            log::info!("'{app}' claimed by in-tab userscript (bypassing OS master pan)");
        }
    }

    /// Explicitly releases a claim when a specific tab pauses or closes.
    pub fn release_claim(&self, app: &str, tab_id: &str) {
        let mut guard = self.claims.lock().unwrap();
        if let Some(tabs) = guard.get_mut(&app.to_lowercase()) {
            tabs.remove(tab_id);
            if tabs.is_empty() {
                guard.remove(&app.to_lowercase());
                log::info!("'{app}' released by userscripts (resuming OS master pan)");
            }
        }
    }
}

/// Spawns a background TCP server to receive claim and release heartbeats from
/// userscripts.
fn start_claim_server(tracker: Arc<ClaimTracker>, running: Arc<AtomicBool>) -> anyhow::Result<()> {
    let listener = match TcpListener::bind(("127.0.0.1", SERVER_PORT)) {
        Ok(l) => l,
        Err(err) => {
            log::warn!(
                "Could not bind HTTP claim server on port {SERVER_PORT}: {err}. Userscript coordination disabled."
            );
            return Ok(());
        }
    };
    listener.set_nonblocking(true)?;

    thread::spawn(move || {
        log::info!("Userscript coordination server listening on http://127.0.0.1:{SERVER_PORT}");
        let mut buf = [0u8; 1024];
        while running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Sockets accepted from a non-blocking listener inherit
                    // non-blocking mode on Windows.
                    // Switch to blocking with a brief timeout so read waits for
                    // the HTTP request bytes.
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);

                    let app = if req.contains("app=firefox") {
                        "firefox"
                    } else if req.contains("app=msedge") || req.contains("app=edge") {
                        "msedge"
                    } else if req.contains("app=brave") {
                        "brave"
                    } else {
                        "chrome"
                    };

                    let tab_id = if let Some(idx) = req.find("tab=") {
                        let rest = &req[idx + 4..];
                        let end = rest.find(['&', ' ', '\r', '\n']).unwrap_or(rest.len());
                        &rest[..end]
                    } else {
                        "default"
                    };

                    if req.contains("/claim") {
                        tracker.record_claim(app, tab_id);
                        let response = "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(response.as_bytes());
                    } else if req.contains("/release") {
                        tracker.release_claim(app, tab_id);
                        let response = "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(response.as_bytes());
                    } else if req.contains("/ping") || req.starts_with("OPTIONS") {
                        let response = "HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(response.as_bytes());
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(40));
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(40));
                }
            }
        }
    });

    Ok(())
}

/// Metadata about a running process.
#[derive(Debug, Clone)]
struct ProcessNode {
    /// Process ID.
    parent_pid: u32,
    /// Executable name.
    exe_name: String,
}

/// Metadata about a top-level application window.
#[derive(Debug, Clone)]
struct WindowInfo {
    /// Window handle.
    hwnd: HWND,
    /// Process ID that owns this window.
    pid: u32,
    /// Window title.
    title: String,
    /// Whether the window is minimized.
    is_minimized: bool,
    /// Window rectangular bounding box.
    rect: RECT,
}

/// Callback state passed to `EnumWindows`.
#[derive(Default)]
struct EnumWindowsState {
    /// Discovered candidate top-level windows in Z-order.
    windows: Vec<WindowInfo>,
}

/// Callback function invoked by Win32 for each top-level window.
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: We only read the pointer initialized by the caller.
    let state = unsafe { &mut *(lparam.0 as *mut EnumWindowsState) };

    // SAFETY: Querying visibility state of the given window handle.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }

    // SAFETY: Querying extended window styles to filter out tool windows.
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };
    if (ex_style & (WS_EX_TOOLWINDOW.0 as i32)) != 0 {
        return BOOL(1);
    }

    // SAFETY: Checking if this window has an owner window (e.g. child popup /
    // dialog).
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) };
    if let Ok(owner_hwnd) = owner {
        if !owner_hwnd.is_invalid() {
            return BOOL(1);
        }
    }

    // SAFETY: Querying whether the window is minimized.
    let is_minimized = unsafe { IsIconic(hwnd) }.as_bool();

    let mut rect = RECT::default();
    if !is_minimized {
        // SAFETY: DwmGetWindowAttribute gets exact frame bounds excluding
        // invisible drop shadows.
        let hr = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&raw mut rect).cast(),
                size_of::<RECT>() as u32,
            )
        };
        if hr.is_err() {
            // SAFETY: Fallback to standard GetWindowRect if DWM query fails.
            if unsafe { GetWindowRect(hwnd, &raw mut rect) }.is_err() {
                return BOOL(1);
            }
        }

        // Ignore zero-sized or inverted invisible windows.
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return BOOL(1);
        }
    }

    let mut pid = 0u32;
    // SAFETY: Retrieves the process identifier associated with the window.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&raw mut pid));
    }
    if pid == 0 {
        return BOOL(1);
    }

    let mut title = String::new();
    // SAFETY: Querying title text length.
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len > 0 {
        let mut buffer = vec![0u16; (title_len + 1) as usize];
        // SAFETY: Copying window title into allocated buffer.
        let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        if copied > 0 {
            title = String::from_utf16_lossy(&buffer[..copied as usize]);
        }
    }

    state.windows.push(WindowInfo {
        hwnd,
        pid,
        title,
        is_minimized,
        rect,
    });

    BOOL(1)
}

/// Enumerate visible top-level windows and group them by PID in Z-order.
fn get_windows_by_pid() -> (HashMap<u32, Vec<WindowInfo>>, Vec<WindowInfo>) {
    let mut state = EnumWindowsState::default();
    let lparam = LPARAM((&raw mut state) as isize);

    // SAFETY: Invoking Win32 EnumWindows with safe callback and state pointer.
    let _ = unsafe { EnumWindows(Some(enum_windows_callback), lparam) };

    let all_windows = state.windows;
    let mut map: HashMap<u32, Vec<WindowInfo>> = HashMap::new();
    for win in &all_windows {
        map.entry(win.pid).or_default().push(win.clone());
    }

    (map, all_windows)
}

/// Snapshots process hierarchy and executable names via ToolHelp32.
fn get_process_tree() -> HashMap<u32, ProcessNode> {
    let mut map = HashMap::new();

    // SAFETY: Captures snapshot of all processes running on system.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let Ok(handle) = snapshot else {
        return map;
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // SAFETY: Reading first process entry from snapshot handle.
    if unsafe { Process32FirstW(handle, &raw mut entry) }.is_ok() {
        loop {
            let name_len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe_name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);

            map.insert(
                entry.th32ProcessID,
                ProcessNode {
                    parent_pid: entry.th32ParentProcessID,
                    exe_name,
                },
            );

            // SAFETY: Reading subsequent process entry from snapshot handle.
            if unsafe { Process32NextW(handle, &raw mut entry) }.is_err() {
                break;
            }
        }
    }

    // SAFETY: Closing snapshot handle.
    unsafe {
        let _ = CloseHandle(handle);
    }

    map
}

/// Resolves candidate windows for an audio-producing PID by ascending the
/// process tree.
fn resolve_windows_for_pid<'a>(
    mut pid: u32,
    windows: &'a HashMap<u32, Vec<WindowInfo>>,
    process_tree: &HashMap<u32, ProcessNode>,
) -> Option<&'a [WindowInfo]> {
    // Check up to 6 levels of process ancestry (e.g. Chrome utility / audio
    // helper -> browser process)
    for _ in 0..6 {
        if let Some(wins) = windows.get(&pid) {
            if !wins.is_empty() {
                return Some(wins.as_slice());
            }
        }
        if let Some(node) = process_tree.get(&pid) {
            if node.parent_pid == 0 || node.parent_pid == pid {
                break;
            }
            pid = node.parent_pid;
        } else {
            break;
        }
    }
    None
}

/// Tracks window selection state across frames to provide sticky window binding
/// and detect active window dragging/motion.
#[derive(Default)]
struct WindowTracker {
    /// Last chosen HWND (`hwnd.0 as isize`) for each audio process ID.
    last_chosen: HashMap<u32, isize>,
    /// Previous bounding box for each HWND (`hwnd.0 as isize`) to detect
    /// motion.
    last_rects: HashMap<isize, RECT>,
}

impl WindowTracker {
    /// Selects the best window for an audio process from candidate windows.
    fn choose_window<'a>(
        &mut self,
        pid: u32,
        candidates: &'a [WindowInfo],
        foreground_hwnd: HWND,
    ) -> Option<&'a WindowInfo> {
        if candidates.is_empty() {
            return None;
        }

        if candidates.len() == 1 {
            let chosen = &candidates[0];
            self.last_chosen.insert(pid, chosen.hwnd.0 as isize);
            return Some(chosen);
        }

        // Criterion 1: Active motion (user is dragging/moving one of the
        // windows)
        for win in candidates {
            if win.is_minimized {
                continue;
            }
            if let Some(prev) = self.last_rects.get(&(win.hwnd.0 as isize)) {
                let moved_x = (win.rect.left - prev.left).abs();
                let moved_y = (win.rect.top - prev.top).abs();
                if moved_x > 2 || moved_y > 2 {
                    self.last_chosen.insert(pid, win.hwnd.0 as isize);
                    return Some(win);
                }
            }
        }

        // Criterion 2: Media title hint
        // Check if one window clearly contains a media keyword and others don't
        let has_media_hint = |win: &WindowInfo| -> bool {
            let title = win.title.to_lowercase();
            const KEYWORDS: &[&str] = &[
                "youtube",
                "twitch",
                "spotify",
                "soundcloud",
                "netflix",
                "prime video",
                "vimeo",
                "bandcamp",
                "deezer",
                "tidal",
                "music",
                "video",
                "hulu",
                "disney+",
                "plex",
                "podcasts",
                "audible",
                "▶",
                "🔊",
                "🎵",
            ];
            KEYWORDS.iter().any(|&kw| title.contains(kw))
        };

        let media_candidates: Vec<&'a WindowInfo> = candidates
            .iter()
            .filter(|w| !w.is_minimized && has_media_hint(w))
            .collect();

        if media_candidates.len() == 1 {
            let chosen = media_candidates[0];
            self.last_chosen.insert(pid, chosen.hwnd.0 as isize);
            return Some(chosen);
        }

        // Criterion 3: Sticky window (the window previously chosen for this
        // PID, if still alive and visible)
        if let Some(&last_hwnd) = self.last_chosen.get(&pid) {
            if let Some(win) = candidates
                .iter()
                .find(|w| (w.hwnd.0 as isize) == last_hwnd && !w.is_minimized)
            {
                return Some(win);
            }
        }

        // Criterion 4: Foreground window (if the user is actively focused on
        // one of this process's windows)
        if !foreground_hwnd.is_invalid() {
            if let Some(win) = candidates
                .iter()
                .find(|w| w.hwnd == foreground_hwnd && !w.is_minimized)
            {
                self.last_chosen.insert(pid, win.hwnd.0 as isize);
                return Some(win);
            }
        }

        // Criterion 5: Topmost non-minimized window in Z-order
        let chosen = candidates
            .iter()
            .find(|w| !w.is_minimized)
            .unwrap_or(&candidates[0]);
        self.last_chosen.insert(pid, chosen.hwnd.0 as isize);
        Some(chosen)
    }

    /// Updates previous rect cache for all observed windows.
    fn update_rects(&mut self, windows: &[WindowInfo]) {
        self.last_rects.clear();
        for win in windows {
            if !win.is_minimized {
                self.last_rects.insert(win.hwnd.0 as isize, win.rect);
            }
        }
    }
}

/// Queries the bounding rectangle of the monitor nearest to the given window.
fn get_window_monitor_bounds(hwnd: HWND, fallback_left: i32, fallback_width: i32) -> (i32, i32) {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: Querying monitor handle nearest to window.
    let hmonitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    // SAFETY: Reading monitor geometry into the initialized MONITORINFO
    // structure.
    if unsafe { GetMonitorInfoW(hmonitor, &raw mut info) }.as_bool() {
        let left = info.rcMonitor.left;
        let width = (info.rcMonitor.right - info.rcMonitor.left).max(1);
        (left, width)
    } else {
        (fallback_left, fallback_width)
    }
}

/// Computes stereo balance volumes `[left, right]` given window bounds and
/// screen bounds using an edge-aware panning formula scaled by available travel
/// range.
fn compute_pan_volumes(
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
        // Minimized windows or audio processes without a window default to centered (pan = 0.0)
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

/// Resets stereo balance for all active audio sessions back to centered `[1.0,
/// 1.0]`.
fn reset_all_sessions(session_manager: &IAudioSessionManager2) {
    // SAFETY: Enumerating active sessions to restore unity balance.
    let enumerator = match unsafe { session_manager.GetSessionEnumerator() } {
        Ok(e) => e,
        Err(_) => return,
    };

    // SAFETY: Querying session count.
    let count = match unsafe { enumerator.GetCount() } {
        Ok(c) => c,
        Err(_) => return,
    };

    let default_volumes = [1.0f32, 1.0f32];

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
            // SAFETY: Restoring balanced stereo levels.
            unsafe {
                let _ = channel_vol.SetAllVolumes(&default_volumes, std::ptr::null());
            }
        }
    }
}

/// Main audio tracking loop: monitors desktop windows and applies WASAPI
/// stereo panning to active audio sessions.
fn run_audio_loop(
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

        let (windows_by_pid, all_windows) = get_windows_by_pid();
        let process_tree = get_process_tree();

        // SAFETY: Querying session enumerator for currently active audio
        // sessions.
        if let Ok(enumerator) = unsafe { session_manager.GetSessionEnumerator() } {
            // SAFETY: Reading number of sessions.
            if let Ok(count) = unsafe { enumerator.GetCount() } {
                for i in 0..count {
                    // SAFETY: Getting session control interface.
                    let Ok(session_control) = (unsafe { enumerator.GetSession(i) }) else {
                        continue;
                    };
                    let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>()
                    else {
                        continue;
                    };

                    // SAFETY: Checking session playback state.
                    let Ok(state) = (unsafe { session_control2.GetState() }) else {
                        continue;
                    };
                    if state != AudioSessionStateActive {
                        continue;
                    }

                    // SAFETY: Fetching process ID responsible for this audio
                    // session.
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

                    let exe_name = process_tree
                        .get(&pid)
                        .map(|n| n.exe_name.as_str())
                        .unwrap_or("unknown");

                    // Check if this application is currently claimed by an
                    // in-tab userscript
                    if claim_tracker.is_claimed(exe_name) {
                        let mut current_volumes = [0.0f32; 2];
                        // SAFETY: Reading channel volumes to ensure balanced
                        // master volume.
                        if unsafe { channel_volume.GetAllVolumes(&mut current_volumes) }.is_ok() {
                            let diff_l = (current_volumes[0] - 1.0).abs();
                            let diff_r = (current_volumes[1] - 1.0).abs();
                            if diff_l > VOLUME_EPSILON || diff_r > VOLUME_EPSILON {
                                let default_volumes = [1.0f32, 1.0f32];
                                // SAFETY: Restoring 1.0 balance while
                                // userscript has control.
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
                        continue;
                    }

                    let candidate_windows =
                        resolve_windows_for_pid(pid, &windows_by_pid, &process_tree);
                    let window = candidate_windows.and_then(|candidates| {
                        tracker.choose_window(pid, candidates, foreground_hwnd)
                    });

                    let (target_l, target_r) =
                        compute_pan_volumes(window, screen_left, screen_width);

                    let mut current_volumes = [0.0f32; 2];
                    // SAFETY: Reading current channel volume levels.
                    if unsafe { channel_volume.GetAllVolumes(&mut current_volumes) }.is_ok() {
                        let diff_l = (current_volumes[0] - target_l).abs();
                        let diff_r = (current_volumes[1] - target_r).abs();

                        if diff_l > VOLUME_EPSILON || diff_r > VOLUME_EPSILON {
                            let new_volumes = [target_l, target_r];
                            // SAFETY: Setting left and right channel audio
                            // levels.
                            if unsafe {
                                channel_volume.SetAllVolumes(&new_volumes, std::ptr::null())
                            }
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
                }
            }
        }

        tracker.update_rects(&all_windows);
        thread::sleep(POLL_INTERVAL);
    }

    log::info!("Exiting: Resetting all audio session channels to default balance...");
    reset_all_sessions(&session_manager);
    log::info!("Done. Goodbye!");

    Ok(())
}

fn print_help() {
    println!(
        "WinAudioPan - Dynamic window-based audio panning for Windows

USAGE:
    winaudiopan [OPTIONS]

OPTIONS:
    --console       Force allocate and display a console log window
    --no-tray       Run in the foreground without a system tray icon
    -h, --help      Print help information
    -V, --version   Print version information
"
    );
}

fn print_version() {
    println!("winaudiopan {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut force_console = false;
    let mut no_tray = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                let _ = ConsoleManager::init(false);
                print_help();
                return;
            }
            "-V" | "--version" => {
                let _ = ConsoleManager::init(false);
                print_version();
                return;
            }
            "--console" => {
                force_console = true;
            }
            "--no-tray" | "--cli" => {
                no_tray = true;
            }
            other => {
                let _ = ConsoleManager::init(false);
                eprintln!("Unknown option: '{other}'. Use --help for available options.");
                std::process::exit(1);
            }
        }
    }

    // Attach to parent terminal if launched from CLI, or allocate console if
    // requested. If launched from Explorer / Start menu without args, no
    // console window is created.
    let _has_console = ConsoleManager::init(force_console);

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

    if no_tray {
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
