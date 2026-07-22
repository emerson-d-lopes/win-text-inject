//! Identify the injection target and decide, before injecting, whether injection can work at all.
//!
//! `SendInput` into a higher-integrity window fails silently. Per MSDN:
//!
//! > This function fails when it is blocked by UIPI. Note that neither `GetLastError` nor the
//! > return value will indicate the failure was caused by UIPI blocking.
//!
//! So the text simply vanishes. Checking the target's integrity level costs microseconds and turns
//! a silent loss into an honest "press Ctrl+V here" message. This is the root cause of the
//! elevated-window failures reported against every tool in this category.

use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{
    GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, RealGetWindowClassW,
};

use crate::Error;

/// Windows integrity levels, ordered. Comparison is what matters, not the raw RID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Integrity {
    Untrusted,
    Low,
    Medium,
    MediumPlus,
    High,
    System,
    Protected,
}

impl Integrity {
    /// Map the RID from a mandatory-label SID to a level.
    ///
    /// Ranges rather than equality: Windows defines the in-between values as belonging to the next
    /// lower named level.
    fn from_rid(rid: u32) -> Self {
        match rid {
            0..=0x0FFF => Integrity::Untrusted,
            0x1000..=0x1FFF => Integrity::Low,
            0x2000..=0x2FFF => Integrity::Medium,
            0x3000..=0x3FFF => Integrity::MediumPlus,
            0x4000..=0x4FFF => Integrity::High,
            0x5000..=0x5FFF => Integrity::System,
            _ => Integrity::Protected,
        }
    }
}

/// The window that will receive injected text, captured as a snapshot.
///
/// Capture this at hotkey **press**, not at injection time: between press and release the user may
/// have moved focus, and injecting into whatever happens to be foreground later is how text ends up
/// in the wrong application.
#[derive(Debug, Clone)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
    /// Lowercased executable file name, e.g. `code.exe`. Empty when it could not be read.
    pub exe: String,
    /// Real window class of the foreground window, e.g. `Chrome_RenderWidgetHostHWND`.
    pub class: String,
    pub integrity: Integrity,
}

impl Target {
    /// Snapshot the current foreground window.
    pub fn foreground() -> Result<Self, Error> {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.0.is_null() {
            return Err(Error::NoForegroundWindow);
        }

        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            return Err(Error::NoForegroundWindow);
        }

        Ok(Self {
            hwnd: hwnd.0 as isize,
            pid,
            exe: process_exe_name(pid).unwrap_or_default(),
            class: window_class(hwnd),
            integrity: process_integrity(pid).unwrap_or(Integrity::Medium),
        })
    }

    /// True when this target still holds the foreground. Injection must be aborted otherwise.
    pub fn still_foreground(&self) -> bool {
        let current = unsafe { GetForegroundWindow() };
        current.0 as isize == self.hwnd
    }

    /// Whether synthesized input can reach this target.
    ///
    /// UIPI permits injection only into processes at an equal or lower integrity level.
    pub fn accepts_injection(&self) -> bool {
        match our_integrity() {
            Some(ours) => self.integrity <= ours,
            // Unable to determine our own level: assume the optimistic case and let the caller's
            // verification step catch a failure, rather than refusing to work at all.
            None => true,
        }
    }
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { RealGetWindowClassW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..len as usize])
}

fn process_exe_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;

        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(
            Path::new(&full)
                .file_name()?
                .to_string_lossy()
                .to_lowercase(),
        )
    }
}

fn process_integrity(pid: u32) -> Option<Integrity> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let level = token_integrity(handle);
        let _ = CloseHandle(handle);
        level
    }
}

fn our_integrity() -> Option<Integrity> {
    unsafe { token_integrity(GetCurrentProcess()) }
}

unsafe fn token_integrity(process: HANDLE) -> Option<Integrity> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;

    let mut needed: u32 = 0;
    // First call always fails with ERROR_INSUFFICIENT_BUFFER; it exists to report the size.
    let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
    if needed == 0 {
        let _ = CloseHandle(token);
        return None;
    }

    let mut buf = vec![0u8; needed as usize];
    let result = GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut _),
        needed,
        &mut needed,
    );
    let _ = CloseHandle(token);
    result.ok()?;

    let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    if sid.is_invalid() {
        return None;
    }

    let count_ptr = windows::Win32::Security::GetSidSubAuthorityCount(sid);
    if count_ptr.is_null() {
        return None;
    }
    let last = (*count_ptr).saturating_sub(1) as u32;
    let rid_ptr = windows::Win32::Security::GetSidSubAuthority(sid, last);
    if rid_ptr.is_null() {
        return None;
    }
    Some(Integrity::from_rid(*rid_ptr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_is_ordered() {
        assert!(Integrity::Low < Integrity::Medium);
        assert!(Integrity::Medium < Integrity::High);
        assert!(Integrity::High < Integrity::System);
    }

    #[test]
    fn rid_ranges_map_to_named_levels() {
        assert_eq!(Integrity::from_rid(0x0000), Integrity::Untrusted);
        assert_eq!(Integrity::from_rid(0x1000), Integrity::Low);
        assert_eq!(Integrity::from_rid(0x2000), Integrity::Medium);
        assert_eq!(Integrity::from_rid(0x3000), Integrity::MediumPlus);
        assert_eq!(Integrity::from_rid(0x4000), Integrity::High);
        assert_eq!(Integrity::from_rid(0x5000), Integrity::System);
    }

    #[test]
    fn in_between_rids_fall_to_the_lower_named_level() {
        // Windows treats values between named levels as the lower level, not the higher one.
        assert_eq!(Integrity::from_rid(0x2100), Integrity::Medium);
        assert_eq!(Integrity::from_rid(0x4FFF), Integrity::High);
    }

    #[test]
    fn our_own_integrity_is_readable() {
        // The test process must be able to read its own token; failure means the SID walk is wrong.
        assert!(our_integrity().is_some());
    }

    #[test]
    fn a_normal_test_process_runs_at_medium_or_high() {
        let level = our_integrity().unwrap();
        assert!(level >= Integrity::Medium, "unexpected level {level:?}");
    }
}
