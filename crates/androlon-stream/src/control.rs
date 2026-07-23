//! scrcpy v4.1 control channel: serialize input events into control messages
//! and write them to the second socket the server opens when `control=true`.
//!
//! Wire format per scrcpy's `ControlMessageReader.java` / `control_msg.c`
//! (v4.x). All integers big-endian. Fractional values (pressure, scroll) are
//! fixed-point: u16 `0..=0xffff` maps 0.0..=1.0, i16 maps -1.0..=1.0.
//!
//! The server also *sends* device messages (clipboard, ack) on this socket; a
//! drain thread discards them so the server never blocks on a full buffer.

use crate::error::Result;
use std::io::{Read, Write};
use std::net::TcpStream;

// Control message types (client → device), scrcpy v4.1.
const TYPE_INJECT_KEYCODE: u8 = 0;
const TYPE_INJECT_TEXT: u8 = 1;
const TYPE_INJECT_TOUCH_EVENT: u8 = 2;
const TYPE_INJECT_SCROLL_EVENT: u8 = 3;
const TYPE_BACK_OR_SCREEN_ON: u8 = 4;
const TYPE_START_APP: u8 = 16;

// android.view.KeyEvent / MotionEvent actions.
pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;

// android.view.MotionEvent button masks.
pub const BUTTON_PRIMARY: u32 = 1;
pub const BUTTON_SECONDARY: u32 = 1 << 1;
pub const BUTTON_TERTIARY: u32 = 1 << 2;

/// scrcpy's "fake finger" pointer id: the server injects a touchscreen event
/// (not a mouse event), which every app and game accepts.
pub const POINTER_ID_GENERIC_FINGER: u64 = u64::MAX - 1; // -2

// A handful of android.view.KeyEvent keycodes (the SDL→Android map lives in
// the app crate; these are the protocol-level names it builds on).
pub mod keycodes {
    pub const AKEYCODE_HOME: i32 = 3;
    pub const AKEYCODE_BACK: i32 = 4;
    pub const AKEYCODE_0: i32 = 7; // 0..9 are contiguous
    pub const AKEYCODE_DPAD_UP: i32 = 19;
    pub const AKEYCODE_DPAD_DOWN: i32 = 20;
    pub const AKEYCODE_DPAD_LEFT: i32 = 21;
    pub const AKEYCODE_DPAD_RIGHT: i32 = 22;
    pub const AKEYCODE_A: i32 = 29; // a..z are contiguous
    pub const AKEYCODE_COMMA: i32 = 55;
    pub const AKEYCODE_PERIOD: i32 = 56;
    pub const AKEYCODE_TAB: i32 = 61;
    pub const AKEYCODE_SPACE: i32 = 62;
    pub const AKEYCODE_ENTER: i32 = 66;
    pub const AKEYCODE_DEL: i32 = 67; // backspace
    pub const AKEYCODE_GRAVE: i32 = 68;
    pub const AKEYCODE_MINUS: i32 = 69;
    pub const AKEYCODE_EQUALS: i32 = 70;
    pub const AKEYCODE_LEFT_BRACKET: i32 = 71;
    pub const AKEYCODE_RIGHT_BRACKET: i32 = 72;
    pub const AKEYCODE_BACKSLASH: i32 = 73;
    pub const AKEYCODE_SEMICOLON: i32 = 74;
    pub const AKEYCODE_APOSTROPHE: i32 = 75;
    pub const AKEYCODE_SLASH: i32 = 76;
    pub const AKEYCODE_PAGE_UP: i32 = 92;
    pub const AKEYCODE_PAGE_DOWN: i32 = 93;
    pub const AKEYCODE_ESCAPE: i32 = 111;
    pub const AKEYCODE_FORWARD_DEL: i32 = 112;
    pub const AKEYCODE_MOVE_HOME: i32 = 122;
    pub const AKEYCODE_MOVE_END: i32 = 123;
    pub const AKEYCODE_APP_SWITCH: i32 = 187;
}

// android.view.KeyEvent meta states.
pub mod metastate {
    pub const AMETA_NONE: i32 = 0;
    pub const AMETA_SHIFT_ON: i32 = 0x01;
    pub const AMETA_ALT_ON: i32 = 0x02;
    pub const AMETA_CTRL_ON: i32 = 0x1000;
}

