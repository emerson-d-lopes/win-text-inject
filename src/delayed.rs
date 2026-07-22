//! Delayed clipboard rendering: know exactly when the target read the clipboard.
//!
//! The "it pasted my previous clipboard" bug exists because the injector restores on a timer while
//! the target reads the clipboard whenever its message pump gets round to it. Any fixed delay is a
//! guess, and under load the guess is wrong. Tuning the delay upward — which is the shipped
//! mitigation in every tool surveyed — only moves the threshold.
//!
//! Delayed rendering removes the guess. Instead of publishing the text, publish a promise:
//! `SetClipboardData(CF_UNICODETEXT, NULL)` with this process as clipboard owner. Windows then
//! sends `WM_RENDERFORMAT` to the owner at the instant a consumer actually asks for the data, and
//! the owner supplies it then. That message *is* the "the target has read it" signal, so the
//! restore can be sequenced strictly after the read instead of racing it.
//!
//! Requires a window with a running message pump, so this owns a hidden window on its own thread.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardOwner, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, CW_USEDEFAULT, MSG, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT, WNDCLASSW, WS_OVERLAPPED,
};

use crate::clipboard::{alloc_global_public, utf16_bytes_public, CF_UNICODETEXT_PUBLIC};
use crate::Error;

/// Text formats Windows may ask us to render. `CF_UNICODETEXT` is what we advertise; the others are
/// synthesized from it, and a consumer asking for one of those still routes back to us.
const CF_TEXT: u32 = 1;
const CF_OEMTEXT: u32 = 7;

struct State {
    /// Text to hand over when a consumer asks. Cleared once ownership is lost.
    pending: Option<String>,
    /// Set when a consumer actually requested the data.
    rendered: bool,
    /// How many times the data has been requested since publishing.
    ///
    /// Consumers are not guaranteed to read exactly once. Chromium in particular touches the
    /// clipboard more than once per paste, so the first render is not proof the paste completed.
    render_count: u32,
    /// Tick of the most recent render, for debouncing the restore.
    last_render: Option<std::time::Instant>,
}

fn state() -> &'static (Mutex<State>, Condvar) {
    static S: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
    S.get_or_init(|| {
        (
            Mutex::new(State {
                pending: None,
                rendered: false,
                render_count: 0,
                last_render: None,
            }),
            Condvar::new(),
        )
    })
}

