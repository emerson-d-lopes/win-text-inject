//! End-to-end injection against real applications, verified by reading the text back out.
//!
//! `WM_GETTEXT` cannot see Chromium, Electron or WinUI text, so verification here is done the way
//! any app will tell you the truth: select-all, copy, read the clipboard. That works uniformly
//! across Win32 edit controls, Chromium, and Electron editors.
//!
//! ```text
//! cargo run --example real_app_test -- --attach chrome.exe
//! cargo run --example real_app_test -- --attach code.exe
//! cargo run --example real_app_test -- --launch notepad.exe
//! ```

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_A, VK_C, VK_CONTROL, VK_DELETE, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use win_text_inject::{clipboard, inject, modifiers, Chord, Options, Outcome, Strategy, Target};

const OLD: &str = "USER PREVIOUS CLIPBOARD";
const TRANSCRIPT: &str = "the quick brown fox jumps over the lazy dog";

static mut FOUND: Vec<isize> = Vec::new();

unsafe extern "system" fn collect_top(hwnd: HWND, _: LPARAM) -> windows::core::BOOL {
    #[allow(static_mut_refs)]
    FOUND.push(hwnd.0 as isize);
    true.into()
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn chord3(m1: VIRTUAL_KEY, m2: VIRTUAL_KEY, k: VIRTUAL_KEY) {
    let inputs = [
        key(m1, false),
        key(m2, false),
        key(k, false),
        key(k, true),
        key(m2, true),
        key(m1, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn chord(modifier: VIRTUAL_KEY, k: VIRTUAL_KEY) {
    let inputs = [
        key(modifier, false),
        key(k, false),
        key(k, true),
        key(modifier, true),
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Read the target's field contents by making the app copy them to the clipboard.
///
/// App-agnostic: works where WM_GETTEXT and UI Automation do not.
fn copy_back(terminal: bool) -> Option<String> {
    let before = clipboard::sequence_number();
    let _ = modifiers::sanitize();
    // Terminals use Ctrl+Shift+A / Ctrl+Shift+C; plain Ctrl+A is a shell line-editing binding.
    if terminal {
        chord3(VK_CONTROL, VK_SHIFT, VK_A);
        std::thread::sleep(Duration::from_millis(200));
        chord3(VK_CONTROL, VK_SHIFT, VK_C);
    } else {
        chord(VK_CONTROL, VK_A);
        std::thread::sleep(Duration::from_millis(150));
        chord(VK_CONTROL, VK_C);
    }

    // Poll rather than sleep a fixed amount: the app copies when it gets round to it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if clipboard::sequence_number() != before {
            std::thread::sleep(Duration::from_millis(120));
            return clipboard::Snapshot::capture()
                .ok()
                .and_then(|s| s.text().map(|t| t.to_owned()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Leave the target's field the way we found it.
///
/// Skipped for terminals: there is no editable field to clear, and Ctrl+A then Delete at a shell
/// prompt would edit whatever command line is sitting there.
fn clear_field(terminal: bool) {
    if terminal {
        return;
    }
    let _ = modifiers::sanitize();
    chord(VK_CONTROL, VK_A);
    std::thread::sleep(Duration::from_millis(100));
    let inputs = [key(VK_DELETE, false), key(VK_DELETE, true)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
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

/// Bring a window belonging to `exe` to the foreground.
///
/// Test-harness only. A real dictation app must never steal focus; it injects into whatever the
/// user already focused.
fn force_foreground(exe: &str) -> bool {
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
            if pid == 0 || process_exe(pid).as_deref() != Some(exe) {
                continue;
            }
            let mut title = [0u16; 8];
            if GetWindowTextW(hwnd, &mut title) == 0 {
                continue;
            }
            let _ = ShowWindow(hwnd, SW_RESTORE);

            // SetForegroundWindow is refused across the foreground lock; attaching to the current
            // foreground thread's input queue lifts that for the duration of the call.
            let fg = GetForegroundWindow();
            let fg_thread = GetWindowThreadProcessId(fg, None);
            let ours = GetCurrentThreadId();
            let attached = fg_thread != ours && AttachThreadInput(ours, fg_thread, true).as_bool();
            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);
            if attached {
                let _ = AttachThreadInput(ours, fg_thread, false);
            }
            return true;
        }
        false
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().cloned().unwrap_or_else(|| "--attach".into());
    let exe = args.get(1).cloned().unwrap_or_else(|| "chrome.exe".into());

    let mut child = None;
    if mode == "--launch" {
        println!("launching {exe}");
        child = std::process::Command::new(&exe).spawn().ok();
        std::thread::sleep(Duration::from_millis(2500));
    }

    println!("bringing {exe} to the foreground...");
    force_foreground(&exe);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if matches!(Target::foreground(), Ok(ref t) if t.exe == exe) {
            break;
        }
        if Instant::now() > deadline {
            eprintln!("FAIL: {exe} never took the foreground");
            std::process::exit(2);
        }
        force_foreground(&exe);
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_millis(700));

    // The launching console can steal the foreground back during the settle sleep. Re-assert and
    // re-check rather than aborting on a transient.
    for _ in 0..5 {
        if matches!(Target::foreground(), Ok(ref t) if t.exe == exe) {
            break;
        }
        force_foreground(&exe);
        std::thread::sleep(Duration::from_millis(400));
    }

    let target = Target::foreground().expect("foreground target");

    // Abort before injecting, not after: injecting into an unintended app pastes test text into
    // someone's real work.
    if target.exe != exe {
        eprintln!("ABORT: foreground is '{}', expected '{}'", target.exe, exe);
        std::process::exit(2);
    }

    // --chord lets a chord be verified against a real app instead of assumed from documentation.
    let chord_override = args.iter().position(|a| a == "--chord").and_then(|i| {
        args.get(i + 1).and_then(|c| match c.as_str() {
            "ctrlv" => Some(Chord::CtrlV),
            "ctrlshiftv" => Some(Chord::CtrlShiftV),
            "shiftinsert" => Some(Chord::ShiftInsert),
            _ => None,
        })
    });
    let auto_chord = chord_override.unwrap_or_else(|| Chord::for_exe(&target.exe));
    println!("target: exe={} class={}", target.exe, target.class);
    println!(
        "chord: {auto_chord:?}{}\n",
        if chord_override.is_some() {
            " (override)"
        } else {
            " (from table)"
        }
    );

    let is_terminal = matches!(
        target.exe.as_str(),
        "windowsterminal.exe" | "conhost.exe" | "mintty.exe" | "putty.exe"
    );

    // Start from an empty field so the readback is unambiguous.
    clear_field(is_terminal);
    std::thread::sleep(Duration::from_millis(200));

    clipboard::set_text_private(OLD).expect("seed clipboard");

    let outcome = inject(
        &target,
        TRANSCRIPT,
        Options {
            strategy: Strategy::ClipboardPaste,
            chord: Some(auto_chord),
            ..Default::default()
        },
    );
    println!("outcome: {outcome:?}");

    // Check the restore BEFORE copy-back, which necessarily overwrites the clipboard.
    std::thread::sleep(Duration::from_millis(300));
    let clipboard_after = clipboard::Snapshot::capture()
        .ok()
        .and_then(|s| s.text().map(|t| t.to_owned()));
    let restored = clipboard_after.as_deref() == Some(OLD);

    let field = copy_back(is_terminal);
    let delivered = field
        .as_deref()
        .map(|f| f.contains(TRANSCRIPT))
        .unwrap_or(false);
    let exact = field.as_deref() == Some(TRANSCRIPT);

    clear_field(is_terminal);

    println!(
        "\nfield contents  : {:?}",
        field.as_deref().unwrap_or("<unreadable>")
    );
    println!("delivered       : {delivered}");
    println!("exact match     : {exact}");
    println!("clipboard kept  : {restored}");
    println!(
        "read confirmed  : {}",
        matches!(
            outcome,
            Ok(Outcome::Pasted {
                read_confirmed: true
            })
        )
    );

    if let Some(c) = child.as_mut() {
        let _ = c.kill();
    }

    if delivered && restored {
        println!("\nPASS ({exe})");
    } else {
        println!("\nFAIL ({exe})");
        std::process::exit(1);
    }
}
