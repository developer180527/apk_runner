//! The player's presentation layer: streaming an Android virtual display
//! into a native window, with input, audio, and keymaps. The Hub is a
//! separate crate — this one never draws management UI.

pub mod app;
#[cfg(target_os = "macos")]
pub mod avlayer;
pub mod input;
pub mod keymap;

pub mod video;
