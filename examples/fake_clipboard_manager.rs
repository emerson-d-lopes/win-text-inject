//! A clipboard manager stand-in, to test two risks that only appear when one is running.
//!
//! 1. **Privacy.** Does the manager capture dictated transcripts? A well-behaved one checks the
//!    opt-out formats; a naive one reads `CF_UNICODETEXT` and archives everything.
//! 2. **Correctness.** Delayed rendering treats `WM_RENDERFORMAT` as "the target read it". A
//!    manager that reads the clipboard on every change satisfies that render itself, which could
//!    make a paste that never landed look confirmed and trigger an early restore.
//!
//! ```text
//! cargo run --example fake_clipboard_manager            # honors opt-out formats
//! cargo run --example fake_clipboard_manager -- --naive # ignores them, reads everything
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, CountClipboardFormats, EnumClipboardFormats,
    GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    TranslateMessage, CW_USEDEFAULT, MSG, WM_CLIPBOARDUPDATE, WNDCLASSW, WS_OVERLAPPED,
};

const CF_UNICODETEXT: u32 = 13;
static NAIVE: AtomicBool = AtomicBool::new(false);

/// True when the clipboard carries any of the opt-out formats a manager is expected to honor.
fn is_excluded() -> bool {
    let names = [
        w!("ExcludeClipboardContentFromMonitorProcessing"),
        w!("CanIncludeInClipboardHistory"),
        w!("CanUploadToCloudClipboard"),
        w!("Clipboard Viewer Ignore"),
    ];
    let ids: Vec<u32> = names
        .iter()
        .map(|n| unsafe { RegisterClipboardFormatW(*n) })
        .collect();

    let mut present = Vec::new();
    unsafe {
        let count = CountClipboardFormats();
        let mut cur = 0u32;
        for _ in 0..count.max(0) {
            cur = EnumClipboardFormats(cur);
            if cur == 0 {
                break;
            }
            present.push(cur);
        }
    }
    ids.iter().any(|id| present.contains(id))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            let excluded = is_excluded();
            let naive = NAIVE.load(Ordering::SeqCst);

            if excluded && !naive {
                println!("[manager] change seen, opted out -- NOT reading");
            } else {
                // Reading is what triggers WM_RENDERFORMAT on a delayed-render promise.
                let text = unsafe { read_text() };
                match text {
                    Some(t) => println!("[manager] CAPTURED: {t:?}"),
                    None => println!("[manager] change seen, no text"),
                }
            }
            let _ = unsafe { CloseClipboard() };
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe fn read_text() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
    let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
    let ptr = GlobalLock(hglobal) as *const u16;
    if ptr.is_null() {
        return None;
    }
    let max = GlobalSize(hglobal) / 2;
    let mut len = 0usize;
    while len < max && *ptr.add(len) != 0 {
        len += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    let _ = GlobalUnlock(hglobal);
    Some(s)
}

fn main() {
    let naive = std::env::args().any(|a| a == "--naive");
    NAIVE.store(naive, Ordering::SeqCst);
    println!(
        "fake clipboard manager running ({})",
        if naive {
            "NAIVE - ignores opt-out formats"
        } else {
            "well-behaved - honors opt-out formats"
        }
    );

    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = w!("WinTextInjectFakeManager");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            Default::default(),
            class,
            w!("fake clipboard manager"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .expect("create window");

        AddClipboardFormatListener(hwnd).expect("register listener");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
