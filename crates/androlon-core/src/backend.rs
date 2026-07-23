//! Per-OS backend abstraction — the box that quarantines everything OS-specific
//! (hypervisor + guest→host GPU path). The portable core only ever sees these
//! traits, so lighting up a new OS means writing one backend, not touching the app.

use crate::config::SdkConfig;
use crate::error::Result;
use crate::model::{Avd, BootProfile, RootStatus, RuntimeKind};
use std::path::Path;

/// The guest→host GPU virtualization path (problem B). v1 = gfxstream (prebuilt
/// in Google's emulator); M5 = Venus (native Vulkan on Linux/Win, MoltenVK on macOS).
pub trait GpuBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn is_implemented(&self) -> bool;
    /// Extra `emulator` args this backend needs (v1 path).
    fn emulator_args(&self) -> Vec<String>;
}

/// v1: Google's emulator GPU path — guest Vulkan/GLES → host GPU via gfxstream
/// (Metal on macOS, native Vulkan on Linux/Windows). Prebuilt; we write nothing.
pub struct Gfxstream;
impl GpuBackend for Gfxstream {
    fn name(&self) -> &'static str {
        "gfxstream"
    }
    fn is_implemented(&self) -> bool {
        true
    }
    fn emulator_args(&self) -> Vec<String> {
        vec!["-gpu".into(), "auto".into()]
    }
}

/// Roadmap (M5): near-native Vulkan via virtio-gpu Venus. macOS routes Venus →
/// MoltenVK → Metal (the only place we own a translation layer, and even there
/// MoltenVK is prebuilt). Placeholder so call sites can reference it.
pub struct Venus;
impl GpuBackend for Venus {
    fn name(&self) -> &'static str {
        "venus"
    }
    fn is_implemented(&self) -> bool {
        false
    }
    fn emulator_args(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The Android runtime for one OS: provision the SDK/VM, boot, install, launch,
/// manage virtual displays, report root. v1 impl (`EmulatorService`) uses the
/// prebuilt AVD emulator and works on macOS/Linux/Windows; alternative impls
/// (e.g. Waydroid on Linux) can slot in behind this trait later.
pub trait AndroidBackend {
    /// Which runtime this is (for UI/logging).
    fn kind(&self) -> RuntimeKind;
    fn is_provisioned(&self) -> bool;
    fn install_packages(&self) -> Result<()>;
    fn create_avd(&self, avd: &Avd) -> Result<()>;
    /// Boot the runtime. `profile` selects snapshot behaviour (Developer vs Consumer).
    fn boot(&self, avd: &Avd, profile: BootProfile, log_file: &Path) -> Result<()>;
    fn install_apk(&self, apk: &Path) -> Result<()>;
    fn root_status(&self) -> RootStatus;
    fn stop(&self);
}

/// Build the runtime the user selected. The rest of the app depends only on the
/// returned trait object, so emulator↔Waydroid is a one-line swap here.
pub fn make_backend(config: SdkConfig, runtime: RuntimeKind) -> Box<dyn AndroidBackend> {
    match runtime {
        RuntimeKind::Emulator => Box::new(crate::emulator::EmulatorService::new(config)),
        RuntimeKind::Waydroid => Box::new(crate::waydroid::WaydroidBackend::new(config)),
    }
}
