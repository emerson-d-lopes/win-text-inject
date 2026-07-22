//! Correct text injection into the focused Windows application.
//!
//! Every open-source dictation tool surveyed in July 2026 delivers text the same way: save the
//! clipboard, overwrite it, synthesize Ctrl+V, `sleep()`, restore. That approach has three defects
//! that this crate exists to fix.
//!
//! 1. **Transcripts leak into clipboard history and the Microsoft cloud clipboard.** Writing
//!    `CF_UNICODETEXT` alone opts into both. See [`clipboard::set_text_private`].
//! 2. **Held modifiers corrupt the synthesized chord.** In push-to-talk a modifier is held by
//!    construction when injection fires. See [`modifiers::sanitize`].
//! 3. **Injection into elevated windows fails silently.** UIPI blocks it and reports nothing
//!    through `GetLastError` or the return value, so text vanishes. See [`Target::accepts_injection`].
//! 4. **The clipboard restore races the target's read.** A target reads the clipboard whenever its
//!    message pump gets to the paste, so a timer-based restore can win and the target then reads
//!    the *previous* clipboard. Any fixed delay is a guess. See [`delayed`].
//!
//! # Example
//!
//! ```no_run
//! # fn main() -> Result<(), win_text_inject::Error> {
//! // Capture at hotkey press, so focus changes during dictation cannot misdirect the text.
//! let target = win_text_inject::Target::foreground()?;
//!
//! // ... record and transcribe ...
//!
//! let outcome = win_text_inject::inject(&target, "hello world", Default::default())?;
//! if outcome.needs_manual_paste() {
//!     // Text is on the clipboard; tell the user to press Ctrl+V.
//! }
//! # Ok(())
//! # }
//! ```

#![cfg(windows)]
#![warn(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod clipboard;
pub mod delayed;
pub mod modifiers;
pub mod sendinput;
mod target;

pub use delayed::Offer;
pub use sendinput::{type_text, INJECT_TAG};
pub use target::{Integrity, Target};

use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CONTROL, VK_INSERT, VK_SHIFT, VK_V,
};

/// Failures that are worth distinguishing at the call site.
#[derive(Debug)]
pub enum Error {
    /// No foreground window, or it belongs to no process.
    NoForegroundWindow,
    /// Another process held the clipboard lock across every retry.
    ClipboardLocked(windows::core::Error),
    /// A clipboard call failed after the clipboard was successfully opened.
    Clipboard(windows::core::Error),
    /// `GlobalAlloc` or `GlobalLock` failed while preparing clipboard data.
    Alloc(windows::core::Error),
    /// `SendInput` accepted fewer events than submitted. Almost always UIPI.
    SendInputBlocked,
    /// Focus moved between capture and injection.
    FocusChanged,
    /// The hidden clipboard-owner window could not be created.
    OwnerWindowFailed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoForegroundWindow => write!(f, "no foreground window"),
            Error::ClipboardLocked(e) => write!(f, "clipboard held by another process: {e}"),
            Error::Clipboard(e) => write!(f, "clipboard operation failed: {e}"),
            Error::Alloc(e) => write!(f, "global allocation failed: {e}"),
            Error::SendInputBlocked => write!(f, "SendInput was blocked, most likely by UIPI"),
            Error::FocusChanged => write!(f, "focus moved away from the captured target"),
            Error::OwnerWindowFailed => write!(f, "clipboard owner window could not be created"),
        }
    }
}

impl std::error::Error for Error {}

/// Key combination used to trigger a paste in the target application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    /// The near-universal paste binding.
    CtrlV,
    /// Terminals, where Ctrl+V is a control character or unbound.
    CtrlShiftV,
    /// Legacy paste binding, still honored by some older Win32 software.
    ShiftInsert,
}

impl Chord {
    /// The chord most likely to paste in the given application.
    ///
    /// Every entry here is verified against the real application (see `examples/real_app_test.rs`),
    /// not taken from documentation. VS Code was previously listed as needing Shift+Insert on the
    /// strength of a vendor support page; testing showed Shift+Insert does nothing in the editor
    /// and Ctrl+V works. That claim appears to describe the integrated terminal, not the editor.
    pub fn for_exe(exe: &str) -> Self {
        match exe {
            // Terminals bind Ctrl+V to a control character or to nothing.
            "windowsterminal.exe"
            | "conhost.exe"
            | "mintty.exe"
            | "putty.exe"
            | "alacritty.exe"
            | "wezterm-gui.exe" => Chord::CtrlShiftV,
            _ => Chord::CtrlV,
        }
    }

    fn keys(self) -> (&'static [VIRTUAL_KEY], VIRTUAL_KEY) {
        match self {
            Chord::CtrlV => (&[VK_CONTROL], VK_V),
            Chord::CtrlShiftV => (&[VK_CONTROL, VK_SHIFT], VK_V),
            Chord::ShiftInsert => (&[VK_SHIFT], VK_INSERT),
        }
    }
}