fn fixed_u16(v: f32) -> u16 {
    // scrcpy: round(v * 2^16), clamped to 0xffff.
    let x = (v * 65536.0).round() as i64;
    x.clamp(0, 0xffff) as u16
}

fn fixed_i16(v: f32) -> i16 {
    // scrcpy: round(v * 2^15), clamped to i16 range.
    let x = (v * 32768.0).round() as i64;
    x.clamp(-0x8000, 0x7fff) as i16
}

/// Position block shared by touch and scroll messages: the point in *stream*
/// coordinates plus the stream size it is relative to (the server maps it onto
/// the display and drops it if the size no longer matches, e.g. mid-rotation).
#[derive(Debug, Clone, Copy)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl Position {
    fn put(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.x.to_be_bytes());
        buf.extend_from_slice(&self.y.to_be_bytes());
        buf.extend_from_slice(&self.width.to_be_bytes());
        buf.extend_from_slice(&self.height.to_be_bytes());
    }
}

/// Serialize INJECT_TOUCH_EVENT (32 bytes).
pub fn touch_event(
    action: u8,
    pointer_id: u64,
    pos: Position,
    pressure: f32,
    action_button: u32,
    buttons: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.push(TYPE_INJECT_TOUCH_EVENT);
    buf.push(action);
    buf.extend_from_slice(&pointer_id.to_be_bytes());
    pos.put(&mut buf);
    buf.extend_from_slice(&fixed_u16(pressure).to_be_bytes());
    buf.extend_from_slice(&action_button.to_be_bytes());
    buf.extend_from_slice(&buttons.to_be_bytes());
    buf
}

/// Serialize INJECT_SCROLL_EVENT (21 bytes). h/v are in -1.0..=1.0 "ticks".
pub fn scroll_event(pos: Position, hscroll: f32, vscroll: f32, buttons: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(21);
    buf.push(TYPE_INJECT_SCROLL_EVENT);
    pos.put(&mut buf);
    buf.extend_from_slice(&fixed_i16(hscroll).to_be_bytes());
    buf.extend_from_slice(&fixed_i16(vscroll).to_be_bytes());
    buf.extend_from_slice(&buttons.to_be_bytes());
    buf
}

/// Serialize INJECT_KEYCODE (14 bytes).
pub fn key_event(action: u8, keycode: i32, repeat: i32, metastate: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(14);
    buf.push(TYPE_INJECT_KEYCODE);
    buf.push(action);
    buf.extend_from_slice(&keycode.to_be_bytes());
    buf.extend_from_slice(&repeat.to_be_bytes());
    buf.extend_from_slice(&metastate.to_be_bytes());
    buf
}

/// Serialize INJECT_TEXT (5 bytes + UTF-8 payload).
pub fn text_event(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut buf = Vec::with_capacity(5 + bytes.len());
    buf.push(TYPE_INJECT_TEXT);
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
    buf
}

/// Serialize BACK_OR_SCREEN_ON (2 bytes): BACK if awake, wake if asleep.
pub fn back_or_screen_on(action: u8) -> Vec<u8> {
    vec![TYPE_BACK_OR_SCREEN_ON, action]
}

/// Serialize START_APP (2 bytes + name): launch a package on the display this
/// connection captures (a `new_display` session starts it on that display).
/// The name is a "tiny string": 1-byte length + UTF-8, so ≤255 bytes.
/// Prefix `+` to force-stop the app first; prefix `?` to search by label.
pub fn start_app(name: &str) -> Vec<u8> {
    let bytes = &name.as_bytes()[..name.len().min(255)];
    let mut buf = Vec::with_capacity(2 + bytes.len());
    buf.push(TYPE_START_APP);
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
    buf
}

/// The connected control socket. Writes are small and the socket is nodelay'd,
/// so sending inline from the UI thread is fine. Construction spawns a drain
/// thread that discards device→client messages until the socket closes.
pub struct ControlChannel {
    stream: TcpStream,
}

impl ControlChannel {
    pub fn new(stream: TcpStream) -> Self {
        let _ = stream.set_nodelay(true); // input latency matters more than bytes
        if let Ok(mut rx) = stream.try_clone() {
            std::thread::spawn(move || {
                let mut sink = [0u8; 4096];
                while matches!(rx.read(&mut sink), Ok(n) if n > 0) {}
            });
        }
        ControlChannel { stream }
    }

