//! Reproduction harness for the "pastes my old clipboard instead of the transcript" bug.
//!
//! Handy issue #502, open since 2025-12-30 with 52 comments and no fix; the shipped mitigation is a
//! user-tunable delay slider. Users report it still occurring at 400 ms.
//!
//! The mechanism under test: a paste target reads the clipboard *asynchronously* after receiving the
//! paste keystroke. An injector that restores the previous clipboard on a fixed timer will restore
//! before a busy target has read, and the target then reads the restored (old) content.
//!
//! This harness creates a real window that processes a real `WM_PASTE`, with a controllable delay
//! standing in for message-pump backlog under load. `WM_PASTE` rather than synthesized Ctrl+V
//! deliberately: what is under test is clipboard read timing, and depending on foreground
//! activation would make the result flaky for reasons unrelated to the bug. Run:
//!
//! ```text
//! cargo run --example repro_502
//! ```

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_CONTROL, VK_V};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, MSG, PM_REMOVE, SW_SHOW, WM_DESTROY, WM_KEYDOWN, WM_PASTE, WM_QUIT, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

const CF_UNICODETEXT: u32 = 13;

/// Milliseconds the mock target waits after receiving Ctrl+V before reading the clipboard.
/// Stands in for a real application's message-pump backlog under load.
static CONSUMER_LAG_MS: AtomicU64 = AtomicU64::new(0);
static PASTE_SEEN: AtomicBool = AtomicBool::new(false);
static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);
static READ_SIGNALLED: AtomicBool = AtomicBool::new(false);
static RESTORE_ERR: AtomicBool = AtomicBool::new(false);

