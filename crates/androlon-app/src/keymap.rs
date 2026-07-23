//! Per-app keyboard → touch bindings, so keyboard-hostile games become
//! playable: keys press on-screen buttons, WASD drives a virtual joystick.
//!
//! Profiles live at `~/.androlon/keymaps/<package>.conf`, one binding per
//! line, coordinates normalized 0..1 (resolution-independent):
//!
//! ```text
//! # comment
//! joystick w a s d  0.20 0.75 0.12    # up left down right  cx cy radius
//! tap space         0.85 0.80         # key  x y
//! tap e             0.92 0.60
//! ```
//!
//! Each `tap` binding holds its own pointer id, so chords work (hold "fire"
//! while pressing "jump" = two fingers). The joystick is one persistent
//! finger at `center + direction * radius`, sliding as keys combine.
//! Mapped keys never reach Android as keyboard input; unmapped keys do.

use sdl3::keyboard::Keycode;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    /// Finger down at (x, y) while the key is held. `pointer` is unique.
    Tap { x: f32, y: f32, pointer: u64 },
    /// One of the four joystick direction keys: dx/dy in -1..1.
    Joy { dx: f32, dy: f32 },
}

#[derive(Debug, Clone, Copy)]
pub struct JoystickCfg {
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    pub pointer: u64,
}

#[derive(Debug, Default)]
pub struct Keymap {
    bindings: HashMap<Keycode, Action>,
    pub joystick: Option<JoystickCfg>,
}

impl Keymap {
    /// Load `~/.androlon/keymaps/<package>.conf`, or None if absent/empty.
    pub fn load(package: &str) -> Option<Keymap> {
        let path = std::env::var("HOME")
            .map(PathBuf::from)
            .ok()?
            .join(".androlon/keymaps")
            .join(format!("{package}.conf"));
        let text = std::fs::read_to_string(path).ok()?;
        let map = Self::parse(&text);
        (!map.bindings.is_empty()).then_some(map)
    }

    pub fn parse(text: &str) -> Keymap {
        let mut map = Keymap::default();
        // Pointer ids: joystick gets 0; taps count up from 1. (Distinct from
        // the mouse's fake-finger id, which is u64::MAX-1.)
        let mut next_pointer: u64 = 1;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let tok: Vec<&str> = line.split_whitespace().collect();
            match tok.as_slice() {
                ["tap", key, x, y] => {
                    let (Some(key), Ok(x), Ok(y)) = (keycode(key), x.parse(), y.parse()) else {
                        continue;
                    };
                    map.bindings.insert(key, Action::Tap { x, y, pointer: next_pointer });
                    next_pointer += 1;
                }
                ["joystick", up, left, down, right, cx, cy, r] => {
                    let keys = [keycode(up), keycode(left), keycode(down), keycode(right)];
                    let (Ok(cx), Ok(cy), Ok(r)) = (cx.parse(), cy.parse(), r.parse()) else {
                        continue;
                    };
                    let dirs = [(0.0, -1.0), (-1.0, 0.0), (0.0, 1.0), (1.0, 0.0)];
                    for (key, (dx, dy)) in keys.into_iter().zip(dirs) {
                        if let Some(key) = key {
                            map.bindings.insert(key, Action::Joy { dx, dy });
                        }
                    }
                    map.joystick = Some(JoystickCfg { cx, cy, radius: r, pointer: 0 });
                }
                _ => {}
            }
        }
        map
    }

    pub fn get(&self, key: Keycode) -> Option<Action> {
        self.bindings.get(&key).copied()
    }
}

/// Live joystick state: which direction keys are held, and whether the
/// synthetic finger is currently down.
#[derive(Debug, Default)]
pub struct JoyState {
    held: Vec<(f32, f32)>,
    pub down: bool,
}

impl JoyState {
    pub fn press(&mut self, dx: f32, dy: f32) {
        if !self.held.contains(&(dx, dy)) {
            self.held.push((dx, dy));
        }
    }

    pub fn release(&mut self, dx: f32, dy: f32) {
        self.held.retain(|&d| d != (dx, dy));
    }

    /// Current stick deflection: the (normalized) sum of held directions.
    pub fn direction(&self) -> Option<(f32, f32)> {
        if self.held.is_empty() {
            return None;
        }
        let (sx, sy) = self
            .held
            .iter()
            .fold((0.0f32, 0.0f32), |(ax, ay), (dx, dy)| (ax + dx, ay + dy));
        let len = (sx * sx + sy * sy).sqrt();
        if len < 1e-3 {
            // Opposing keys cancel: stick centered but still held down.
            return Some((0.0, 0.0));
        }
        Some((sx / len, sy / len))
    }
}

/// SDL keycode from a config token ("w", "space", "lshift", …).
fn keycode(name: &str) -> Option<Keycode> {
    // SDL key names are capitalized ("Space", "Left Shift"); accept the
    // simple lowercase forms people actually type.
    let canonical = match name.to_lowercase().as_str() {
        "space" => "Space".to_string(),
        "lshift" => "Left Shift".to_string(),
        "rshift" => "Right Shift".to_string(),
        "tab" => "Tab".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        other => other.to_string(), // single letters/digits are their own name
    };
    Keycode::from_name(&canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_taps_and_joystick() {
        let map = Keymap::parse(
            "# demo\njoystick w a s d 0.2 0.75 0.12\ntap space 0.85 0.8\ntap e 0.92 0.6\n",
        );
        assert!(map.joystick.is_some());
        assert!(matches!(map.get(Keycode::Space), Some(Action::Tap { .. })));
        assert!(matches!(map.get(Keycode::W), Some(Action::Joy { dy, .. }) if dy < 0.0));
        // Distinct pointers per tap binding.
        let (Some(Action::Tap { pointer: p1, .. }), Some(Action::Tap { pointer: p2, .. })) =
            (map.get(Keycode::Space), map.get(Keycode::E))
        else {
            panic!("taps missing")
        };
        assert_ne!(p1, p2);
    }

    #[test]
    fn joystick_combines_directions() {
        let mut joy = JoyState::default();
        joy.press(0.0, -1.0); // W
        joy.press(1.0, 0.0); // D
        let (dx, dy) = joy.direction().unwrap();
        assert!(dx > 0.5 && dy < -0.5); // up-right diagonal, normalized
        joy.release(0.0, -1.0);
        assert_eq!(joy.direction(), Some((1.0, 0.0)));
        joy.release(1.0, 0.0);
        assert_eq!(joy.direction(), None);
    }

    #[test]
    fn opposing_keys_center_the_stick() {
        let mut joy = JoyState::default();
        joy.press(-1.0, 0.0);
        joy.press(1.0, 0.0);
        assert_eq!(joy.direction(), Some((0.0, 0.0)));
    }
}