    pub fn send(&mut self, msg: &[u8]) -> Result<()> {
        self.stream.write_all(msg)?;
        Ok(())
    }

    pub fn send_touch(
        &mut self,
        action: u8,
        pos: Position,
        pressure: f32,
        action_button: u32,
        buttons: u32,
    ) -> Result<()> {
        self.send(&touch_event(
            action,
            POINTER_ID_GENERIC_FINGER,
            pos,
            pressure,
            action_button,
            buttons,
        ))
    }

    pub fn send_scroll(&mut self, pos: Position, hscroll: f32, vscroll: f32) -> Result<()> {
        self.send(&scroll_event(pos, hscroll, vscroll, 0))
    }

    pub fn send_key(&mut self, action: u8, keycode: i32, metastate: i32) -> Result<()> {
        self.send(&key_event(action, keycode, 0, metastate))
    }

    pub fn send_text(&mut self, text: &str) -> Result<()> {
        self.send(&text_event(text))
    }

    pub fn send_start_app(&mut self, name: &str) -> Result<()> {
        self.send(&start_app(name))
    }

    pub fn send_back(&mut self) -> Result<()> {
        let down = back_or_screen_on(ACTION_DOWN);
        let up = back_or_screen_on(ACTION_UP);
        self.send(&down)?;
        self.send(&up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_layout() {
        let pos = Position { x: 100, y: 200, width: 1080, height: 2400 };
        let msg = touch_event(ACTION_DOWN, POINTER_ID_GENERIC_FINGER, pos, 1.0, BUTTON_PRIMARY, BUTTON_PRIMARY);
        assert_eq!(msg.len(), 32);
        assert_eq!(msg[0], TYPE_INJECT_TOUCH_EVENT);
        assert_eq!(msg[1], ACTION_DOWN);
        assert_eq!(&msg[2..10], &(u64::MAX - 1).to_be_bytes());
        assert_eq!(&msg[10..14], &100i32.to_be_bytes());
        assert_eq!(&msg[14..18], &200i32.to_be_bytes());
        assert_eq!(&msg[18..20], &1080u16.to_be_bytes());
        assert_eq!(&msg[20..22], &2400u16.to_be_bytes());
        assert_eq!(&msg[22..24], &0xffffu16.to_be_bytes()); // pressure 1.0
        assert_eq!(&msg[24..28], &1u32.to_be_bytes());
        assert_eq!(&msg[28..32], &1u32.to_be_bytes());
    }

    #[test]
    fn scroll_layout() {
        let pos = Position { x: 5, y: 6, width: 720, height: 1280 };
        let msg = scroll_event(pos, 0.0, -1.0, 0);
        assert_eq!(msg.len(), 21);
        assert_eq!(msg[0], TYPE_INJECT_SCROLL_EVENT);
        assert_eq!(&msg[13..15], &0i16.to_be_bytes());
        assert_eq!(&msg[15..17], &(-0x8000i16).to_be_bytes()); // vscroll -1.0
    }

    #[test]
    fn key_layout() {
        let msg = key_event(ACTION_UP, keycodes::AKEYCODE_ENTER, 0, metastate::AMETA_SHIFT_ON);
        assert_eq!(msg.len(), 14);
        assert_eq!(msg[0], TYPE_INJECT_KEYCODE);
        assert_eq!(msg[1], ACTION_UP);
        assert_eq!(&msg[2..6], &66i32.to_be_bytes());
        assert_eq!(&msg[10..14], &1i32.to_be_bytes());
    }

    #[test]
    fn text_layout() {
        let msg = text_event("hi");
        assert_eq!(msg, vec![TYPE_INJECT_TEXT, 0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn fixed_point_clamps() {
        assert_eq!(fixed_u16(0.0), 0);
        assert_eq!(fixed_u16(1.0), 0xffff);
        assert_eq!(fixed_u16(2.0), 0xffff);
        assert_eq!(fixed_i16(1.0), 0x7fff);
        assert_eq!(fixed_i16(-1.0), -0x8000);
        assert_eq!(fixed_i16(0.5), 0x4000);
    }
}
