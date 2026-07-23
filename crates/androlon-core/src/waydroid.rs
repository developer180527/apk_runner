//! Waydroid runtime backend (Linux). Waydroid runs Android in an LXC container
//! on the host kernel — much lighter than a VM, but Linux-only and it needs a
//! Wayland session. Implemented behind the same `AndroidBackend` trait as the
//! emulator so the host app can swap runtimes without any other change.
//!
//! On non-Linux hosts every operation returns a clear "unsupported" error.

use crate::backend::AndroidBackend;
use crate::config::SdkConfig;
use crate::error::Result;
use crate::model::{Avd, BootProfile, RootStatus, RuntimeKind};
use std::path::Path;

#[cfg(target_os = "linux")]
use crate::subprocess::{run, CommandOutput};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use crate::error::EngineError;

pub struct WaydroidBackend {
    #[allow(dead_code)]
    config: SdkConfig,
}

impl WaydroidBackend {
    pub fn new(config: SdkConfig) -> Self {
        WaydroidBackend { config }
    }

    #[cfg(target_os = "linux")]
    fn waydroid(&self, args: &[&str]) -> Result<CommandOutput> {
        // `waydroid` is expected on PATH on a configured Linux host.
        run(&PathBuf::from("waydroid"), args, &[], &[])
    }

    #[cfg(not(target_os = "linux"))]
    fn unsupported<T>(&self) -> Result<T> {
        Err(EngineError::BackendUnimplemented(
            "waydroid (Linux-only runtime; not available on this OS)".into(),
        ))
    }
}

impl AndroidBackend for WaydroidBackend {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Waydroid
    }

    #[cfg(target_os = "linux")]
    fn is_provisioned(&self) -> bool {
        run(&PathBuf::from("waydroid"), &["status"], &[], &[])
            .map(|o| o.ok())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    fn is_provisioned(&self) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn install_packages(&self) -> Result<()> {
        // `waydroid init` downloads the system + vendor images.
        self.waydroid(&["init"]).map(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    fn install_packages(&self) -> Result<()> {
        self.unsupported()
    }

    fn create_avd(&self, _avd: &Avd) -> Result<()> {
        // Waydroid has no per-AVD concept; the single container is the device.
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn boot(&self, _avd: &Avd, _profile: BootProfile, _log_file: &Path) -> Result<()> {
        // Waydroid persists container state; the Developer/Consumer snapshot
        // distinction doesn't apply, so `profile` is intentionally ignored.
        self.waydroid(&["session", "start"]).map(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    fn boot(&self, _avd: &Avd, _profile: BootProfile, _log_file: &Path) -> Result<()> {
        self.unsupported()
    }

    #[cfg(target_os = "linux")]
    fn install_apk(&self, apk: &Path) -> Result<()> {
        self.waydroid(&["app", "install", &apk.display().to_string()])
            .map(|_| ())
    }
    #[cfg(not(target_os = "linux"))]
    fn install_apk(&self, _apk: &Path) -> Result<()> {
        self.unsupported()
    }

    fn root_status(&self) -> RootStatus {
        // Waydroid images can ship rooted; probing is a later refinement.
        RootStatus::None
    }

    #[cfg(target_os = "linux")]
    fn stop(&self) {
        let _ = self.waydroid(&["session", "stop"]);
    }
    #[cfg(not(target_os = "linux"))]
    fn stop(&self) {}
}
