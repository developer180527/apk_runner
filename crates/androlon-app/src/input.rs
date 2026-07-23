//! SDL → Android input translation for live app-surface windows.
//!
//! Mouse becomes a fake finger (touch), the wheel becomes scroll ticks, and
//! keys become Android keycodes with a shift/alt/ctrl metastate — the server's
//! IME turns shift+a into 'A', so no separate text path is needed. A few host
//! keys map to Android navigation since the surface has no chrome for them:
//! Escape → BACK, F1 → HOME, F2 → APP_SWITCH.

use androlon_stream::control::{keycodes::*, metastate::*};
use sdl3::keyboard::{Keycode, Mod};

/// Map a window-space mouse position onto the stream's coordinate space.
/// The blit stretches the frame to fill the window, so this is a pure scale.
pub fn window_to_stream(
    (wx, wy): (f32, f32),
    (win_w, win_h): (u32, u32),
    (stream_w, stream_h): (u32, u32),
) -> (i32, i32) {
    let sx = wx * stream_w as f32 / win_w.max(1) as f32;
    let sy = wy * stream_h as f32 / win_h.max(1) as f32;
    (
        (sx as i32).clamp(0, stream_w.saturating_sub(1) as i32),
        (sy as i32).clamp(0, stream_h.saturating_sub(1) as i32),
    )
}

/// SDL keymod → Android metastate.
pub fn meta_of(m: Mod) -> i32 {
    let mut meta = AMETA_NONE;
    if m.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD) {
        meta |= AMETA_SHIFT_ON;
    }
    if m.intersects(Mod::LALTMOD | Mod::RALTMOD) {
        meta |= AMETA_ALT_ON;
    }
    if m.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD) {
        meta |= AMETA_CTRL_ON;
    }
    meta
}

/// SDL keycode → Android keycode. `None` = no sensible mapping; drop the key.
pub fn android_keycode(key: Keycode) -> Option<i32> {
    use Keycode as K;
    // Letters and digits are contiguous in both keycode spaces.
    let raw = key as u32 as i32; // SDLK_* values; printable keys are ASCII
    if ('a' as i32..='z' as i32).contains(&raw) {
        return Some(AKEYCODE_A + (raw - 'a' as i32));
    }
    if ('0' as i32..='9' as i32).contains(&raw) {
        return Some(AKEYCODE_0 + (raw - '0' as i32));
    }
    Some(match key {
        K::Return => AKEYCODE_ENTER,
        K::Backspace => AKEYCODE_DEL,
        K::Delete => AKEYCODE_FORWARD_DEL,
        K::Tab => AKEYCODE_TAB,
        K::Space => AKEYCODE_SPACE,
        K::Up => AKEYCODE_DPAD_UP,
        K::Down => AKEYCODE_DPAD_DOWN,
        K::Left => AKEYCODE_DPAD_LEFT,
        K::Right => AKEYCODE_DPAD_RIGHT,
        K::Comma => AKEYCODE_COMMA,
        K::Period => AKEYCODE_PERIOD,
        K::Minus => AKEYCODE_MINUS,
        K::Equals => AKEYCODE_EQUALS,
        K::LeftBracket => AKEYCODE_LEFT_BRACKET,
        K::RightBracket => AKEYCODE_RIGHT_BRACKET,
        K::Backslash => AKEYCODE_BACKSLASH,
        K::Semicolon => AKEYCODE_SEMICOLON,
        K::Apostrophe => AKEYCODE_APOSTROPHE,
        K::Grave => AKEYCODE_GRAVE,
        K::Slash => AKEYCODE_SLASH,
        K::Home => AKEYCODE_MOVE_HOME,
        K::End => AKEYCODE_MOVE_END,
        K::PageUp => AKEYCODE_PAGE_UP,
        K::PageDown => AKEYCODE_PAGE_DOWN,
        // Android navigation on host keys (the surface has no nav chrome).
        K::Escape => AKEYCODE_BACK,
        K::F1 => AKEYCODE_HOME,
        K::F2 => AKEYCODE_APP_SWITCH,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        assert_eq!(android_keycode(Keycode::A), Some(AKEYCODE_A));
        assert_eq!(android_keycode(Keycode::Z), Some(AKEYCODE_A + 25));
        assert_eq!(android_keycode(Keycode::_0), Some(AKEYCODE_0));
        assert_eq!(android_keycode(Keycode::_9), Some(AKEYCODE_0 + 9));
    }

    #[test]
    fn nav_keys() {
        assert_eq!(android_keycode(Keycode::Escape), Some(AKEYCODE_BACK));
        assert_eq!(android_keycode(Keycode::Return), Some(AKEYCODE_ENTER));
        assert_eq!(android_keycode(Keycode::F5), None);
    }

    #[test]
    fn coordinate_scaling() {
        // 800x450 window showing a 1600x900 stream: scale 2x.
        assert_eq!(window_to_stream((400.0, 225.0), (800, 450), (1600, 900)), (800, 450));
        // Clamped to the stream bounds.
        assert_eq!(window_to_stream((805.0, -3.0), (800, 450), (1600, 900)), (1599, 0));
    }
}
