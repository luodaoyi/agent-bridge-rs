#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::{
        StationsAndDesktops::{
            CloseDesktop, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
            OpenDesktopW, UOI_NAME,
        },
        Threading::GetCurrentThreadId,
    },
};

#[cfg(windows)]
const DESKTOP_READ_WRITE_OBJECTS: u32 = 0x0001 | 0x0040;
#[cfg(windows)]
const DEFAULT_INTERACTIVE_DESKTOP: &str = "WinSta0\\Default";
#[cfg(windows)]
const DEFAULT_DESKTOP_NAME: &str = "Default";
#[cfg(windows)]
const INTERACTIVE_WINDOW_STATION: &str = "WinSta0";

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn user_object_name(handle: HANDLE) -> Option<String> {
    if handle.is_null() {
        return None;
    }
    let mut buffer = [0_u16; 256];
    let mut needed = 0_u32;
    let byte_len = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: `buffer` is a valid writable UTF-16 buffer of `byte_len` bytes.
    let ok = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buffer.as_mut_ptr().cast::<c_void>(),
            byte_len,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(windows)]
fn current_window_station_and_desktop() -> (Option<String>, Option<String>) {
    // SAFETY: Process window station and thread desktop queries do not transfer handle ownership.
    let winsta = unsafe { GetProcessWindowStation() };
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    (user_object_name(winsta), user_object_name(desktop))
}

#[cfg(windows)]
fn can_open_default_desktop() -> bool {
    let default_name = wide_null(DEFAULT_DESKTOP_NAME);
    // SAFETY: `default_name` is NUL-terminated; any opened desktop handle is closed immediately.
    let handle = unsafe { OpenDesktopW(default_name.as_ptr(), 0, 0, DESKTOP_READ_WRITE_OBJECTS) };
    if handle.is_null() {
        return false;
    }
    unsafe {
        CloseDesktop(handle);
    }
    true
}

#[cfg(windows)]
pub(crate) fn should_reset_to_default_desktop(
    winsta: Option<&str>,
    desktop: Option<&str>,
    can_open_default: bool,
) -> bool {
    if !can_open_default {
        return false;
    }
    let Some(winsta_name) = winsta else {
        return false;
    };
    let Some(desktop_name) = desktop else {
        return false;
    };
    winsta_name.eq_ignore_ascii_case(INTERACTIVE_WINDOW_STATION)
        && !desktop_name.eq_ignore_ascii_case(DEFAULT_DESKTOP_NAME)
}

/// When a Windows host (such as Antigravity's `WinSta0\exebox-*` shell) runs
/// `agent-bridge` on an ephemeral desktop whose DACL strips AppContainer access,
/// reset child launches back to `WinSta0\Default` if reachable so restricted-token
/// sandboxes (Codex `windows.sandbox = "mxc"`) initialize `user32.dll` cleanly.
#[cfg(windows)]
pub(crate) fn preferred_default_desktop_wide() -> Option<Vec<u16>> {
    let (winsta, desktop) = current_window_station_and_desktop();
    if should_reset_to_default_desktop(
        winsta.as_deref(),
        desktop.as_deref(),
        can_open_default_desktop(),
    ) {
        Some(wide_null(DEFAULT_INTERACTIVE_DESKTOP))
    } else {
        None
    }
}

#[cfg(all(windows, test))]
mod tests {
    use super::should_reset_to_default_desktop;

    #[test]
    fn resets_only_when_interactive_non_default_desktop_and_default_is_accessible() {
        assert!(should_reset_to_default_desktop(
            Some("WinSta0"),
            Some("exebox-D7YNX563EFGI2D6IINTZK6B3T3"),
            true
        ));
        assert!(should_reset_to_default_desktop(
            Some("winsta0"),
            Some("custom-desktop"),
            true
        ));
        assert!(!should_reset_to_default_desktop(
            Some("WinSta0"),
            Some("Default"),
            true
        ));
        assert!(!should_reset_to_default_desktop(
            Some("WinSta0"),
            Some("default"),
            true
        ));
        assert!(!should_reset_to_default_desktop(
            Some("WinSta0"),
            Some("exebox-123"),
            false
        ));
        assert!(!should_reset_to_default_desktop(
            Some("Service-0x0-3e7$"),
            Some("Default"),
            true
        ));
        assert!(!should_reset_to_default_desktop(
            None,
            Some("exebox-1"),
            true
        ));
        assert!(!should_reset_to_default_desktop(
            Some("WinSta0"),
            None,
            true
        ));
    }
}
