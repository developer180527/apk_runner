use std::path::PathBuf;

/// App-visible root state of the running Android instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootStatus {
    None,   // no root
    Adbd,   // `adb root` shell only (userdebug)
    Magisk, // systemless su visible to apps
}

impl RootStatus {
    pub fn label(self) -> &'static str {
        match self {
            RootStatus::None => "none",
            RootStatus::Adbd => "adbd",
            RootStatus::Magisk => "magisk",
        }
    }
}

/// How Android surfaces are presented on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    Desktop,   // whole Android desktop in one window
    Coherence, // one window per app (per virtual display)
    Phone,     // single phone-shaped window
}

/// Boot behaviour. Developer favours determinism + root persistence; Consumer
/// favours a fast, phone-like resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProfile {
    /// Cold boot every time, no snapshot save/load. Required for Magisk/root
    /// changes to persist and for reproducible behaviour.
    Developer,
    /// Quick Boot (default): load a saved snapshot if present, save on exit —
    /// so end users get near-instant resume like closing/opening a phone.
    Consumer,
}

impl BootProfile {
    /// Emulator snapshot flags for this profile. (Consumer = no flags = the
    /// emulator's default Quick Boot; Developer = disable all snapshotting.)
    pub fn snapshot_args(self) -> Vec<String> {
        match self {
            BootProfile::Developer => vec!["-no-snapshot".into()],
            BootProfile::Consumer => Vec::new(),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            BootProfile::Developer => "developer",
            BootProfile::Consumer => "consumer",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "developer" | "dev" => Some(BootProfile::Developer),
            "consumer" | "user" => Some(BootProfile::Consumer),
            _ => None,
        }
    }
}

/// Which Android runtime backs the host — the user-swappable engine. The
/// portable core only ever sees the `AndroidBackend` trait, so adding a runtime
/// is one impl, not an app change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Google's AVD emulator (QEMU + host hypervisor). Cross-platform.
    Emulator,
    /// Waydroid (LXC container on the host kernel). Linux only; lighter than a VM.
    Waydroid,
}

impl RuntimeKind {
    pub fn label(self) -> &'static str {
        match self {
            RuntimeKind::Emulator => "emulator",
            RuntimeKind::Waydroid => "waydroid",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "emulator" | "avd" => Some(RuntimeKind::Emulator),
            "waydroid" => Some(RuntimeKind::Waydroid),
            _ => None,
        }
    }
    /// Whether this runtime can run on the current host OS.
    pub fn supported_here(self) -> bool {
        match self {
            RuntimeKind::Emulator => true,
            RuntimeKind::Waydroid => cfg!(target_os = "linux"),
        }
    }
}

/// A configured AVD the engine manages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avd {
    pub name: String,
    pub device_profile: String,
    /// (width, height, dpi) written into the AVD config. Coherence gives each
    /// app its own virtual display, so this is just the backing display.
    pub screen: (u32, u32, u32),
    /// The emulator's own hash of the device profile. It must match the
    /// profile, or hardware resolution falls back to defaults.
    pub device_hash: String,
}

impl Avd {
    pub fn phone() -> Self {
        Avd {
            name: "androlon_phone".into(),
            device_profile: "medium_phone".into(),
            screen: (1080, 2400, 420),
            device_hash: MEDIUM_PHONE_HASH.into(),
        }
    }
    /// The backing runtime display. Coherence gives each app its own virtual
    /// display, so this is never what the user sees — keep it consistent with
    /// `device_profile` (a profile/screen mismatch confuses the emulator's
    /// hardware resolution).
    pub fn desktop() -> Self {
        Avd {
            name: "androlon_desktop".into(),
            device_profile: "medium_phone".into(),
            screen: (1080, 2400, 420),
            device_hash: MEDIUM_PHONE_HASH.into(),
        }
    }
}

/// `hw.device.hash2` for the `medium_phone` profile, as the emulator computes it.
const MEDIUM_PHONE_HASH: &str = "MD5:3db3250dab5d0d93b29353040181c7e9";

/// Where AVDs live (`$ANDROID_AVD_HOME`, else `~/.android/avd`).
pub fn avd_home() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("ANDROID_AVD_HOME") {
        return std::path::PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".android/avd"))
        .unwrap_or_else(|_| std::path::PathBuf::from(".android/avd"))
}

/// Presence check for one required tool.
#[derive(Debug, Clone)]
pub struct ToolCheck {
    pub name: String,
    pub path: PathBuf,
    pub present: bool,
}

/// Aggregated health snapshot — the equivalent of `apkrun.sh doctor`, reused by
/// the CLI now and the ImGui Settings panel later.
#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub sdk_root: PathBuf,
    pub api: u32,
    pub system_image: String,
    pub tools: Vec<ToolCheck>,
    pub avds: Vec<String>,
    pub emulator_running: bool,
    pub root_status: RootStatus,
    pub is_rootable: bool,
}
