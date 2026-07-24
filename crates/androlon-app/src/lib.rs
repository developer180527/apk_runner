//! Androlon's presentation layer, shared by the suite's front-end binaries:
//! `androlon-app` (the Hub shell: management UI + panes) and
//! `androlon-player` (slim single-app player that appified bundles run).

pub mod app;
#[cfg(target_os = "macos")]
pub mod avlayer;
pub mod input;
pub mod keymap;
pub mod ui;
pub mod video;