fn received() -> &'static Mutex<Option<String>> {
    static R: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // WM_PASTE is how a paste actually reaches an edit control. Posting it directly keeps the
        // harness deterministic: what is under test is the clipboard read timing, not the keyboard
        // path, and relying on foreground activation makes the result flaky for unrelated reasons.
        WM_PASTE => {
            // The application is busy; the paste is queued behind other work.
            std::thread::sleep(Duration::from_millis(
                CONSUMER_LAG_MS.load(Ordering::SeqCst),
            ));
            *received().lock().unwrap() = read_clipboard_text();
            PASTE_SEEN.store(true, Ordering::SeqCst);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl_down = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
            if ctrl_down && wp.0 as u16 == VK_V.0 {
                std::thread::sleep(Duration::from_millis(
                    CONSUMER_LAG_MS.load(Ordering::SeqCst),
                ));
                *received().lock().unwrap() = read_clipboard_text();
                PASTE_SEEN.store(true, Ordering::SeqCst);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn read_clipboard_text() -> Option<String> {
    unsafe {
        // The clipboard is a global lock. Without a retry a momentarily-contended open returns
        // None, which reads as "the content was lost" and makes every measurement flaky.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(Some(HWND::default())).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !opened {
            return None;
        }
        let result = (|| {
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
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(hglobal);
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

fn create_target_window() -> HWND {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = w!("WinTextInjectRepro502");

        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            Default::default(),
            class,
            w!("repro 502 target"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            200,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .expect("create window");

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        TARGET_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        hwnd
    }
}

/// Pump messages until the target has recorded a paste, or the budget expires.
fn pump_until_paste(budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    unsafe {
        let mut msg = MSG::default();
        while std::time::Instant::now() < deadline {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if PASTE_SEEN.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Algorithm {
    /// What every surveyed tool does: restore on a fixed timer, unconditionally.
    UnconditionalRestore,
    /// Restore only if the clipboard sequence number is still ours.
    SequenceGatedRestore,
    /// Do not restore at all.
    NoRestore,
    /// Publish a promise, restore only after Windows reports the target actually read it.
    DelayedRender,
}

impl Algorithm {
    fn name(self) -> &'static str {
        match self {
            Algorithm::UnconditionalRestore => "unconditional restore (Handy)",
            Algorithm::SequenceGatedRestore => "sequence-gated restore",
            Algorithm::NoRestore => "no restore",
            Algorithm::DelayedRender => "delayed render (the fix)",
        }
    }
}

/// Run one trial. Returns what the target actually received.
fn trial(algorithm: Algorithm, consumer_lag_ms: u64, post_paste_ms: u64) -> (Option<String>, bool) {
    const OLD: &str = "PREVIOUS CLIPBOARD";
    const NEW: &str = "dictated transcript";

    PASTE_SEEN.store(false, Ordering::SeqCst);
    *received().lock().unwrap() = None;
    CONSUMER_LAG_MS.store(consumer_lag_ms, Ordering::SeqCst);

    // Seed the user's existing clipboard content.
    win_text_inject::clipboard::set_text_private(OLD).unwrap();

    let snapshot = win_text_inject::clipboard::Snapshot::capture().unwrap();

    // The delayed path publishes a promise instead of the text itself.
    let offer = if algorithm == Algorithm::DelayedRender {
        Some(win_text_inject::Offer::publish(NEW).unwrap())
    } else {
        win_text_inject::clipboard::set_text_private(NEW).unwrap();
        None
    };
    let ours = win_text_inject::clipboard::sequence_number();

    std::thread::sleep(Duration::from_millis(30));

    // Deliver the paste, then let the target process it while we run the restore timer.
    let injector = std::thread::spawn(move || {
        deliver_paste();
        match algorithm {
            Algorithm::UnconditionalRestore => {
                std::thread::sleep(Duration::from_millis(post_paste_ms));
                let _ = win_text_inject::clipboard::set_text_private(OLD);
            }
            Algorithm::SequenceGatedRestore => {
                std::thread::sleep(Duration::from_millis(post_paste_ms));
                let _ = snapshot.restore_if_ours(ours);
            }
            Algorithm::NoRestore => {}
            Algorithm::DelayedRender => {
                // No guessing: this returns the moment the target actually reads the clipboard.
                let read = offer
                    .as_ref()
                    .map(|o| o.wait_for_read(Duration::from_secs(3)))
                    .unwrap_or(false);
                READ_SIGNALLED.store(read, Ordering::SeqCst);
                if read {
                    let r = win_text_inject::clipboard::set_text_private(OLD);
                    RESTORE_ERR.store(r.is_err(), Ordering::SeqCst);
                }
            }
        }
    });

    pump_until_paste(Duration::from_millis(
        consumer_lag_ms + post_paste_ms + 2000,
    ));
    let _ = injector.join();

    let got = received().lock().unwrap().clone();
    // Did the user's original clipboard survive? "no restore" gets the paste right by simply
    // never restoring, which is not a fix -- it silently destroys whatever the user had copied.
    let restored = read_clipboard_text().as_deref() == Some(OLD);
    (got, restored)
}

fn deliver_paste() {
    let hwnd = HWND(TARGET_HWND.load(Ordering::SeqCst) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_PASTE, WPARAM(0), LPARAM(0));
    }
}

fn main() {
    const NEW: &str = "dictated transcript";
    const POST_PASTE_MS: u64 = 120;

    println!("repro: Handy #502 -- clipboard restored before the target read it\n");
    println!("injector restore delay: {POST_PASTE_MS} ms (Handy default is user-tuned)\n");

    create_target_window();
    // Let the window settle and take foreground before any synthesized input.
    std::thread::sleep(Duration::from_millis(400));
    pump_until_paste(Duration::from_millis(200));

    let lags = [10u64, 60, 150, 400];
    let algorithms = [
        Algorithm::UnconditionalRestore,
        Algorithm::SequenceGatedRestore,
        Algorithm::NoRestore,
        Algorithm::DelayedRender,
    ];

    let mut failures = 0;
    println!(
        "{:<38} {:>10}  {:<24} {:<10} verdict",
        "algorithm", "app lag", "target received", "clipboard"
    );
    println!("{}", "-".repeat(104));

    for algorithm in algorithms {
        for lag in lags {
            let (got, restored) = trial(algorithm, lag, POST_PASTE_MS);
            let text_ok = got.as_deref() == Some(NEW);
            let ok = text_ok && restored;
            if !ok {
                failures += 1;
            }
            println!(
                "{:<38} {:>7} ms  {:<24} {:<10} {}",
                algorithm.name(),
                lag,
                got.as_deref().unwrap_or("<nothing>"),
                if restored {
                    "restored".to_string()
                } else {
                    format!(
                        "LOST r={} e={}",
                        READ_SIGNALLED.load(Ordering::SeqCst),
                        RESTORE_ERR.load(Ordering::SeqCst)
                    )
                },
                if ok {
                    "ok"
                } else if !text_ok {
                    "WRONG TEXT"
                } else {
                    "CLIPBOARD LOST"
                }
            );
        }
        println!();
    }

    println!("{failures} failing trial(s)");
    println!(
        "\nA restore that fires before the target has read the clipboard delivers the user's\n\
         previous clipboard instead of the transcript. A sequence-number gate does not help:\n\
         the sequence number is still ours, because nobody else wrote -- we clobbered ourselves.\n\
         Delayed rendering removes the guess: WM_RENDERFORMAT arrives exactly when the target\n\
         reads, so the restore is sequenced after the read rather than racing it."
    );
}