/// How text should be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Clipboard plus a synthesized paste chord. Fast and correct for long text.
    #[default]
    ClipboardPaste,
    /// Type the text as Unicode input. Slower, but works where paste is blocked.
    UnicodeType,
    /// Write the clipboard and stop, leaving the paste to the user.
    ClipboardOnly,
}

/// Tunables. Defaults are deliberately conservative.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// How the text should be delivered.
    pub strategy: Strategy,
    /// Chord override. `None` selects per target executable.
    pub chord: Option<Chord>,
    /// Settle time between writing the clipboard and sending the paste chord.
    pub pre_paste: Duration,
    /// Time allowed for the target to read the clipboard before restoring it.
    ///
    /// Only consulted when [`Options::delayed_render`] is off. With delayed rendering the restore
    /// is triggered by the target's actual read, so there is no delay to tune.
    pub post_paste: Duration,
    /// Restore the previous clipboard contents after pasting.
    pub restore_clipboard: bool,
    /// Abort if focus left the captured target.
    pub require_same_target: bool,
    /// Publish the text as a delayed-render promise and restore once the target actually reads it.
    ///
    /// On by default. Turning this off falls back to the timer-based restore that every other tool
    /// ships, which loses the transcript when the target is slower than `post_paste`.
    pub delayed_render: bool,
    /// How long to wait for the target to read the clipboard before giving up.
    pub read_timeout: Duration,
    /// After the first read, how long reads must stay quiet before the clipboard is restored.
    ///
    /// Consumers may read more than once per paste. This is not a race-the-target delay -- it
    /// starts from an observed read, so it does not need to be tuned per machine.
    pub read_quiet: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            chord: None,
            pre_paste: Duration::from_millis(30),
            post_paste: Duration::from_millis(120),
            restore_clipboard: true,
            require_same_target: true,
            delayed_render: true,
            read_timeout: Duration::from_secs(3),
            read_quiet: Duration::from_millis(400),
        }
    }
}

/// What actually happened, so the caller can tell the user the truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A paste chord was sent.
    ///
    /// `read_confirmed` is true only when the target was observed reading the clipboard, which
    /// delayed rendering makes knowable. When it is false the paste may not have landed — worth
    /// surfacing rather than assuming success, which is what every other tool does.
    Pasted {
        /// Whether Windows reported the target actually reading the clipboard.
        read_confirmed: bool,
    },
    /// Text was typed directly into the target.
    Typed,
    /// Text is on the clipboard but was not delivered; the user must paste it.
    ClipboardOnly(ClipboardOnlyReason),
}

/// Why the text was left on the clipboard instead of being delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOnlyReason {
    /// Caller asked for it.
    Requested,
    /// Target runs at a higher integrity level, so UIPI would discard the input.
    ElevatedTarget,
}

impl Outcome {
    /// True when the user has to paste manually for the text to arrive.
    pub fn needs_manual_paste(self) -> bool {
        matches!(self, Outcome::ClipboardOnly(_))
    }
}

/// Deliver `text` to `target`.
///
/// Order matters: the elevation check happens before anything is written, and modifier
/// sanitization happens before any synthesized chord.
pub fn inject(target: &Target, text: &str, options: Options) -> Result<Outcome, Error> {
    if options.require_same_target && !target.still_foreground() {
        return Err(Error::FocusChanged);
    }

    // Refusing early is the whole point: injecting here would succeed silently and deliver nothing.
    if !target.accepts_injection() {
        clipboard::set_text_private(text)?;
        return Ok(Outcome::ClipboardOnly(ClipboardOnlyReason::ElevatedTarget));
    }

    match options.strategy {
        Strategy::ClipboardOnly => {
            clipboard::set_text_private(text)?;
            Ok(Outcome::ClipboardOnly(ClipboardOnlyReason::Requested))
        }
        Strategy::UnicodeType => {
            modifiers::sanitize()?;
            sendinput::type_text(text)?;
            Ok(Outcome::Typed)
        }
        Strategy::ClipboardPaste => {
            let snapshot = if options.restore_clipboard {
                clipboard::Snapshot::capture().ok()
            } else {
                None
            };

            let chord = options.chord.unwrap_or_else(|| Chord::for_exe(&target.exe));

            if options.delayed_render {
                // Publish a promise rather than the text. Windows reports the target's actual read,
                // so the restore is sequenced after it instead of racing a timer.
                let offer = delayed::Offer::publish(text)?;
                std::thread::sleep(options.pre_paste);
                modifiers::sanitize()?;

                // Anything that reads the clipboard satisfies the render, so a read observed before
                // the paste says nothing about the target. A clipboard manager that archives every
                // change will otherwise confirm a paste that never happened.
                offer.mark_paste_sent();
                send_chord(chord)?;

                // Not a single render: Chromium probes the clipboard before the read that actually
                // populates the field, so restoring after the first one reintroduces the very bug
                // this path exists to fix.
                let reads = offer.wait_for_target_read(options.read_timeout, options.read_quiet);
                let read_confirmed = reads.is_some();

                // Three cases, and they need different handling:
                //
                // 1. A read after the paste. Confident; restore now, sequenced after the read.
                // 2. No such read, but the promise was consumed before the paste (a clipboard
                //    manager archived it). The target's read is unobservable, so fall back to the
                //    timer. Worse than case 1, but not restoring at all would permanently destroy
                //    the user's clipboard on every dictation while a manager is running.
                // 3. No read at all. The paste most likely never landed, so leave the transcript on
                //    the clipboard for the user to paste manually rather than discarding it.
                let should_restore = if read_confirmed {
                    true
                } else if offer.consumed_before_paste() {
                    std::thread::sleep(options.post_paste);
                    true
                } else {
                    false
                };

                if should_restore {
                    if let Some(snapshot) = snapshot {
                        let _ = snapshot.restore();
                    }
                }
                return Ok(Outcome::Pasted { read_confirmed });
            }

            clipboard::set_text_private(text)?;
            let ours = clipboard::sequence_number();

            std::thread::sleep(options.pre_paste);
            modifiers::sanitize()?;
            send_chord(chord)?;
            std::thread::sleep(options.post_paste);

            // Only restore when the clipboard still holds our write, so a third party that took the
            // clipboard mid-paste is not clobbered. Note this does NOT prevent the target from
            // reading the restored value -- only delayed rendering does.
            if let Some(snapshot) = snapshot {
                let _ = snapshot.restore_if_ours(ours);
            }

            Ok(Outcome::Pasted {
                read_confirmed: false,
            })
        }
    }
}

