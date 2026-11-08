//! System tray notification area utility for WinAudioPan.

use std::mem::size_of;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::LRESULT;
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::WPARAM;
use windows::Win32::Graphics::Gdi::CreateBitmap;
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::NIF_ICON;
use windows::Win32::UI::Shell::NIF_MESSAGE;
use windows::Win32::UI::Shell::NIF_TIP;
use windows::Win32::UI::Shell::NIM_ADD;
use windows::Win32::UI::Shell::NIM_DELETE;
use windows::Win32::UI::Shell::NIM_MODIFY;
use windows::Win32::UI::Shell::NOTIFYICONDATAW;
use windows::Win32::UI::Shell::Shell_NotifyIconW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::AppendMenuW;
use windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT;
use windows::Win32::UI::WindowsAndMessaging::CreateIconIndirect;
use windows::Win32::UI::WindowsAndMessaging::CreatePopupMenu;
use windows::Win32::UI::WindowsAndMessaging::CreateWindowExW;
use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;
use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;
use windows::Win32::UI::WindowsAndMessaging::DestroyMenu;
use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
use windows::Win32::UI::WindowsAndMessaging::DispatchMessageW;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::WindowsAndMessaging::GetMessageW;
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::Win32::UI::WindowsAndMessaging::ICONINFO;
use windows::Win32::UI::WindowsAndMessaging::IDI_APPLICATION;
use windows::Win32::UI::WindowsAndMessaging::IDYES;
use windows::Win32::UI::WindowsAndMessaging::LoadIconW;
use windows::Win32::UI::WindowsAndMessaging::MB_ICONINFORMATION;
use windows::Win32::UI::WindowsAndMessaging::MB_YESNO;
use windows::Win32::UI::WindowsAndMessaging::MF_CHECKED;
use windows::Win32::UI::WindowsAndMessaging::MF_DISABLED;
use windows::Win32::UI::WindowsAndMessaging::MF_GRAYED;
use windows::Win32::UI::WindowsAndMessaging::MF_SEPARATOR;
use windows::Win32::UI::WindowsAndMessaging::MF_STRING;
use windows::Win32::UI::WindowsAndMessaging::MF_UNCHECKED;
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::Win32::UI::WindowsAndMessaging::MessageBoxW;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::Win32::UI::WindowsAndMessaging::PostQuitMessage;
use windows::Win32::UI::WindowsAndMessaging::RegisterClassExW;
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::TPM_BOTTOMALIGN;
use windows::Win32::UI::WindowsAndMessaging::TPM_RIGHTBUTTON;
use windows::Win32::UI::WindowsAndMessaging::TrackPopupMenuEx;
use windows::Win32::UI::WindowsAndMessaging::TranslateMessage;
use windows::Win32::UI::WindowsAndMessaging::WM_CLOSE;
use windows::Win32::UI::WindowsAndMessaging::WM_COMMAND;
use windows::Win32::UI::WindowsAndMessaging::WM_DESTROY;
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK;
use windows::Win32::UI::WindowsAndMessaging::WM_NULL;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_USER;
use windows::Win32::UI::WindowsAndMessaging::WNDCLASSEXW;
use windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW;
use windows::Win32::UI::WindowsAndMessaging::WS_POPUP;
use windows::core::BOOL;
use windows::core::PCWSTR;
use windows::core::w;

use crate::ClaimTracker;
use crate::console::ConsoleManager;

/// Custom window message sent by the Windows shell when tray icon events occur.
const WM_TRAY_CALLBACK: u32 = WM_USER + 101;

/// Menu command ID for toggling audio panning.
const CMD_TOGGLE_ENABLED: usize = 1001;
/// Menu command ID for resetting all audio session balances.
const CMD_RESET_BALANCES: usize = 1002;
/// Menu command ID for showing or hiding the console log window.
const CMD_TOGGLE_CONSOLE: usize = 1003;
/// Menu command ID for displaying the about dialog box.
const CMD_ABOUT: usize = 1004;
/// Menu command ID for exiting the utility.
const CMD_EXIT: usize = 1005;

/// Shared application state accessible to the tray icon menu.
#[derive(Clone)]
pub struct TrayContext {
    /// Atomic flag indicating whether the entire service is running.
    pub running: Arc<AtomicBool>,
    /// Atomic flag indicating whether audio panning calculations are enabled.
    pub enabled: Arc<AtomicBool>,
    /// Atomic flag requesting an immediate reset of all session balances.
    pub reset_requested: Arc<AtomicBool>,
    /// Userscript claim tracker.
    pub claim_tracker: Arc<ClaimTracker>,
}

