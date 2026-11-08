//! Console management for attaching to a parent CLI or allocating a log window
//! on demand.

use std::fs::OpenOptions;
use std::os::windows::io::IntoRawHandle;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::ATTACH_PARENT_PROCESS;
use windows::Win32::System::Console::AllocConsole;
use windows::Win32::System::Console::AttachConsole;
use windows::Win32::System::Console::GetConsoleWindow;
use windows::Win32::System::Console::GetStdHandle;
use windows::Win32::System::Console::STD_ERROR_HANDLE;
use windows::Win32::System::Console::STD_INPUT_HANDLE;
use windows::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows::Win32::System::Console::SetStdHandle;
use windows::Win32::UI::WindowsAndMessaging::DeleteMenu;
use windows::Win32::UI::WindowsAndMessaging::GetSystemMenu;
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
use windows::Win32::UI::WindowsAndMessaging::MF_BYCOMMAND;
use windows::Win32::UI::WindowsAndMessaging::SC_CLOSE;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
use windows::Win32::UI::WindowsAndMessaging::ShowWindow;

/// Utility for managing console attachment, allocation, and stdio redirection.
pub struct ConsoleManager;

impl ConsoleManager {
    /// Attempts to attach to an existing parent console (e.g. when run from a
    /// terminal / CLI), or optionally forces allocation of a new console
    /// window.
    pub fn init(force_alloc: bool) -> bool {
        // First check if stdout is already a valid redirected/piped handle.
        // SAFETY: Checking if process was spawned with an already-valid output
        // handle.
        let existing_stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let has_existing_stdout = match existing_stdout {
            Ok(h) => !h.is_invalid() && !h.0.is_null(),
            Err(_) => false,
        };

        if has_existing_stdout && !force_alloc {
            return true;
        }

        let attached = if force_alloc {
            // SAFETY: Allocating a new console window for this process.
            unsafe { AllocConsole() }.is_ok()
        } else {
            // SAFETY: Attaching to the console of the parent process if one
            // exists.
            unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_ok()
        };

        if attached {
            Self::redirect_stdio();
            if force_alloc {
                Self::disable_close_button();
            }
        }

        attached
    }

    /// Redirects standard I/O handles (`stdout`, `stderr`, `stdin`) to the
    /// active console device so Rust logging and print macros display
    /// correctly.
    pub fn redirect_stdio() {
        if let Ok(file) = OpenOptions::new().write(true).open("CONOUT$") {
            let raw = file.into_raw_handle();
            // SAFETY: Updating process stdio output and error handles to the
            // valid console device.
            unsafe {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, HANDLE(raw.cast()));
                let _ = SetStdHandle(STD_ERROR_HANDLE, HANDLE(raw.cast()));
            }
        }

        if let Ok(file) = OpenOptions::new().read(true).open("CONIN$") {
            let raw = file.into_raw_handle();
            // SAFETY: Updating process stdio input handle to the valid console
            // device.
            unsafe {
                let _ = SetStdHandle(STD_INPUT_HANDLE, HANDLE(raw.cast()));
            }
        }
    }

    /// Checks whether a console window currently exists and is visible.
    pub fn is_console_visible() -> bool {
        // SAFETY: Querying current console window handle.
        let console_hwnd = unsafe { GetConsoleWindow() };
        if console_hwnd.is_invalid() {
            false
        } else {
            // SAFETY: Querying visibility state of the console window.
            unsafe { IsWindowVisible(console_hwnd) }.as_bool()
        }
    }

    /// Toggles the console window between visible and hidden, allocating a
    /// new console window if one does not already exist.
    pub fn toggle_console() {
        // SAFETY: Querying current console window handle.
        let console_hwnd = unsafe { GetConsoleWindow() };
        if console_hwnd.is_invalid() {
            // SAFETY: Allocating a fresh console window on user demand.
            if unsafe { AllocConsole() }.is_ok() {
                Self::redirect_stdio();
                Self::disable_close_button();
            }
        } else {
            let is_visible = Self::is_console_visible();
            // SAFETY: Hiding or showing the existing console window.
            unsafe {
                let _ = ShowWindow(console_hwnd, if is_visible { SW_HIDE } else { SW_SHOW });
            }
        }
    }

    /// Disables the system close button ('X') on the console window to prevent
    /// accidental termination of the background service.
    fn disable_close_button() {
        // SAFETY: Querying console window handle and disabling the SC_CLOSE
        // menu command.
        unsafe {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_invalid() {
                let menu = GetSystemMenu(hwnd, false);
                if !menu.is_invalid() {
                    let _ = DeleteMenu(menu, SC_CLOSE, MF_BYCOMMAND);
                }
            }
        }
    }
}
