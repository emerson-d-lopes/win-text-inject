# Changelog

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
