//! Release physically-held modifier keys before synthesizing input.
//!
//! In a push-to-talk dictation app a modifier is held *by construction* at the moment injection
//! fires. `SendInput` does not reset keyboard state:
//!
//! > This function does not reset the keyboard's current state. Any keys that are already pressed
//! > when the function is called might interfere with the events that this function generates.
//!
//! So a user holding Right-Alt turns a synthesized Ctrl+V into AltGr+V, which is a different
//! character on many layouts. No dictation tool surveyed does this.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_RWIN,
};

use crate::sendinput::{tagged_keyboard_input, INJECT_TAG};
use crate::Error;

/// Every modifier that can alter the meaning of a synthesized chord.
const MODIFIERS: [VIRTUAL_KEY; 8] = [
    VK_LSHIFT, VK_RSHIFT, VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN,
];

/// High bit of `GetAsyncKeyState` means the key is currently physically down.
const KEY_DOWN_MASK: u16 = 0x8000;

fn is_down(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & KEY_DOWN_MASK) != 0
}

/// Synthesize key-up for every modifier currently held.
///
/// Returns the modifiers that were released. They are deliberately **not** restored afterwards:
/// re-pressing a modifier the user has since physically released leaves it stuck down forever,
/// which is a far worse failure than a lost modifier.
pub fn sanitize() -> Result<Vec<VIRTUAL_KEY>, Error> {
    let held: Vec<VIRTUAL_KEY> = MODIFIERS.into_iter().filter(|vk| is_down(*vk)).collect();
    if held.is_empty() {
        return Ok(held);
    }

    let inputs: Vec<INPUT> = held
        .iter()
        .map(|vk| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: *vk,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: INJECT_TAG,
                },
            },
        })
        .collect();

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(Error::SendInputBlocked);
    }
    Ok(held)
}

/// True if any modifier is currently held. Cheap pre-check for callers that want to log or delay.
pub fn any_held() -> bool {
    MODIFIERS.into_iter().any(is_down)
}

/// Build a tagged key-up event for one virtual key, exposed for chord construction.
pub(crate) fn key_up(vk: VIRTUAL_KEY) -> INPUT {
    tagged_keyboard_input(vk, KEYEVENTF_KEYUP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_set_covers_both_sides_of_every_modifier() {
        assert_eq!(MODIFIERS.len(), 8);
        for pair in [
            (VK_LSHIFT, VK_RSHIFT),
            (VK_LCONTROL, VK_RCONTROL),
            (VK_LMENU, VK_RMENU),
            (VK_LWIN, VK_RWIN),
        ] {
            assert!(MODIFIERS.contains(&pair.0));
            assert!(MODIFIERS.contains(&pair.1));
        }
    }

    #[test]
    fn any_held_agrees_with_per_key_state() {
        // Whatever the live keyboard state is, the aggregate must match the individual checks.
        let individually = MODIFIERS.into_iter().any(is_down);
        assert_eq!(any_held(), individually);
    }
}
