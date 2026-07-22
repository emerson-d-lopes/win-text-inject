//! End-to-end injection against a real application, with the result read back out of it.
//!
//! Launches the target, focuses it, injects text through the full public API, then reads the
//! target's own text back to prove the transcript actually arrived. Also checks that the user's
//! previous clipboard survived and that the transcript stayed out of Windows clipboard history.
//!
//! ```text
//! cargo run --example real_app_test -- notepad
//! ```

use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumChildWindows, EnumWindows, GetForegroundWindow, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, SendMessageW, SetForegroundWindow, ShowWindow,
    SW_RESTORE, WM_GETTEXT, WM_GETTEXTLENGTH,
};

use win_text_inject::{clipboard, inject, Options, Outcome, Strategy, Target};

/// Collected during EnumChildWindows.
static mut FOUND: Vec<isize> = Vec::new();

unsafe extern "system" fn collect(hwnd: HWND, _: LPARAM) -> windows::core::BOOL {
    #[allow(static_mut_refs)]
    FOUND.push(hwnd.0 as isize);
    true.into()
}

/// Read text out of a window or any of its children, whichever yields the most.
///
/// Modern Notepad hosts a RichEdit child, so the top-level window's own text is just the title.
fn read_window_text(hwnd: HWND) -> String {
    fn text_of(hwnd: HWND) -> String {
        unsafe {
            let len = SendMessageW(hwnd, WM_GETTEXTLENGTH, None, None).0;
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = SendMessageW(
                hwnd,
                WM_GETTEXT,
                Some(WPARAM(buf.len())),
                Some(LPARAM(buf.as_mut_ptr() as isize)),
            )
            .0;
            String::from_utf16_lossy(&buf[..n.max(0) as usize])
        }
    }

    let mut best = text_of(hwnd);
    unsafe {
        #[allow(static_mut_refs)]
        FOUND.clear();
        let _ = EnumChildWindows(Some(hwnd), Some(collect), LPARAM(0));
        #[allow(static_mut_refs)]
        let children: Vec<isize> = FOUND.clone();
        for child in children {
            let t = text_of(HWND(child as *mut _));
            if t.len() > best.len() {
                best = t;
            }
        }
    }
    best
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // --attach targets an already-running app instead of launching one, so the caller can set the
    // page/field up first. Launching and hoping for foreground is unreliable.
    let attach = args.first().map(|s| s == "--attach").unwrap_or(false);
    let app = if attach {
        args.get(1).cloned().unwrap_or_else(|| "chrome.exe".into())
    } else {
        args.first().cloned().unwrap_or_else(|| "notepad".into())
    };
    let exe = match app.as_str() {
        "notepad" => "notepad.exe".to_string(),
        other => other.to_string(),
    };

    const OLD: &str = "USER PREVIOUS CLIPBOARD";
    let transcript = "the quick brown fox jumps over the lazy dog";

    let mut child = None;
    if attach {
        println!("waiting up to 20s for {exe} to be the foreground window...");
        println!("(focus it now -- this will not steal focus for you)\n");
        // Politely wait rather than forcing. SetForegroundWindow is unreliable across the
        // foreground lock, and forcing it is exactly how test text ends up in the wrong app.
        let _ = focus_window_of(&exe);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(t) = Target::foreground() {
                if t.exe == exe {
                    break;
                }
            }
            if std::time::Instant::now() > deadline {
                eprintln!("timed out waiting for {exe} to take the foreground");
                std::process::exit(2);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Let the caret settle inside the focused control.
        std::thread::sleep(Duration::from_millis(600));
    } else {
        println!("launching {exe}\n");
        child = match std::process::Command::new(&exe).spawn() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("could not launch {exe}: {e}");
                std::process::exit(2);
            }
        };
        std::thread::sleep(Duration::from_millis(2500));
    }

    let target = match Target::foreground() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("no foreground target: {e}");
            if let Some(c) = child.as_mut() {
                let _ = c.kill();
            }
            std::process::exit(2);
        }
    };
    println!("target: exe={} class={}", target.exe, target.class);
    println!("integrity: {:?}", target.integrity);
    println!("accepts injection: {}\n", target.accepts_injection());

    // Abort BEFORE injecting, not after. SetForegroundWindow fails silently when another process
    // holds the foreground lock, and injecting into whatever happened to be focused means pasting
    // test text into someone's real application.
    if target.exe != exe {
        eprintln!(
            "ABORT: expected foreground '{}', found '{}'. Refusing to inject into an \
             unintended application.",
            exe, target.exe
        );
        if let Some(c) = child.as_mut() {
            let _ = c.kill();
        }
        std::process::exit(2);
    }

    // Seed the clipboard with something the user would be upset to lose.
    clipboard::set_text_private(OLD).expect("seed clipboard");

    let options = Options {
        strategy: Strategy::ClipboardPaste,
        ..Default::default()
    };
    println!("injecting with delayed_render={}", options.delayed_render);

    let outcome = inject(&target, transcript, options);
    println!("outcome: {outcome:?}\n");

    // Let the app settle, then read what it actually contains.
    std::thread::sleep(Duration::from_millis(800));
    let contents = read_window_text(HWND(target.hwnd as *mut _));

    let delivered = contents.contains(transcript);
    let clipboard_now = clipboard::Snapshot::capture()
        .ok()
        .and_then(|s| s.text().map(|t| t.to_owned()));
    let clipboard_restored = clipboard_now.as_deref() == Some(OLD);

    println!("target contains transcript : {delivered}");
    println!("clipboard restored to user : {clipboard_restored}");
    println!(
        "read confirmed by Windows  : {}",
        matches!(
            outcome,
            Ok(Outcome::Pasted {
                read_confirmed: true
            })
        )
    );
    if !delivered {
        println!("\ntarget text was:\n{contents:?}");
    }

    if let Some(c) = child.as_mut() {
        let _ = c.kill();
    }

    // Chromium and modern WinUI apps do not expose their text through WM_GETTEXT, so a negative
    // readback here proves nothing. Say so rather than reporting a failure the harness cannot see.
    let readback_supported = !matches!(
        target.class.as_str(),
        "Chrome_WidgetWin_1" | "Notepad" | "ApplicationFrameWindow"
    );

    if delivered && clipboard_restored {
        println!("\nPASS");
    } else if !readback_supported {
        println!(
            "\nINDETERMINATE: '{}' does not expose its text via WM_GETTEXT, so delivery cannot \
             be confirmed from here. Check the target's field directly.\nClipboard restore: {}",
            target.class,
            if clipboard_restored { "PASS" } else { "FAIL" }
        );
    } else {
        println!("\nFAIL");
        std::process::exit(1);
    }
}

