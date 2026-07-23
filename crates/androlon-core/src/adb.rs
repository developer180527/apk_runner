use crate::config::{is_executable, SdkConfig};
use crate::error::{EngineError, Result};
use crate::model::RootStatus;
use crate::subprocess::{run, run_checked};
use std::path::Path;

/// Wraps `adb`: device state, root, install, launch, shell. Port of the
/// adb-facing helpers in `apkrun.sh`.
pub struct AdbService<'a> {
    pub config: &'a SdkConfig,
}

impl<'a> AdbService<'a> {
    pub fn new(config: &'a SdkConfig) -> Self {
        AdbService { config }
    }

    pub fn adb(&self, args: &[&str]) -> Result<crate::subprocess::CommandOutput> {
        run(&self.config.adb(), args, &self.config.tool_dirs(), &self.config.tool_env())
    }

    /// `adb shell …`, returning trimmed combined output.
    pub fn shell(&self, args: &[&str]) -> Result<String> {
        let mut full = vec!["shell"];
        full.extend_from_slice(args);
        Ok(self.adb(&full)?.trimmed().to_string())
    }

    /// Is an emulator/device in `device` state?
    pub fn device_online(&self) -> bool {
        self.adb(&["get-state"])
            .map(|o| o.trimmed() == "device")
            .unwrap_or(false)
    }

    /// Classify current root: magisk (app-visible su) > adbd (root shell) > none.
    pub fn root_status(&self) -> RootStatus {
        if !self.device_online() {
            return RootStatus::None;
        }
        if let Ok(su) = self.shell(&["su", "-c", "id"]) {
            if su.contains("uid=0") {
                return RootStatus::Magisk;
            }
        }
        if let Ok(uid) = self.shell(&["id", "-u"]) {
            if uid == "0" {
                return RootStatus::Adbd;
            }
        }
        RootStatus::None
    }

    /// Elevate adbd to root (userdebug images only).
    pub fn enable_adbd_root(&self) -> Result<()> {
        if !self.config.is_rootable() {
            return Err(EngineError::NotRootable(self.config.image_type.clone()));
        }
        self.adb(&["root"])?;
        self.adb(&["wait-for-device"])?;
        Ok(())
    }

    /// Install (or reinstall) an APK, granting runtime permissions.
    pub fn install(&self, apk: &Path) -> Result<()> {
        run_checked(
            &self.config.adb(),
            &["install", "-r", "-g", &apk.display().to_string()],
            &self.config.tool_dirs(),
            &self.config.tool_env(),
        )?;
        Ok(())
    }

    /// Best-effort package name via aapt2 (empty if unavailable).
    pub fn package_name(&self, apk: &Path) -> String {
        if !is_executable(&self.config.aapt2()) {
            return String::new();
        }
        run(
            &self.config.aapt2(),
            &["dump", "packagename", &apk.display().to_string()],
            &self.config.tool_dirs(),
            &self.config.tool_env(),
        )
        .map(|o| o.trimmed().to_string())
        .unwrap_or_default()
    }

    /// Resolve `pkg/.Activity` for the launcher entry point.
    pub fn launch_component(&self, pkg: &str) -> Option<String> {
        let out = self
            .shell(&[
                "cmd", "package", "resolve-activity", "--brief", "-c",
                "android.intent.category.LAUNCHER", pkg,
            ])
            .ok()?;
        let line = out.lines().last()?.trim().to_string();
        if line.contains('/') {
            Some(line)
        } else {
            None
        }
    }

    /// Start an app, optionally on a specific virtual display (Coherence mode).
    pub fn launch(&self, component: &str, display: Option<u32>) -> Result<()> {
        let display_str;
        let mut args: Vec<&str> = vec!["am", "start", "-n", component];
        if let Some(d) = display {
            display_str = d.to_string();
            args.push("--display");
            args.push(&display_str);
        }
        self.shell(&args)?;
        Ok(())
    }
}
