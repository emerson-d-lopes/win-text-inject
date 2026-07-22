# win-text-inject

Correct text injection into the focused Windows application, for dictation and text-expansion tools.

Every open-source dictation tool surveyed in July 2026 delivers text the same way: save the clipboard, overwrite it, synthesize Ctrl+V, `sleep()`, restore. That approach has three defects. This crate fixes them.

## 1. Transcripts leak into clipboard history and the Microsoft cloud clipboard

Writing `CF_UNICODETEXT` alone opts into both. Chrome's Incognito mode avoids this by registering four additional formats; almost nothing else does. Wispr Flow's own documentation admits *"dictated text is not concealed and may appear in Windows clipboard managers."*

`set_text_private` attaches all four opt-outs, honored by Windows clipboard history, the cloud clipboard, and cooperating third-party managers:

```
ExcludeClipboardContentFromMonitorProcessing
CanIncludeInClipboardHistory        = DWORD 0
CanUploadToCloudClipboard           = DWORD 0
Clipboard Viewer Ignore
```

Verify it yourself:

```
cargo run --example clipboard_privacy
```

## 2. Held modifiers corrupt the synthesized chord

In push-to-talk dictation a modifier is held *by construction* at the moment injection fires. Per MSDN on `SendInput`:

> This function does not reset the keyboard's current state. Any keys that are already pressed when the function is called might interfere with the events that this function generates.

A user holding Right-Alt turns a synthesized Ctrl+V into AltGr+V, which is a different character on many layouts. `modifiers::sanitize` releases every held modifier first, and deliberately does not restore them — re-pressing a modifier the user has since released leaves it stuck down forever.

## 3. Injection into elevated windows fails silently

Per MSDN on `SendInput`:

> This function fails when it is blocked by UIPI. Note that neither `GetLastError` nor the return value will indicate the failure was caused by UIPI blocking.

So the text simply vanishes. `Target::accepts_injection` compares integrity levels in microseconds and lets the caller degrade honestly — leave the text on the clipboard and say so — instead of losing it.

## Usage

```rust
use win_text_inject::{inject, Options, Target};

// Capture at hotkey PRESS, not at injection time. Between press and release the user may have
// changed focus, and injecting into whatever is foreground later is how text lands in the wrong app.
let target = Target::foreground()?;

// ... record and transcribe ...

let outcome = inject(&target, &transcript, Options::default())?;
if outcome.needs_manual_paste() {
    // Text is on the clipboard; prompt the user to press Ctrl+V.
}
# Ok::<(), win_text_inject::Error>(())
```

`Chord::for_exe` picks the paste chord per target: Ctrl+Shift+V for terminals, Shift+Insert for VS Code / Cursor / Windsurf, Ctrl+V otherwise.

### Interaction with a low-level keyboard hook

Every event this crate synthesizes carries `INJECT_TAG` in `dwExtraInfo`. If your app installs a `WH_KEYBOARD_LL` hook to detect its hotkey, skip events whose `dwExtraInfo` equals `INJECT_TAG` in addition to checking `LLKHF_INJECTED`, or the synthesized paste chord will re-enter your own hook.

## What this crate does not do

**It does not use UI Automation.** UIA cannot insert text — per Microsoft, TextPattern *"does not provide a means to insert or modify text"* and is *"a read-only solution"*, while `ValuePattern::SetValue` replaces the entire control value and ignores the caret. Touching UIA also causes Chromium to enable its accessibility tree process-wide, imposing a permanent performance cost on every Chrome and Electron process on the machine.

**It does not implement a TSF text service.** Text Services Framework is the API Microsoft actually recommends for dictation, and it is how Win+H and Voice Access inject so cleanly. But TSF insertion requires being a TIP loaded in-process inside every target application — an in-proc COM DLL, registered, built per architecture, signed. That is a separate project.

**It does not eliminate the clipboard race, only the data loss from it.** The restore is gated on the clipboard sequence number: if a third party took the clipboard between our write and the restore, the restore is skipped rather than clobbering their content. Tools that restore unconditionally are the ones that paste your *previous* clipboard.

## Status

Early. The clipboard, modifier, and integrity paths are implemented and tested; per-app profiles are a small static table so far.

## License

MIT OR Apache-2.0