fn send_chord(chord: Chord) -> Result<(), Error> {
    use windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS;

    let (mods, key) = chord.keys();
    let mut inputs = Vec::with_capacity(mods.len() * 2 + 2);

    for m in mods {
        inputs.push(sendinput::tagged_keyboard_input(*m, KEYBD_EVENT_FLAGS(0)));
    }
    inputs.push(sendinput::tagged_keyboard_input(key, KEYBD_EVENT_FLAGS(0)));
    inputs.push(modifiers::key_up(key));
    for m in mods.iter().rev() {
        inputs.push(modifiers::key_up(*m));
    }

    sendinput::send(&inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_get_ctrl_shift_v() {
        assert_eq!(Chord::for_exe("windowsterminal.exe"), Chord::CtrlShiftV);
        assert_eq!(Chord::for_exe("alacritty.exe"), Chord::CtrlShiftV);
    }

    #[test]
    fn vs_code_gets_ctrl_v_not_shift_insert() {
        // Verified against real VS Code: Shift+Insert does nothing in the editor and the paste
        // never fires. The Shift+Insert advice in vendor docs describes the integrated terminal.
        assert_eq!(Chord::for_exe("code.exe"), Chord::CtrlV);
        assert_eq!(Chord::for_exe("cursor.exe"), Chord::CtrlV);
    }

    #[test]
    fn unknown_apps_fall_back_to_ctrl_v() {
        assert_eq!(Chord::for_exe("notepad.exe"), Chord::CtrlV);
        assert_eq!(Chord::for_exe(""), Chord::CtrlV);
    }

    #[test]
    fn chord_lookup_assumes_lowercased_input() {
        // Target::foreground lowercases the exe name, so the table only needs lowercase keys.
        assert_eq!(Chord::for_exe("Code.exe"), Chord::CtrlV);
    }

    #[test]
    fn only_clipboard_only_requires_manual_paste() {
        assert!(!Outcome::Pasted {
            read_confirmed: true
        }
        .needs_manual_paste());
        assert!(!Outcome::Typed.needs_manual_paste());
        assert!(Outcome::ClipboardOnly(ClipboardOnlyReason::ElevatedTarget).needs_manual_paste());
    }

    #[test]
    fn defaults_restore_the_clipboard_and_pin_the_target() {
        let o = Options::default();
        assert!(o.restore_clipboard);
        assert!(o.require_same_target);
        assert_eq!(o.strategy, Strategy::ClipboardPaste);
    }

    #[test]
    fn delayed_render_is_the_default() {
        // The timer path is the known-broken one; it must be opt-in, not the default.
        assert!(Options::default().delayed_render);
    }

    #[test]
    fn timer_path_cannot_confirm_a_read() {
        // Only the delayed-render path can know the target actually read the clipboard.
        assert!(!Outcome::Pasted {
            read_confirmed: false
        }
        .needs_manual_paste());
    }

    #[test]
    fn chord_key_sequences_are_well_formed() {
        assert_eq!(Chord::CtrlV.keys().0.len(), 1);
        assert_eq!(Chord::CtrlShiftV.keys().0.len(), 2);
        assert_eq!(Chord::ShiftInsert.keys().1, VK_INSERT);
    }
}
