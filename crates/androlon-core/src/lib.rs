//! Androlon engine core — portable (Rust, std-only) orchestration of a
//! self-contained Android SDK/emulator. Mirrors `apkrun.sh`. No OS APIs here;
//! anything OS-specific lives behind the `backend` traits.

pub mod adb;
pub mod appify;
pub mod backend;
pub mod config;
pub mod emulator;
pub mod error;
pub mod model;
pub mod subprocess;
pub mod waydroid;

pub use adb::AdbService;
pub use backend::{make_backend, AndroidBackend, Gfxstream, GpuBackend, Venus};
pub use config::SdkConfig;
pub use emulator::EmulatorService;
pub use error::{EngineError, Result};
pub use model::{
    avd_home, Avd, BootProfile, DoctorReport, RootStatus, RuntimeKind, ToolCheck, WindowMode,
};
pub use waydroid::WaydroidBackend;
