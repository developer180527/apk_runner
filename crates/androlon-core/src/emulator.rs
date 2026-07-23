use crate::adb::AdbService;
use crate::backend::{AndroidBackend, Gfxstream, GpuBackend};
use crate::config::{is_executable, SdkConfig};
use crate::error::{EngineError, Result};
use crate::model::{Avd, BootProfile, DoctorReport, RootStatus, RuntimeKind, ToolCheck};
use crate::subprocess::{run, run_checked, spawn_detached};
use std::path::Path;
use std::time::{Duration, Instant};

/// Provisions the SDK, creates AVDs, boots/stops the emulator — the Swift/bash
/// port. The v1 `AndroidBackend` for all three OSes (prebuilt AVD emulator).
pub struct EmulatorService {
    pub config: SdkConfig,
    pub gpu: Box<dyn GpuBackend>,
}

impl EmulatorService {
    pub fn new(config: SdkConfig) -> Self {
        EmulatorService { config, gpu: Box::new(Gfxstream) }
    }

    pub fn with_gpu(config: SdkConfig, gpu: Box<dyn GpuBackend>) -> Self {
        EmulatorService { config, gpu }
    }

    fn adb(&self) -> AdbService<'_> {
        AdbService::new(&self.config)
    }

    pub fn list_avds(&self) -> Vec<String> {
        let out = match run(
            &self.config.avdmanager(),
            &["list", "avd"],
            &self.config.tool_dirs(),
            &self.config.tool_env(),
        ) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        out.output
            .lines()
            .filter(|l| l.contains("Name:"))
            .filter_map(|l| l.split(':').nth(1).map(|s| s.trim().to_string()))
            .collect()
    }

    pub fn emulator_running(&self) -> bool {
        self.adb().device_online()
    }

    /// Boot an AVD detached and wait for `sys.boot_completed`. The profile picks
    /// snapshot behaviour: Developer (`-no-snapshot`, deterministic + root-safe)
    /// vs Consumer (Quick Boot fast resume).
    pub fn boot_and_wait(
        &self,
        avd: &Avd,
        profile: BootProfile,
        log_file: &Path,
        timeout: Duration,
    ) -> Result<()> {
        self.boot_and_wait_opts(avd, profile, log_file, timeout, false)
    }

    /// `headless` hides the emulator's own window (`-no-window`) — the suite
    /// presents Android through Coherence panes, so the runtime stays unseen.
    pub fn boot_and_wait_opts(
        &self,
        avd: &Avd,
        profile: BootProfile,
        log_file: &Path,
        timeout: Duration,
        headless: bool,
    ) -> Result<()> {
        if !is_executable(&self.config.emulator()) {
            return Err(EngineError::SdkMissing("emulator".into()));
        }
        let mut args: Vec<String> = vec!["-avd".into(), avd.name.clone()];
        args.extend(profile.snapshot_args());
        args.extend(self.gpu.emulator_args());
        if headless {
            args.push("-no-window".into());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _child = spawn_detached(
            &self.config.emulator(),
            &arg_refs,
            &self.config.tool_dirs(),
            &self.config.tool_env(),
            log_file,
        )?;

        let adb = self.adb();
        adb.adb(&["wait-for-device"])?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if adb.shell(&["getprop", "sys.boot_completed"]).ok().as_deref() == Some("1") {
                let _ = adb.shell(&["input", "keyevent", "82"]); // dismiss keyguard
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        Ok(()) // booted enough to return; caller can re-check readiness
    }

    pub fn doctor(&self) -> DoctorReport {
        let checks = [
            ("sdkmanager", self.config.sdkmanager()),
            ("avdmanager", self.config.avdmanager()),
            ("adb", self.config.adb()),
            ("emulator", self.config.emulator()),
            ("aapt2", self.config.aapt2()),
        ];
        let tools = checks
            .iter()
            .map(|(name, path)| ToolCheck {
                name: name.to_string(),
                present: is_executable(path),
                path: path.clone(),
            })
            .collect();

        DoctorReport {
            sdk_root: self.config.sdk_root.clone(),
            api: self.config.api,
            system_image: self.config.system_image(),
            tools,
            avds: self.list_avds(),
            emulator_running: self.emulator_running(),
            root_status: self.adb().root_status(),
            is_rootable: self.config.is_rootable(),
        }
    }
}

impl AndroidBackend for EmulatorService {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Emulator
    }

    fn is_provisioned(&self) -> bool {
        is_executable(&self.config.sdkmanager()) && is_executable(&self.config.emulator())
    }

    fn install_packages(&self) -> Result<()> {
        if !is_executable(&self.config.sdkmanager()) {
            return Err(EngineError::SdkMissing("sdkmanager".into()));
        }
        let sdk_arg = format!("--sdk_root={}", self.config.sdk_root.display());
        let build_tools = format!("build-tools;{}", self.config.build_tools);
        let platform = self.config.platform();
        let sysimg = self.config.system_image();
        run_checked(
            &self.config.sdkmanager(),
            &[
                &sdk_arg,
                "platform-tools",
                "emulator",
                &build_tools,
                &platform,
                &sysimg,
            ],
            &self.config.tool_dirs(),
            &self.config.tool_env(),
        )?;
        Ok(())
    }

    fn create_avd(&self, avd: &Avd) -> Result<()> {
        if !is_executable(&self.config.avdmanager()) {
            return Err(EngineError::SdkMissing("avdmanager".into()));
        }
        if self.list_avds().contains(&avd.name) {
            return Ok(());
        }
        run_checked(
            &self.config.avdmanager(),
            &[
                "create", "avd", "-n", &avd.name, "-k", &self.config.system_image(),
                "-d", &avd.device_profile, "--force",
            ],
            &self.config.tool_dirs(),
            &self.config.tool_env(),
        )?;
        Ok(())
    }

    fn boot(&self, avd: &Avd, profile: BootProfile, log_file: &Path) -> Result<()> {
        self.boot_and_wait(avd, profile, log_file, Duration::from_secs(180))
    }

    fn install_apk(&self, apk: &Path) -> Result<()> {
        self.adb().install(apk)
    }

    fn root_status(&self) -> RootStatus {
        self.adb().root_status()
    }

    fn stop(&self) {
        let _ = self.adb().adb(&["emu", "kill"]);
    }
}