static OWNER_HWND: AtomicIsize = AtomicIsize::new(0);
static THREAD_STARTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // A consumer asked for one specific format. The clipboard is already open by the requester,
        // so this must call SetClipboardData without opening it.
        WM_RENDERFORMAT => {
            render(wp.0 as u32);
            LRESULT(0)
        }
        // We are losing ownership or shutting down; supply everything we promised.
        WM_RENDERALLFORMATS => {
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                render(CF_UNICODETEXT_PUBLIC);
                let _ = unsafe { CloseClipboard() };
            }
            LRESULT(0)
        }
        // Another process took the clipboard. Our promise is void.
        WM_DESTROYCLIPBOARD => {
            let (lock, _) = state();
            if let Ok(mut s) = lock.lock() {
                s.pending = None;
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Supply the promised text for `format`, and record that a real read occurred.
fn render(format: u32) {
    if !matches!(format, CF_UNICODETEXT_PUBLIC | CF_TEXT | CF_OEMTEXT) {
        return;
    }
    let (lock, cvar) = state();
    let Ok(mut s) = lock.lock() else { return };
    let Some(text) = s.pending.clone() else {
        return;
    };

    if let Ok(handle) = alloc_global_public(&utf16_bytes_public(&text)) {
        // Deliberately always CF_UNICODETEXT: Windows synthesizes the narrow formats from it, so
        // one render satisfies a consumer that asked for any of them.
        let _ = unsafe { SetClipboardData(CF_UNICODETEXT_PUBLIC, Some(HANDLE(handle.0))) };
    }

    s.rendered = true;
    s.render_count += 1;
    s.last_render = Some(std::time::Instant::now());
    cvar.notify_all();
}

/// Start the owner window and its message pump. Idempotent.
fn ensure_owner() -> Result<HWND, Error> {
    if let 0 = OWNER_HWND.load(Ordering::SeqCst) {
    } else {
        return Ok(HWND(OWNER_HWND.load(Ordering::SeqCst) as *mut _));
    }

    if THREAD_STARTED.swap(true, Ordering::SeqCst) {
        // Another caller is mid-startup; wait for the handle to appear.
        for _ in 0..200 {
            let h = OWNER_HWND.load(Ordering::SeqCst);
            if h != 0 {
                return Ok(HWND(h as *mut _));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        return Err(Error::OwnerWindowFailed);
    }

    std::thread::Builder::new()
        .name("win-text-inject-clipboard-owner".into())
        .spawn(|| unsafe {
            let instance = match GetModuleHandleW(None) {
                Ok(i) => i,
                Err(_) => return,
            };
            let class = w!("WinTextInjectClipboardOwner");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);

            // Never shown. Not HWND_MESSAGE: message-only windows are not reliable clipboard
            // owners, and clipboard owner messages are sent directly to the window anyway.
            let hwnd = match CreateWindowExW(
                Default::default(),
                class,
                w!("win-text-inject clipboard owner"),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            ) {
                Ok(h) => h,
                Err(_) => return,
            };

            OWNER_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .map_err(|_| Error::OwnerWindowFailed)?;

    for _ in 0..200 {
        let h = OWNER_HWND.load(Ordering::SeqCst);
        if h != 0 {
            return Ok(HWND(h as *mut _));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(Error::OwnerWindowFailed)
}

/// A promise of text placed on the clipboard, not yet materialized.
pub struct Offer;

impl Offer {
    /// Advertise `text` on the clipboard without publishing it.
    ///
    /// Nothing is copied until a consumer asks, at which point [`Offer::wait_for_read`] returns.
    pub fn publish(text: &str) -> Result<Self, Error> {
        let hwnd = ensure_owner()?;

        {
            let (lock, _) = state();
            let mut s = lock.lock().map_err(|_| Error::OwnerWindowFailed)?;
            s.pending = Some(text.to_owned());
            s.rendered = false;
            s.render_count = 0;
            s.last_render = None;
        }

        {
            // Retrying open is essential here: a contended clipboard otherwise fails outright with
            // ERROR_ACCESS_DENIED, which in a dictation app means a dropped transcript.
            let _guard = crate::clipboard::ClipboardGuard::open_owned_by(hwnd)?;
            let result = unsafe {
                (|| {
                    EmptyClipboard().map_err(Error::Clipboard)?;
                    // NULL data is the promise; Windows comes back with WM_RENDERFORMAT.
                    //
                    // For delayed rendering SetClipboardData returns NULL on *success*, which the
                    // bindings surface as Err with a zeroed last-error. Only a non-zero code is a real
                    // failure here.
                    if let Err(e) = SetClipboardData(CF_UNICODETEXT_PUBLIC, None) {
                        if e.code().0 != 0 {
                            return Err(Error::Clipboard(e));
                        }
                    }
                    crate::clipboard::attach_privacy_formats();
                    Ok::<(), Error>(())
                })()
            };
            result?;

            // Ownership is the real confirmation the promise was accepted.
            if unsafe { GetClipboardOwner() }.unwrap_or_default() != hwnd {
                return Err(Error::OwnerWindowFailed);
            }
        }

        Ok(Self)
    }

    /// Block until a consumer actually read the clipboard, or `timeout` elapses.
    ///
    /// Returns `true` if the data was read. A `false` return means the paste never reached the
    /// target, which is itself useful: the caller can report that instead of silently assuming
    /// success.
    pub fn wait_for_read(&self, timeout: Duration) -> bool {
        let (lock, cvar) = state();
        let Ok(guard) = lock.lock() else { return false };
        let Ok((guard, _)) = cvar.wait_timeout_while(guard, timeout, |s| !s.rendered) else {
            return false;
        };
        guard.rendered
    }

    /// Wait for the first read, then until reads have been quiet for `quiet`.
    ///
    /// A single `WM_RENDERFORMAT` is *not* proof the paste completed. Chromium touches the
    /// clipboard more than once per paste — an early probe, then the real read — so restoring after
    /// the first render puts the old text back before the read that matters, which is precisely the
    /// bug this was meant to fix. Waiting for renders to go quiet covers multi-read consumers.
    ///
    /// Returns the number of reads observed, or `None` if none arrived within `timeout`.
    pub fn wait_for_reads_to_settle(&self, timeout: Duration, quiet: Duration) -> Option<u32> {
        if !self.wait_for_read(timeout) {
            return None;
        }
        loop {
            let last = {
                let (lock, _) = state();
                let s = lock.lock().ok()?;
                s.last_render?
            };
            let elapsed = last.elapsed();
            if elapsed >= quiet {
                break;
            }
            std::thread::sleep(quiet - elapsed);
        }
        let (lock, _) = state();
        let s = lock.lock().ok()?;
        Some(s.render_count)
    }

    /// Number of times a consumer has asked for the data since publishing.
    pub fn read_count(&self) -> u32 {
        state().0.lock().map(|s| s.render_count).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_window_starts_and_is_reused() {
        let a = ensure_owner().expect("owner window");
        let b = ensure_owner().expect("owner window again");
        assert_eq!(a.0, b.0);
        assert!(!a.0.is_null());
    }

    #[test]
    fn non_text_formats_are_not_rendered() {
        // CF_BITMAP (2) is not something we promise; asking for it must not mark a read.
        let (lock, _) = state();
        {
            let mut s = lock.lock().unwrap();
            s.pending = Some("x".into());
            s.rendered = false;
        }
        render(2);
        assert!(!lock.lock().unwrap().rendered);
    }
}
