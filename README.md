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

## 4. The clipboard restore races the target's read

This is the defect behind Handy issue #502 (open since 2025-12-30, 52 comments, no fix; the shipped mitigation is a delay slider, and users report it still failing at 400 ms).

A target reads the clipboard *asynchronously*, whenever its message pump gets to the paste. An injector that restores the previous clipboard on a fixed timer restores before a busy target has read, and the target then reads the restored — old — content. Every fixed delay is a guess, and tuning it upward only moves the threshold.

**Delayed rendering removes the guess.** Instead of publishing the text, publish a promise: `SetClipboardData(CF_UNICODETEXT, NULL)` with a hidden owner window. Windows sends `WM_RENDERFORMAT` to that window at the instant a consumer actually asks for the data. That message *is* the "target has read it" signal, so the restore is sequenced strictly after the read instead of racing it.

```
cargo run --example repro_502
```

Measured against a real window with a controllable message-pump lag, 120 ms restore delay:

| algorithm | 10 ms | 60 ms | 150 ms | 400 ms | clipboard restored |
|---|---|---|---|---|---|
| unconditional restore (what everyone ships) | ok | ok | **wrong text** | **wrong text** | yes |
| sequence-number gated restore | ok | ok | **wrong text** | **wrong text** | yes |
| no restore | ok | ok | ok | ok | **no — user's clipboard destroyed** |
| **delayed render** | ok | ok | ok | ok | **yes** |

Note the second row: gating the restore on the clipboard sequence number does **not** fix this. The sequence number is still ours, because nobody else wrote — we clobber ourselves. The sequence gate solves a different problem (a third party taking the clipboard mid-paste) and is kept for that reason.

### One render is not enough

The synthetic harness above reads the clipboard exactly once. Real applications do not.

Tested against real Chrome, restoring after the first `WM_RENDERFORMAT` **still delivered the old clipboard**. Chromium touches the clipboard more than once per paste — an early probe, then the read that actually populates the field — so a restore fired after the first render lands between the two, which is the original bug wearing a different hat.

The fix is to wait for renders to go *quiet*, not for the first one: `Offer::wait_for_reads_to_settle`. This is not a race-the-target delay — the clock starts from an observed read, so it does not need per-machine tuning.

### Verified against real applications

`cargo run --example real_app_test -- --attach <exe>` injects through the full public API and reads the result back by making the app copy its own field to the clipboard — the only verification that works uniformly across Win32, Chromium and Electron. `WM_GETTEXT` and UI Automation both fail on at least one of those.

| Application | Chord | Delivered | Exact | Clipboard kept | Read confirmed |
|---|---|---|---|---|---|
| Chrome | Ctrl+V | yes | yes | yes | yes |
| VS Code | Ctrl+V | yes | yes | yes | yes |
| Notepad | Ctrl+V | yes | yes | yes | yes |
| Windows Terminal | Ctrl+Shift+V | yes | n/a | yes | yes |

Every chord in the table is verified this way, not taken from documentation. VS Code was originally listed as needing Shift+Insert on the strength of a vendor support page; testing showed Shift+Insert does nothing in the editor — the paste never fires at all. That advice appears to describe the integrated terminal.

### Clipboard managers

A manager that reads the clipboard on every change interacts with delayed rendering in a way worth stating plainly. Tested with `examples/fake_clipboard_manager.rs` in both modes.

**A well-behaved manager** — one that checks the opt-out formats — does not read our promise at all, so nothing changes. Windows' own clipboard history behaves this way: with history enabled, `examples/watcher_probe.rs` reports no reader consuming the promise. The privacy formats protect the correctness signal as a side effect.

**A naive manager** that ignores the opt-out formats does two things:

1. **It captures the transcript.** The opt-out formats are a cooperative protocol, not enforcement. This is a real and unavoidable privacy limitation — do not claim otherwise.
2. **It consumes the promise before the paste.** Once *anything* renders it, the clipboard holds real data and Windows sends no further `WM_RENDERFORMAT`, so the target's read becomes unobservable — not delayed, gone.

Case 2 is why `mark_paste_sent` exists: reads before the paste are ignored, so a manager cannot forge a confirmation. When the promise was consumed early, `inject` detects it via `consumed_before_paste` and falls back to the timer-based restore, reporting `read_confirmed: false` honestly rather than pretending. Verified: with a naive manager running, delivery and clipboard restore still succeed, and the outcome correctly reports that the read could not be confirmed.

If no read is observed at all, the paste most likely never landed, so the transcript is deliberately left on the clipboard rather than discarded.

## Status

Early. Clipboard privacy formats, modifier sanitization, integrity checks, and delayed rendering are implemented and tested. Per-app paste-chord selection is a small static table so far.

Known gaps: `Strategy::UnicodeType` is not yet exercised against a real window; there is no fallback chain when `SendInput` is blocked mid-paste; no CI.

## License

MIT OR Apache-2.0