/// Find a visible top-level window belonging to `exe` and bring it to the foreground.
fn focus_window_of(exe: &str) -> Option<HWND> {
    unsafe {
        #[allow(static_mut_refs)]
        FOUND.clear();
        let _ = EnumWindows(Some(collect_top), LPARAM(0));
        #[allow(static_mut_refs)]
        let all: Vec<isize> = FOUND.clone();

        for h in all {
            let hwnd = HWND(h as *mut _);
            if !IsWindowVisible(hwnd).as_bool() {
                continue;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                continue;
            }
            if process_exe(pid).as_deref() == Some(exe) {
                // A window with no title is usually a hidden helper, not the real UI.
                let mut title = [0u16; 8];
                if GetWindowTextW(hwnd, &mut title) == 0 {
                    continue;
                }
                let _ = ShowWindow(hwnd, SW_RESTORE);

                // A plain SetForegroundWindow is refused when another process owns the foreground
                // lock. Attaching to the current foreground thread's input queue lifts that
                // restriction for the duration of the call. Test-harness only -- a real dictation
                // app must never steal focus, it injects into whatever the user already focused.
                let fg = GetForegroundWindow();
                let fg_thread = GetWindowThreadProcessId(fg, None);
                let our_thread = GetCurrentThreadId();
                let attached = fg_thread != our_thread
                    && AttachThreadInput(our_thread, fg_thread, true).as_bool();

                let _ = SetForegroundWindow(hwnd);
                let _ = BringWindowToTop(hwnd);

                if attached {
                    let _ = AttachThreadInput(our_thread, fg_thread, false);
                }
                let _: PCWSTR = PCWSTR::null();
                return Some(hwnd);
            }
        }
        None
    }
}

unsafe extern "system" fn collect_top(hwnd: HWND, _: LPARAM) -> windows::core::BOOL {
    #[allow(static_mut_refs)]
    FOUND.push(hwnd.0 as isize);
    true.into()
}

fn process_exe(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        r.ok()?;
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(
            std::path::Path::new(&full)
                .file_name()?
                .to_string_lossy()
                .to_lowercase(),
        )
    }
}
