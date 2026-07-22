//! Clipboard writes that stay out of clipboard history, cloud sync, and third-party managers.
//!
//! Every dictation tool surveyed writes `CF_UNICODETEXT` and nothing else, so transcripts land in
//! Windows clipboard history and sync to the Microsoft cloud clipboard. Chrome's Incognito mode
//! avoids that by registering four extra formats; so does this.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::Error;

/// Declared locally rather than via the `Win32_System_Ole` feature: the OLE module is a large
/// dependency to pull in for one stable, documented constant.
const CF_UNICODETEXT: u32 = 13;

/// Opt-out formats honored by Windows clipboard history, the cloud clipboard, and well-behaved
/// third-party managers (KeePassXC and Windows Credential Manager use the same set).
const PRIVACY_FORMATS: [PCWSTR; 4] = [
    w!("ExcludeClipboardContentFromMonitorProcessing"),
    w!("CanIncludeInClipboardHistory"),
    w!("CanUploadToCloudClipboard"),
    w!("Clipboard Viewer Ignore"),
];

/// `ExcludeClipboardContentFromMonitorProcessing` and `Clipboard Viewer Ignore` are presence-only
/// flags; the other two are read as a `DWORD` and must be zero to mean "no".
fn privacy_payload(index: usize) -> Option<u32> {
    match index {
        1 | 2 => Some(0u32),
        _ => None,
    }
}

/// Number of retries when another process holds the clipboard lock.
const OPEN_RETRIES: u32 = 5;
const OPEN_BACKOFF_MS: u64 = 12;

/// RAII guard so the clipboard is always closed, including on early return or panic.
pub(crate) struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<Self, Error> {
        Self::open_owned_by(HWND::default())
    }

    /// Open with `owner` recorded as the clipboard owner, so delayed-render messages are routed to
    /// it. The retry loop matters: the clipboard is a global lock and a contended open otherwise
    /// fails outright with `ERROR_ACCESS_DENIED`.
    pub(crate) fn open_owned_by(owner: HWND) -> Result<Self, Error> {
        let mut last = windows::core::Error::empty();
        for attempt in 0..OPEN_RETRIES {
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => return Ok(Self),
                Err(e) => {
                    last = e;
                    if attempt + 1 < OPEN_RETRIES {
                        std::thread::sleep(std::time::Duration::from_millis(OPEN_BACKOFF_MS));
                    }
                }
            }
        }
        Err(Error::ClipboardLocked(last))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Allocate a moveable global block and copy `bytes` into it.
///
/// Ownership transfers to the clipboard on a successful `SetClipboardData`, so this must not be
/// freed afterwards.
fn alloc_global(bytes: &[u8]) -> Result<HGLOBAL, Error> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(Error::Alloc)?;
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            return Err(Error::Alloc(windows::core::Error::from_win32()));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(handle);
        Ok(handle)
    }
}

fn utf16_bytes(text: &str) -> Vec<u8> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// Write `text` to the clipboard tagged so it is excluded from clipboard history, cloud sync, and
/// cooperating clipboard managers.
///
/// This is the write every dictation tool should be doing and none currently do.
pub fn set_text_private(text: &str) -> Result<(), Error> {
    let _guard = ClipboardGuard::open()?;
    unsafe {
        EmptyClipboard().map_err(Error::Clipboard)?;
        let payload = alloc_global(&utf16_bytes(text))?;
        SetClipboardData(CF_UNICODETEXT, Some(HANDLE(payload.0))).map_err(Error::Clipboard)?;
        attach_privacy_formats();
    }
    Ok(())
}

/// Attach the four opt-out formats to the currently open clipboard.
///
/// Caller must already hold the clipboard open. Failures are swallowed deliberately: the text is
/// already published, and a missing opt-out is a privacy downgrade rather than a functional break.
pub(crate) fn attach_privacy_formats() {
    for (index, name) in PRIVACY_FORMATS.iter().enumerate() {
        let id = unsafe { RegisterClipboardFormatW(*name) };
        if id == 0 {
            continue;
        }
        let data = match privacy_payload(index) {
            Some(dword) => match alloc_global(&dword.to_le_bytes()) {
                Ok(h) => Some(HANDLE(h.0)),
                Err(_) => continue,
            },
            None => None,
        };
        let _ = unsafe { SetClipboardData(id, data) };
    }
}

