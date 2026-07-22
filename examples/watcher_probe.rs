//! Does anything read the clipboard when nobody pasted?
//!
//! Delayed rendering treats `WM_RENDERFORMAT` as "the target read it". That is only sound if the
//! target is the *only* thing that reads. Clipboard history, cloud clipboard sync, and third-party
//! managers all read the clipboard on their own, and any of them could satisfy the render and make
//! a paste that never landed look successful.
//!
//! This publishes a promise and sends no paste at all. A confirmed read here is a false positive.
//!
//! ```text
//! cargo run --example watcher_probe
//! ```

use std::time::Duration;

fn main() {
    let history = clipboard_history_enabled();
    println!("Windows clipboard history enabled: {history:?}\n");

    println!("publishing a delayed-render promise, sending NO paste...");
    let offer = win_text_inject::Offer::publish("canary text nobody pasted").expect("publish");

    let read = offer.wait_for_read(Duration::from_secs(3));

    println!("read reported within 3s: {read}\n");
    if read {
        println!(
            "FALSE POSITIVE: something read the clipboard without a paste. Treating\n\
             WM_RENDERFORMAT as proof the target received the text is unsound on this\n\
             machine -- a watcher can satisfy the render and the restore then fires early."
        );
        std::process::exit(1);
    } else {
        println!(
            "clean: no reader consumed the promise. WM_RENDERFORMAT can be attributed\n\
             to the paste target on this configuration."
        );
    }
}

fn clipboard_history_enabled() -> Option<u32> {
    // Read-only probe; deliberately does not change the user's setting.
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Clipboard",
            "/v",
            "EnableClipboardHistory",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let token = text.split_whitespace().last()?;
    u32::from_str_radix(token.trim_start_matches("0x"), 16).ok()
}
