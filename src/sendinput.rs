//! Synthesized keyboard input, tagged so the sender's own low-level hook can ignore it.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY,
};

use crate::Error;

/// Marker written to `dwExtraInfo` on every event this crate synthesizes.
///
/// A dictation app installs a `WH_KEYBOARD_LL` hook to detect its hotkey. Without a tag, the
/// synthesized paste chord re-enters that hook and can retrigger the hotkey. Callers should skip
/// events whose `dwExtraInfo` equals this value, in addition to checking `LLKHF_INJECTED`.
pub const INJECT_TAG: usize = 0x57_54_49_4A; // "WTIJ"

pub(crate) fn tagged_keyboard_input(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn unicode_unit(unit: u16, key_up: bool) -> INPUT {
    let flags = if key_up {
        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
    } else {
        KEYEVENTF_UNICODE
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                // Must be zero when KEYEVENTF_UNICODE is set.
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

/// Build the full event sequence for `text` as UTF-16 code units.
///
/// Characters outside the BMP become a surrogate pair, and each half is sent as its own
/// down/up pair — four events for one glyph. Receivers reassemble them; there is no way to send a
/// non-BMP character as a single event.
pub(crate) fn unicode_inputs(text: &str) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        inputs.push(unicode_unit(unit, false));
        inputs.push(unicode_unit(unit, true));
    }
    inputs
}

/// Send events in chunks so a long transcript does not occupy the input queue in one burst.
const CHUNK: usize = 256;

pub(crate) fn send(inputs: &[INPUT]) -> Result<(), Error> {
    if inputs.is_empty() {
        return Ok(());
    }
    for chunk in inputs.chunks(CHUNK) {
        let sent = unsafe { SendInput(chunk, std::mem::size_of::<INPUT>() as i32) };
        // A short count here is almost always UIPI silently refusing the injection.
        if sent as usize != chunk.len() {
            return Err(Error::SendInputBlocked);
        }
    }
    Ok(())
}

/// Type `text` directly as Unicode input, bypassing the clipboard entirely.
///
/// Layout-independent, and the only option for targets that block programmatic paste (password
/// managers, some VDI clients). Slow for long text: throughput is bounded by the receiving
/// application's message pump, not by this function.
pub fn type_text(text: &str) -> Result<(), Error> {
    send(&unicode_inputs(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scans(text: &str) -> Vec<u16> {
        unicode_inputs(text)
            .iter()
            .map(|i| unsafe { i.Anonymous.ki.wScan })
            .collect()
    }

    #[test]
    fn each_code_unit_produces_a_down_and_an_up() {
        assert_eq!(unicode_inputs("abc").len(), 6);
    }

    #[test]
    fn non_bmp_characters_produce_four_events() {
        // One emoji is a surrogate pair, so two code units, so four events.
        assert_eq!(unicode_inputs("\u{1F600}").len(), 4);
        assert_eq!(scans("\u{1F600}"), vec![0xD83D, 0xD83D, 0xDE00, 0xDE00]);
    }

    #[test]
    fn unicode_events_must_not_carry_a_virtual_key() {
        for input in unicode_inputs("hi\u{1F600}") {
            assert_eq!(unsafe { input.Anonymous.ki.wVk }, VIRTUAL_KEY(0));
        }
    }

    #[test]
    fn every_event_is_tagged_for_hook_filtering() {
        for input in unicode_inputs("hi") {
            assert_eq!(unsafe { input.Anonymous.ki.dwExtraInfo }, INJECT_TAG);
        }
    }

    #[test]
    fn empty_text_produces_no_events() {
        assert!(unicode_inputs("").is_empty());
        assert!(send(&[]).is_ok());
    }

    #[test]
    fn alternating_events_are_down_then_up() {
        let inputs = unicode_inputs("ab");
        let ups: Vec<bool> = inputs
            .iter()
            .map(|i| unsafe { i.Anonymous.ki.dwFlags }.contains(KEYEVENTF_KEYUP))
            .collect();
        assert_eq!(ups, vec![false, true, false, true]);
    }
}
