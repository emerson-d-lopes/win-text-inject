# Changelog

## 0.1.1

Fixes an intermittent failure when publishing a delayed-render promise.

`SetClipboardData` returns NULL on success for a delayed-render write, which
windows-rs surfaces as `Err`. The previous guard treated any non-zero last-error
code as a real failure, but Win32 does not reset the thread's last-error on
success, so a stale code from an unrelated earlier call read as a failure that
never happened. Observed in real use as `ERROR_NOT_FOUND` (0x80070490) on roughly
one paste in four.

Success is now established from observable clipboard state (we own the clipboard
and `CF_UNICODETEXT` is advertised) rather than from the return value.

## 0.1.0 — unreleased

First release.

- `clipboard::set_text_private` attaches the four opt-out formats so text stays out of Windows
  clipboard history, the cloud clipboard, and cooperating third-party managers.
- `modifiers::sanitize` releases physically-held modifiers before any synthesized chord, so a held
  Right-Alt cannot turn Ctrl+V into AltGr+V.
- `Target::accepts_injection` compares integrity levels before injecting, turning UIPI's silent
  failure into a reportable outcome.
- `delayed::Offer` publishes the text as a delayed-render promise and restores the previous
  clipboard only once the target has actually read it, rather than after a fixed delay.
- `inject` selects the paste chord per target executable, from a table verified against running
  applications rather than vendor documentation.

Verified end to end against Chrome, VS Code, Notepad, and Windows Terminal, with and without a
clipboard manager running.