/// Global singleton state for the active tray window.
struct TrayState {
    context: TrayContext,
    hwnd: HWND,
    active_icon: HICON,
    disabled_icon: HICON,
    taskbar_created_msg: u32,
}

// SAFETY: HWND and HICON are process-level Win32 resource handles that are safe
// to share across thread boundaries for read-only access.
unsafe impl Send for TrayState {}
// SAFETY: HWND and HICON are process-level Win32 resource handles that are safe
// to share across thread boundaries for read-only access.
unsafe impl Sync for TrayState {}

static TRAY_STATE: OnceLock<TrayState> = OnceLock::new();

/// Generates a 16x16 32-bit ARGB tray icon programmatically.
fn create_custom_icon(enabled: bool) -> HICON {
    const W: i32 = 16;
    const H: i32 = 16;
    let mut pixels = [0u32; (W * H) as usize];

    let white = 0xFFFFFFFF;
    let cyan = 0xFF00D2FF;
    let gray = 0xFF9E9E9E;
    let dark_gray = 0xFF616161;
    let red = 0xFFE53935;
    let dark_red = 0xFFC62828;

    let body_color = if enabled { white } else { gray };
    let wave_color = if enabled { cyan } else { dark_gray };

    // Speaker rectangular base (cols 3..=5, rows 5..=10)
    for y in 5..=10 {
        for x in 3..=5 {
            pixels[y * 16 + x] = body_color;
        }
    }

    // Speaker cone expanding outwards (cols 6..=8)
    for x in 6..=8 {
        let spread = x - 5;
        let y_start = 5 - spread;
        let y_end = 10 + spread;
        for y in y_start..=y_end {
            pixels[y * 16 + x] = body_color;
        }
    }

    // Inner sound wave arc (col 10)
    for y in 4..=11 {
        pixels[y * 16 + 10] = wave_color;
    }
    // Inner sound wave center (col 11)
    for y in 5..=10 {
        pixels[y * 16 + 11] = wave_color;
    }

    // Outer sound wave arc (col 13)
    for y in 2..=13 {
        pixels[y * 16 + 13] = wave_color;
    }
    // Outer sound wave center (col 14)
    for y in 3..=12 {
        pixels[y * 16 + 14] = wave_color;
    }

    // Left stereo panning indicator wave (cols 0 and 1)
    for y in 5..=10 {
        pixels[y * 16 + 1] = wave_color;
    }
    for y in 6..=9 {
        pixels[y * 16] = wave_color;
    }

    // If disabled, draw a prominent diagonal slash
    if !enabled {
        for i in 2..=13 {
            pixels[i * 16 + i] = red;
            if i < 15 {
                pixels[i * 16 + i + 1] = dark_red;
            }
        }
    }

    // 1bpp mask (16 rows * 2 bytes = 32 bytes)
    let mask_bits = [0u8; 32];
    // SAFETY: Creating 1bpp mask and 32bpp color bitmaps for HICON creation.
    let hbm_mask = unsafe { CreateBitmap(W, H, 1, 1, Some(mask_bits.as_ptr().cast())) };
    // SAFETY: Creating 32bpp color bitmap with ARGB pixels for HICON creation.
    let hbm_color = unsafe { CreateBitmap(W, H, 1, 32, Some(pixels.as_ptr().cast())) };

    if !hbm_mask.is_invalid() && !hbm_color.is_invalid() {
        let icon_info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };

        // SAFETY: Assembling icon from mask and color bitmaps.
        let hicon = unsafe { CreateIconIndirect(&icon_info) };

        // SAFETY: Deleting temporary GDI bitmaps once icon has been created.
        unsafe {
            let _ = DeleteObject(hbm_mask.into());
            let _ = DeleteObject(hbm_color.into());
        }

        if let Ok(icon) = hicon {
            return icon;
        }
    } else {
        // SAFETY: Cleaning up whichever bitmap succeeded if one failed.
        unsafe {
            if !hbm_mask.is_invalid() {
                let _ = DeleteObject(hbm_mask.into());
            }
            if !hbm_color.is_invalid() {
                let _ = DeleteObject(hbm_color.into());
            }
        }
    }

    // SAFETY: Fallback to standard application icon if custom icon creation
    // fails.
    unsafe { LoadIconW(None, IDI_APPLICATION).unwrap_or_default() }
}

/// Populates a `NOTIFYICONDATAW` struct with tooltip text and icon.
fn create_notify_icon_data(hwnd: HWND, icon: HICON, enabled: bool) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };

    let tooltip = if enabled {
        "WinAudioPan - Active (Panning Audio)"
    } else {
        "WinAudioPan - Paused (Audio Centered)"
    };

    let mut tip = [0u16; 128];
    for (i, c) in tooltip.encode_utf16().take(127).enumerate() {
        tip[i] = c;
    }
    nid.szTip = tip;
    nid
}