pub(crate) const CF_UNICODETEXT_PUBLIC: u32 = CF_UNICODETEXT;

pub(crate) fn alloc_global_public(bytes: &[u8]) -> Result<HGLOBAL, Error> {
    alloc_global(bytes)
}

pub(crate) fn utf16_bytes_public(text: &str) -> Vec<u8> {
    utf16_bytes(text)
}

/// A best-effort snapshot of the clipboard's text content plus the sequence number observed when it
/// was taken.
///
/// Deliberately text-only. Delayed-rendered formats cannot be captured by value at all, and copying
/// a large `CF_DIB` to restore it later duplicates it in this process's memory. Restoring only what
/// can be restored correctly beats pretending to restore everything.
pub struct Snapshot {
    text: Option<String>,
    sequence: u32,
}

impl Snapshot {
    pub fn capture() -> Result<Self, Error> {
        let _guard = ClipboardGuard::open()?;
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let text = unsafe { read_unicode_text() };
        Ok(Self { text, sequence })
    }

    /// The captured text, if the clipboard held any.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// True when another process wrote to the clipboard after this snapshot was taken.
    pub fn superseded(&self) -> bool {
        unsafe { GetClipboardSequenceNumber() != self.sequence }
    }

    /// Restore the captured text unconditionally.
    ///
    /// Correct only when the caller already knows the target has read the clipboard — i.e. after
    /// [`crate::delayed::Offer::wait_for_read`] returned true. Otherwise use [`Snapshot::restore_if_ours`].
    pub fn restore(&self) -> Result<(), Error> {
        match &self.text {
            Some(text) => set_text_private(text),
            None => {
                let _guard = ClipboardGuard::open()?;
                unsafe { EmptyClipboard().map_err(Error::Clipboard) }
            }
        }
    }

    /// Restore the captured text only if the clipboard still holds our own write.
    ///
    /// `expected` is the sequence number produced by our own write. If the live sequence number is
    /// neither our write nor the original, a third party owns the clipboard and restoring would
    /// clobber it, so this does nothing.
    pub fn restore_if_ours(&self, expected: u32) -> Result<bool, Error> {
        let current = unsafe { GetClipboardSequenceNumber() };
        if current != expected {
            return Ok(false);
        }
        match &self.text {
            Some(text) => {
                set_text_private(text)?;
                Ok(true)
            }
            None => {
                let _guard = ClipboardGuard::open()?;
                unsafe { EmptyClipboard().map_err(Error::Clipboard)? };
                Ok(true)
            }
        }
    }
}

/// Caller must hold the clipboard open.
unsafe fn read_unicode_text() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
    let hglobal = HGLOBAL(handle.0);
    let ptr = GlobalLock(hglobal) as *const u16;
    if ptr.is_null() {
        return None;
    }
    let bytes = GlobalSize(hglobal);
    let max_units = bytes / 2;
    let mut len = 0usize;
    while len < max_units && *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    let text = String::from_utf16_lossy(slice);
    let _ = GlobalUnlock(hglobal);
    Some(text)
}

/// Current clipboard sequence number, for pairing a write with a later restore decision.
pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_bytes_are_nul_terminated_little_endian() {
        assert_eq!(utf16_bytes("hi"), vec![b'h', 0, b'i', 0, 0, 0]);
    }

    #[test]
    fn utf16_bytes_encodes_surrogate_pairs() {
        // U+1F600 encodes to the pair D83D DE00.
        assert_eq!(utf16_bytes("\u{1F600}"), vec![0x3D, 0xD8, 0x00, 0xDE, 0, 0]);
    }

    #[test]
    fn only_the_dword_formats_carry_a_payload() {
        assert_eq!(privacy_payload(0), None);
        assert_eq!(privacy_payload(1), Some(0));
        assert_eq!(privacy_payload(2), Some(0));
        assert_eq!(privacy_payload(3), None);
    }
}
