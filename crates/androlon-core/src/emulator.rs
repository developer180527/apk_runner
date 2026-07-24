use crate::adb::AdbService;
use crate::backend::{AndroidBackend, Gfxstream, GpuBackend};
use crate::config::{is_executable, SdkConfig};
use crate::error::{EngineError, Result};
use crate::model::{avd_home, Avd, BootProfile, DoctorReport, RootStatus, RuntimeKind, ToolCheck};
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

    /// Replace avdmanager's generated `config.ini` with one we control.
    ///
    /// Two things matter here, both diagnosed 2026-07-24 against an identical
    /// system image that boots fine from a hand-written config:
    ///
    /// 1. **`tag.ids` must declare `page_size_16kb`** for `_ps16k` images.
    ///    Without it the emulator misconfigures the guest page size, refuses
    ///    HVF ("hvf is not enabled on this aarch64 host"), silently falls back
    ///    to TCG, and never finishes booting. Apple Silicon is natively
    ///    16 KB-page, so this is the difference between a 25-second boot and
    ///    an infinite one.
    /// 2. avdmanager emits unsubstituted template placeholders
    ///    (`avd.id=<build>`, `disk.dataPartition.path=<temp>`) and
    ///    `hw.gpu.enabled=no`.
    fn write_avd_config(&self, avd: &Avd) -> Result<()> {
        let dir = avd_home().join(format!("{}.avd", avd.name));
        if !dir.is_dir() {
            return Ok(()); // avdmanager put it somewhere unexpected; leave it be
        }
        let (w, h, density) = avd.screen;
        // Field-for-field the config verified to boot with HVF. Resist
        // "improving" values here — several innocuous-looking edits (screen
        // size not matching the device profile, a missing hw.device.hash2)
        // were enough to send it back to a TCG non-boot.
        //
        // Cores and RAM are the exception, and are deliberately generous:
        // the emulator has no hardware video encoder, so scrcpy's H.264
        // encode runs on guest CPU and is the frame-rate ceiling for games.
        // Guest cores buy encode headroom directly.
        let config = format!(
            "AvdId={name}\n\
             avd.ini.displayname={name}\n\
             avd.ini.encoding=UTF-8\n\
             abi.type={abi}\n\
             hw.cpu.arch=arm64\n\
             hw.cpu.ncore=6\n\
             hw.ramSize=6144\n\
             vm.heapSize=512\n\
             disk.dataPartition.size=10G\n\
             sdcard.size=512M\n\
             hw.sdCard=yes\n\
             hw.gpu.enabled=yes\n\
             hw.gpu.mode=auto\n\
             hw.keyboard=yes\n\
             hw.mainKeys=no\n\
             hw.dPad=no\n\
             hw.trackBall=no\n\
             hw.gps=yes\n\
             hw.lcd.width={w}\n\
             hw.lcd.height={h}\n\
             hw.lcd.density={density}\n\
             hw.initialOrientation=portrait\n\
             hw.audioInput=yes\n\
             hw.battery=yes\n\
             hw.accelerometer=yes\n\
             hw.gyroscope=yes\n\
             hw.sensors.orientation=yes\n\
             hw.sensors.proximity=yes\n\
             hw.sensors.light=yes\n\
             hw.sensors.magnetic_field=yes\n\
             hw.sensors.pressure=yes\n\
             hw.camera.back=virtualscene\n\
             hw.camera.front=emulated\n\
             hw.arc=false\n\
             hw.device.manufacturer=Generic\n\
             hw.device.name={profile}\n\
             hw.device.hash2={hash2}\n\
             skin.dynamic=yes\n\
             showDeviceFrame=yes\n\
             fastboot.forceFastBoot=yes\n\
             fastboot.forceColdBoot=no\n\
             fastboot.forceChosenSnapshotBoot=no\n\
             fastboot.chosenSnapshotFile=\n\
             PlayStore.enabled={playstore}\n\
             image.sysdir.1=system-images/android-{tag}/{image_type}/{abi}/\n\
             tag.id={image_type_base}\n\
             tag.display={image_type_base}\n\
             tag.ids={tag_ids}\n\
             target=android-{tag}\n\
             runtime.network.latency=none\n\
             runtime.network.speed=full\n",
            name = avd.name,
            abi = self.config.abi,
            profile = avd.device_profile,
            hash2 = avd.device_hash,
            tag = self.config.platform_tag,
            image_type = self.config.image_type,
            // The tag id drops the page-size suffix the image dir carries…
            image_type_base = self.config.image_type.replace("_ps16k", ""),
            // …and `tag.ids` re-declares it as a separate tag. Required: see
            // the note above — without it HVF is refused and boot never ends.
            tag_ids = if self.config.image_type.contains("ps16k") {
                format!("{},page_size_16kb", self.config.image_type.replace("_ps16k", ""))
            } else {
                self.config.image_type.clone()
            },
            playstore = self.config.image_type.contains("playstore"),
        );
        std::fs::write(dir.join("config.ini"), config).map_err(|e| EngineError::Launch {
            tool: "write AVD config".into(),
            source: e,
        })?;

        // …and the sibling `<name>.ini`, which avdmanager fills in with
        // `target=android-0` when it can't resolve the platform package. The
        // emulator reads the target from here; an unresolvable one leaves the
        // VM misconfigured, HVF refused, and the boot hanging in TCG forever.
        let ini = format!(
            "avd.ini.encoding=UTF-8\n\
             path={dir}\n\
             path.rel=avd/{name}.avd\n\
             target=android-{tag}\n",
            dir = dir.display(),
            name = avd.name,
            tag = self.config.platform_tag,
        );
        std::fs::write(avd_home().join(format!("{}.ini", avd.name)), ini).map_err(|e| {
            EngineError::Launch { tool: "write AVD ini".into(), source: e }
        })
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
        if headless {
            // Without a window, `-gpu auto` falls back to SwiftShader
            // (software) — force the host GPU so games stay accelerated.
            args.push("-no-window".into());
            args.push("-gpu".into());
            args.push("host".into());
        } else {
            args.extend(self.gpu.emulator_args());
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
        self.write_avd_config(avd)?;
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