/// Toggles panning state and refreshes the tray icon and tooltip.
fn toggle_panning(hwnd: HWND) {
    if let Some(state) = TRAY_STATE.get() {
        let previously_enabled = state.context.enabled.fetch_xor(true, Ordering::SeqCst);
        let now_enabled = !previously_enabled;

        if now_enabled {
            log::info!("Audio panning ENABLED via tray icon");
        } else {
            log::info!("Audio panning PAUSED via tray icon -> Resetting balances to center");
            state.context.reset_requested.store(true, Ordering::SeqCst);
        }

        let icon = if now_enabled {
            state.active_icon
        } else {
            state.disabled_icon
        };

        let nid = create_notify_icon_data(hwnd, icon, now_enabled);
        // SAFETY: Updating tray icon image and tooltip string.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

/// Displays the tray context popup menu at the current cursor coordinates.
fn show_context_menu(hwnd: HWND) {
    let Some(state) = TRAY_STATE.get() else {
        return;
    };

    let mut pt = POINT::default();
    // SAFETY: Querying current mouse cursor coordinates.
    if unsafe { GetCursorPos(&raw mut pt) }.is_err() {
        return;
    }

    // SAFETY: Bringing window to foreground so the popup menu closes properly
    // if the user clicks away.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }

    // SAFETY: Creating a floating popup context menu.
    let Ok(hmenu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };

    let is_enabled = state.context.enabled.load(Ordering::SeqCst);
    let enabled_flags = if is_enabled {
        MF_CHECKED | MF_STRING
    } else {
        MF_UNCHECKED | MF_STRING
    };

    let console_visible = ConsoleManager::is_console_visible();
    let console_text = if console_visible {
        w!("Hide Console Log Window")
    } else {
        w!("Show Console Log Window")
    };

    // Determine userscript status string
    let userscript_claimed = state.context.claim_tracker.has_any_claims();
    let userscript_text_str = if userscript_claimed {
        "Browser Userscript: Active\0"
    } else {
        "Browser Userscript: Idle (Port 38400)\0"
    };
    let userscript_utf16: Vec<u16> = userscript_text_str.encode_utf16().collect();

    // SAFETY: Appending menu items to the popup menu.
    unsafe {
        // App title header (read-only)
        let _ = AppendMenuW(
            hmenu,
            MF_DISABLED | MF_GRAYED | MF_STRING,
            0,
            w!("WinAudioPan v0.1.0"),
        );
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

        // Toggle panning
        let _ = AppendMenuW(
            hmenu,
            enabled_flags,
            CMD_TOGGLE_ENABLED,
            w!("Audio Panning Active"),
        );

        // Immediate reset
        let _ = AppendMenuW(
            hmenu,
            MF_STRING,
            CMD_RESET_BALANCES,
            w!("Reset All Audio Balances"),
        );

        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

        // Show/hide console window
        let _ = AppendMenuW(hmenu, MF_STRING, CMD_TOGGLE_CONSOLE, console_text);

        // Userscript coordination status
        let _ = AppendMenuW(
            hmenu,
            MF_DISABLED | MF_GRAYED | MF_STRING,
            0,
            PCWSTR::from_raw(userscript_utf16.as_ptr()),
        );

        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

        // About dialog
        let _ = AppendMenuW(hmenu, MF_STRING, CMD_ABOUT, w!("About WinAudioPan..."));

        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

        // Exit
        let _ = AppendMenuW(hmenu, MF_STRING, CMD_EXIT, w!("Exit"));

        // Display menu and handle selected command
        let _ = TrackPopupMenuEx(
            hmenu,
            (TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );

        let _ = DestroyMenu(hmenu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    }
}

/// Handles menu command dispatches.
fn handle_command(hwnd: HWND, cmd_id: usize) {
    let Some(state) = TRAY_STATE.get() else {
        return;
    };

    match cmd_id {
        CMD_TOGGLE_ENABLED => {
            toggle_panning(hwnd);
        }
        CMD_RESET_BALANCES => {
            log::info!("Immediate balance reset requested via tray menu");
            state.context.reset_requested.store(true, Ordering::SeqCst);
        }
        CMD_TOGGLE_CONSOLE => {
            ConsoleManager::toggle_console();
        }
        CMD_ABOUT => {
            show_about_dialog(hwnd);
        }
        CMD_EXIT => {
            log::info!("Exit requested from tray menu. Shutting down...");
            state.context.running.store(false, Ordering::SeqCst);
            // SAFETY: Destroying tray window to exit message loop.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

/// Displays the "About WinAudioPan" dialog box and offers to open the
/// repository.
fn show_about_dialog(hwnd: HWND) {
    let message = format!(
        "WinAudioPan v{}\n\n{}\n\nAuthor: {}\nRepository: {}\nUserscript Server: http://127.0.0.1:38400\n\nWould you like to open the GitHub repository in your browser?\0",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_DESCRIPTION"),
        env!("CARGO_PKG_AUTHORS"),
        env!("CARGO_PKG_REPOSITORY")
    );
    let message_wide: Vec<u16> = message.encode_utf16().collect();

    // SAFETY: Ensuring window is focused before displaying modal message box.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }

    // SAFETY: Displaying standard clean Windows message box with Yes/No
    // actions.
    let response = unsafe {
        MessageBoxW(
            Some(hwnd),
            PCWSTR::from_raw(message_wide.as_ptr()),
            w!("About WinAudioPan"),
            MB_YESNO | MB_ICONINFORMATION,
        )
    };

    if response == IDYES {
        open_repository();
    }
}

/// Opens the project GitHub repository in the user's default browser.
fn open_repository() {
    let url = format!("{}\0", env!("CARGO_PKG_REPOSITORY"));
    let url_wide: Vec<u16> = url.encode_utf16().collect();

    // SAFETY: Launching default web browser to view GitHub repository.
    unsafe {
        let _ = ShellExecuteW(
            None,
            w!("open"),
            PCWSTR::from_raw(url_wide.as_ptr()),
            None,
            None,
            SW_SHOW,
        );
    }
}

/// Win32 window procedure for the hidden tray message window.
unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let taskbar_msg = TRAY_STATE.get().map(|s| s.taskbar_created_msg).unwrap_or(0);

    match msg {
        WM_TRAY_CALLBACK => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if event == WM_RBUTTONUP {
                show_context_menu(hwnd);
            } else if event == WM_LBUTTONDBLCLK {
                toggle_panning(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd_id = wparam.0 & 0xFFFF;
            handle_command(hwnd, cmd_id);
            LRESULT(0)
        }
        m if taskbar_msg != 0 && m == taskbar_msg => {
            // Taskbar restarted -> Re-register tray icon
            if let Some(state) = TRAY_STATE.get() {
                let is_enabled = state.context.enabled.load(Ordering::SeqCst);
                let icon = if is_enabled {
                    state.active_icon
                } else {
                    state.disabled_icon
                };
                let nid = create_notify_icon_data(hwnd, icon, is_enabled);
                // SAFETY: Re-adding icon after explorer restart.
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: Destroying tray window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: Posting quit message to break out of message loop.
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => {
            // SAFETY: Forwarding unhandled messages to default window proc.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Runs the system tray notification icon and Win32 message loop on the main
/// thread.
pub fn run_tray(context: TrayContext) -> anyhow::Result<()> {
    // SAFETY: Querying module instance handle.
    let hinstance = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let class_name = w!("WinAudioPanTrayWindowClass");

    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(tray_window_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };

    // SAFETY: Registering hidden window class for tray icon event routing.
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        anyhow::bail!("Failed to register tray window class");
    }

    // SAFETY: Creating hidden message-only window.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("WinAudioPanTray"),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(hinstance.0)),
            None,
        )
    }?;

    let active_icon = create_custom_icon(true);
    let disabled_icon = create_custom_icon(false);
    // SAFETY: Registering TaskbarCreated message to handle explorer restarts.
    let taskbar_created_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };

    let is_enabled = context.enabled.load(Ordering::SeqCst);
    let initial_icon = if is_enabled {
        active_icon
    } else {
        disabled_icon
    };

    let nid = create_notify_icon_data(hwnd, initial_icon, is_enabled);
    // SAFETY: Adding tray icon to notification area.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }

    let _ = TRAY_STATE.set(TrayState {
        context: context.clone(),
        hwnd,
        active_icon,
        disabled_icon,
        taskbar_created_msg,
    });

    log::info!(
        "WinAudioPan taskbar tray icon created. Right-click for menu, double-click to toggle."
    );

    // Standard Win32 message pump
    let mut msg = MSG::default();
    // SAFETY: Retrieving and dispatching window messages until WM_QUIT.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        // SAFETY: Translating and dispatching window message.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup tray icon on exit
    // SAFETY: Removing tray icon from notification area and freeing icon
    // resources.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let _ = DestroyIcon(active_icon);
        let _ = DestroyIcon(disabled_icon);
    }

    Ok(())
}

/// Posts a close message to the tray window to cleanly break the message pump.
pub fn request_tray_exit() {
    if let Some(state) = TRAY_STATE.get() {
        // SAFETY: Posting WM_CLOSE to tray window to initiate orderly shutdown.
        unsafe {
            let _ = PostMessageW(Some(state.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}
