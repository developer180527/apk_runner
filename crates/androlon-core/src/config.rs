use std::path::{Path, PathBuf};

/// Locations and versions for a self-contained Android SDK, mirroring the top of
/// `apkrun.sh`. Everything lives under `sdk_root` so we never touch a system SDK.
#[derive(Debug, Clone)]
pub struct SdkConfig {
    pub sdk_root: PathBuf,
    /// Numeric API level, for display/target (e.g. 37 = Android 17).
    pub api: u32,
    /// Dotted platform tag used in SDK package ids (Android 17 ships as
    /// `android-37.0`, `android-37.1`, …, not `android-37`).
    pub platform_tag: String,
    /// Android 17 images are all 16 KB-page (`_ps16k`) variants:
    /// `google_apis_ps16k` (userdebug, rootable, no Play) |
    /// `google_apis_playstore_ps16k` (locked user build, NOT rootable).
    pub image_type: String,
    pub abi: String,
    pub build_tools: String,
}

impl Default for SdkConfig {
    fn default() -> Self {
        SdkConfig {
            sdk_root: PathBuf::from(".android-sdk"),
            api: 37, // Android 17
            platform_tag: "37.0".to_string(),
            image_type: "google_apis_ps16k".to_string(),
            abi: "arm64-v8a".to_string(),
            build_tools: "37.0.0".to_string(),
        }
    }
}

impl SdkConfig {
    /// Resolve from env (`APKRUN_SDK`/`APKRUN_API`/`APKRUN_IMG_TYPE`), consistent
    /// with the bash prototype.
    pub fn from_env() -> Self {
        let mut cfg = SdkConfig::default();
        if let Ok(root) = std::env::var("APKRUN_SDK") {
            cfg.sdk_root = PathBuf::from(root);
        } else {
            cfg.sdk_root = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".android-sdk");
        }
        if let Ok(api) = std::env::var("APKRUN_API") {
            if let Ok(n) = api.parse() {
                cfg.api = n;
            }
        }
        if let Ok(tag) = std::env::var("APKRUN_API_TAG") {
            cfg.platform_tag = tag;
        }
        if let Ok(t) = std::env::var("APKRUN_IMG_TYPE") {
            cfg.image_type = t;
        }
        cfg
    }

    // Package coordinates for sdkmanager / avdmanager.
    pub fn system_image(&self) -> String {
        format!("system-images;android-{};{};{}", self.platform_tag, self.image_type, self.abi)
    }
    pub fn platform(&self) -> String {
        format!("platforms;android-{}", self.platform_tag)
    }

    // Tool directories.
    pub fn cmdline_tools_bin(&self) -> PathBuf {
        self.sdk_root.join("cmdline-tools/latest/bin")
    }
    pub fn platform_tools_bin(&self) -> PathBuf {
        self.sdk_root.join("platform-tools")
    }
    pub fn emulator_bin(&self) -> PathBuf {
        self.sdk_root.join("emulator")
    }
    pub fn build_tools_bin(&self) -> PathBuf {
        self.sdk_root.join(format!("build-tools/{}", self.build_tools))
    }

    // Individual tools (add `.exe` on Windows at call sites via `exe()` helper).
    pub fn adb(&self) -> PathBuf {
        self.platform_tools_bin().join(exe("adb"))
    }
    pub fn emulator(&self) -> PathBuf {
        self.emulator_bin().join(exe("emulator"))
    }
    pub fn sdkmanager(&self) -> PathBuf {
        self.cmdline_tools_bin().join(bat("sdkmanager"))
    }
    pub fn avdmanager(&self) -> PathBuf {
        self.cmdline_tools_bin().join(bat("avdmanager"))
    }
    pub fn aapt2(&self) -> PathBuf {
        self.build_tools_bin().join(exe("aapt2"))
    }

    /// The system-image ramdisk rootAVD patches with Magisk.
    pub fn ramdisk_image(&self) -> PathBuf {
        self.sdk_root.join(format!(
            "system-images/android-{}/{}/{}/ramdisk.img",
            self.platform_tag, self.image_type, self.abi
        ))
    }

    /// Play-Store images are `user` builds and cannot be rooted.
    pub fn is_rootable(&self) -> bool {
        !self.image_type.contains("playstore")
    }

    /// Tool dirs to prepend onto a child PATH.
    pub fn tool_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.platform_tools_bin(),
            self.emulator_bin(),
            self.cmdline_tools_bin(),
            self.build_tools_bin(),
        ]
    }

    /// Environment a child SDK tool expects.
    pub fn tool_env(&self) -> Vec<(String, String)> {
        let root = self.sdk_root.display().to_string();
        vec![
            ("ANDROID_SDK_ROOT".into(), root.clone()),
            ("ANDROID_HOME".into(), root),
        ]
    }
}

/// Append `.exe` to a tool name on Windows.
fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// sdkmanager/avdmanager ship as `.bat` on Windows.
fn bat(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.bat")
    } else {
        name.to_string()
    }
}

/// True if `p` exists and (on Unix) is executable.
pub fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}
