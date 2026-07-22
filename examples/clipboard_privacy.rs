//! Proves the privacy opt-out formats are really attached to the clipboard.
//!
//! Run with `cargo run --example clipboard_privacy`. Enumerates every format present after a write
//! and reports which of the four opt-outs are attached, then round-trips the text.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EnumClipboardFormats, GetClipboardFormatNameW,
    OpenClipboard, RegisterClipboardFormatW,
};

const EXPECTED: [PCWSTR; 4] = [
    w!("ExcludeClipboardContentFromMonitorProcessing"),
    w!("CanIncludeInClipboardHistory"),
    w!("CanUploadToCloudClipboard"),
    w!("Clipboard Viewer Ignore"),
];

fn format_name(id: u32) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClipboardFormatNameW(id, &mut buf) };
    if len == 0 {
        // Standard formats have no registered name.
        match id {
            1 => "CF_TEXT".into(),
            13 => "CF_UNICODETEXT".into(),
            16 => "CF_LOCALE".into(),
            _ => format!("<standard {id}>"),
        }
    } else {
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

fn present_formats() -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        if OpenClipboard(Some(HWND::default())).is_err() {
            return out;
        }
        let count = CountClipboardFormats();
        let mut current = 0u32;
        for _ in 0..count.max(0) {
            current = EnumClipboardFormats(current);
            if current == 0 {
                break;
            }
            out.push(current);
        }
        let _ = CloseClipboard();
    }
    out
}

fn main() -> Result<(), win_text_inject::Error> {
    let secret = "transcript that must not reach cloud clipboard";

    println!("writing via set_text_private...\n");
    win_text_inject::clipboard::set_text_private(secret)?;

    let present = present_formats();
    println!("formats on clipboard ({}):", present.len());
    for id in &present {
        println!("  {:>6}  {}", id, format_name(*id));
    }

    println!("\nprivacy opt-outs:");
    let mut all_present = true;
    for name in EXPECTED {
        let id = unsafe { RegisterClipboardFormatW(name) };
        let attached = present.contains(&id);
        all_present &= attached;
        println!(
            "  [{}] {}",
            if attached { "x" } else { " " },
            format_name(id)
        );
    }

    let snapshot = win_text_inject::clipboard::Snapshot::capture()?;
    println!(
        "\nround-trip: {}",
        if snapshot_matches(&snapshot, secret) {
            "ok"
        } else {
            "MISMATCH"
        }
    );

    println!(
        "\nresult: {}",
        if all_present {
            "all four opt-outs attached"
        } else {
            "SOME OPT-OUTS MISSING"
        }
    );

    std::process::exit(if all_present { 0 } else { 1 });
}

fn snapshot_matches(snapshot: &win_text_inject::clipboard::Snapshot, expected: &str) -> bool {
    snapshot.text() == Some(expected)
}
