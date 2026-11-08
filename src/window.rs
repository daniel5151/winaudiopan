//! Desktop window tracking, monitor geometry detection, and process ancestry
//! resolution.

use std::collections::HashMap;
use std::mem::size_of;
use std::time::Duration;
use std::time::Instant;

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
use windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot;
use windows::Win32::System::Diagnostics::ToolHelp::PROCESSENTRY32W;
use windows::Win32::System::Diagnostics::ToolHelp::Process32FirstW;
use windows::Win32::System::Diagnostics::ToolHelp::Process32NextW;
use windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPPROCESS;
use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
use windows::Win32::UI::WindowsAndMessaging::GW_OWNER;
use windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE;
use windows::Win32::UI::WindowsAndMessaging::GetWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextLengthW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::Win32::UI::WindowsAndMessaging::IsIconic;
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use windows::core::BOOL;

/// Minimum time between full process snapshots to reduce CPU and heap churn.
const PROCESS_CACHE_TTL: Duration = Duration::from_millis(1500);

/// Metadata about a running process.
#[derive(Debug, Clone)]
pub struct ProcessNode {
    /// Parent process ID.
    pub parent_pid: u32,
    /// Executable name.
    pub exe_name: String,
}

/// Cached process tree with TTL-based refresh.
pub struct ProcessTreeCache {
    tree: HashMap<u32, ProcessNode>,
    last_refresh: Instant,
}

impl ProcessTreeCache {
    /// Creates a new cache and populates the initial snapshot.
    pub fn new() -> Self {
        let mut cache = Self {
            tree: HashMap::new(),
            last_refresh: Instant::now() - PROCESS_CACHE_TTL,
        };
        cache.refresh_if_stale();
        cache
    }

    /// Refreshes the snapshot if a PID is not found in the cache.
    pub fn refresh_if_missing(&mut self, pid: u32) {
        if !self.tree.contains_key(&pid) {
            self.refresh();
        }
    }

    /// Queries metadata for a process from current snapshot without forcing a
    /// refresh.
    pub fn get(&self, pid: u32) -> Option<&ProcessNode> {
        self.tree.get(&pid)
    }

    /// Refreshes the snapshot if cache TTL has elapsed.
    pub fn refresh_if_stale(&mut self) {
        if self.last_refresh.elapsed() >= PROCESS_CACHE_TTL {
            self.refresh();
        }
    }

    /// Unconditionally snapshots the process hierarchy via ToolHelp32.
    pub fn refresh(&mut self) {
        self.tree.clear();
        self.last_refresh = Instant::now();

        // SAFETY: Captures snapshot of all processes running on system.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        let Ok(handle) = snapshot else {
            return;
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

                self.tree.insert(
                    entry.th32ProcessID,
                    ProcessNode {
                        parent_pid: entry.th32ParentProcessID,
                        exe_name,
                    },
                );

                // SAFETY: Reading subsequent process entry from snapshot
                // handle.
                if unsafe { Process32NextW(handle, &raw mut entry) }.is_err() {
                    break;
                }
            }
        }

        // SAFETY: Closing snapshot handle.
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}

/// Metadata about a top-level application window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// Window handle.
    pub hwnd: HWND,
    /// Window title.
    pub title: String,
    /// Whether the window is minimized.
    pub is_minimized: bool,
    /// Window rectangular bounding box.
    pub rect: RECT,
}

/// Callback state passed to `EnumWindows`.
#[derive(Default)]
struct EnumWindowsState {
    /// Discovered candidate top-level windows grouped by PID in Z-order.
    windows_by_pid: HashMap<u32, Vec<WindowInfo>>,
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
        let needed = (title_len + 1) as usize;
        // Use stack buffer for typical window titles to avoid heap allocations
        if needed <= 256 {
            let mut buffer = [0u16; 256];
            // SAFETY: Copying window title into stack buffer.
            let copied = unsafe { GetWindowTextW(hwnd, &mut buffer[..needed]) };
            if copied > 0 {
                title = String::from_utf16_lossy(&buffer[..copied as usize]);
            }
        } else {
            let mut buffer = vec![0u16; needed];
            // SAFETY: Copying window title into heap buffer for unusually long
            // titles.
            let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
            if copied > 0 {
                title = String::from_utf16_lossy(&buffer[..copied as usize]);
            }
        }
    }

    state
        .windows_by_pid
        .entry(pid)
        .or_default()
        .push(WindowInfo {
            hwnd,
            title,
            is_minimized,
            rect,
        });

    BOOL(1)
}

/// Enumerate visible top-level windows and group them by PID in Z-order.
pub fn get_windows_by_pid() -> HashMap<u32, Vec<WindowInfo>> {
    let mut state = EnumWindowsState::default();
    let lparam = LPARAM((&raw mut state) as isize);

    // SAFETY: Invoking Win32 EnumWindows with safe callback and state pointer.
    let _ = unsafe { EnumWindows(Some(enum_windows_callback), lparam) };

    state.windows_by_pid
}

/// Resolves candidate windows for an audio-producing PID by ascending the
/// process tree.
pub fn resolve_windows_for_pid<'a>(
    mut pid: u32,
    windows: &'a HashMap<u32, Vec<WindowInfo>>,
    process_tree: &ProcessTreeCache,
) -> Option<&'a [WindowInfo]> {
    // Check up to 6 levels of process ancestry (e.g. Chrome utility / audio
    // helper -> browser process)
    for _ in 0..6 {
        if let Some(wins) = windows.get(&pid) {
            if !wins.is_empty() {
                return Some(wins.as_slice());
            }
        }
        if let Some(node) = process_tree.get(pid) {
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
pub struct WindowTracker {
    /// Last chosen HWND for each audio process ID.
    last_chosen: HashMap<u32, HWND>,
    /// Previous bounding box for each HWND (`hwnd.0 as isize`) to detect
    /// motion.
    last_rects: HashMap<isize, RECT>,
}

impl WindowTracker {
    /// Selects the best window for an audio process from candidate windows.
    pub fn choose_window<'a>(
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
            self.last_chosen.insert(pid, chosen.hwnd);
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
                    self.last_chosen.insert(pid, win.hwnd);
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
            self.last_chosen.insert(pid, chosen.hwnd);
            return Some(chosen);
        }

        // Criterion 3: Sticky window (the window previously chosen for this
        // PID, if still alive and visible)
        if let Some(&last_hwnd) = self.last_chosen.get(&pid) {
            if let Some(win) = candidates
                .iter()
                .find(|w| w.hwnd == last_hwnd && !w.is_minimized)
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
                self.last_chosen.insert(pid, win.hwnd);
                return Some(win);
            }
        }

        // Criterion 5: Topmost non-minimized window in Z-order
        let chosen = candidates
            .iter()
            .find(|w| !w.is_minimized)
            .unwrap_or(&candidates[0]);
        self.last_chosen.insert(pid, chosen.hwnd);
        Some(chosen)
    }

    /// Updates previous rect cache for all observed windows without cloning.
    pub fn update_rects<'a>(&mut self, windows: impl IntoIterator<Item = &'a WindowInfo>) {
        self.last_rects.clear();
        for win in windows {
            if !win.is_minimized {
                self.last_rects.insert(win.hwnd.0 as isize, win.rect);
            }
        }
    }
}

/// Queries the bounding rectangle of the monitor nearest to the given window.
pub fn get_window_monitor_bounds(
    hwnd: HWND,
    fallback_left: i32,
    fallback_width: i32,
) -> (i32, i32) {
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
