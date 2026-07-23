import Foundation

/// Provisions the SDK, creates AVDs, and boots/stops the emulator — the Swift
/// port of `apkrun.sh`'s setup/avd/boot/stop stages.
public struct EmulatorService: Sendable {
    public let config: SdkConfig
    public let gpu: GpuBackend

    public init(config: SdkConfig, gpu: GpuBackend = GfxstreamBackend()) {
        self.config = config
        self.gpu = gpu
    }

    private var adb: AdbService { AdbService(config: config) }

    // MARK: provisioning

    public func isSdkProvisioned() -> Bool {
        let fm = FileManager.default
        return fm.isExecutableFile(atPath: config.sdkmanager.path)
            && fm.isExecutableFile(atPath: config.emulator.path)
    }

    /// Install platform-tools, emulator, build-tools, platform and the arm64
    /// userdebug system image. (Large first-run download.)
    public func installPackages() throws {
        guard FileManager.default.isExecutableFile(atPath: config.sdkmanager.path) else {
            throw EngineError.sdkMissing(tool: "sdkmanager")
        }
        try runCommand(config.sdkmanager, [
            "--sdk_root=\(config.sdkRoot.path)",
            "platform-tools", "emulator",
            "build-tools;\(config.buildTools)",
            config.platform, config.systemImage,
        ], extraPATH: config.toolDirs, env: config.toolEnv)
    }

    // MARK: AVDs

    public func listAvds() -> [String] {
        guard let result = try? runCommand(config.avdmanager, ["list", "avd"],
                                           extraPATH: config.toolDirs, env: config.toolEnv)
        else { return [] }
        return result.output
            .split(separator: "\n")
            .compactMap { line in
                line.contains("Name:")
                    ? line.split(separator: ":").last.map { $0.trimmingCharacters(in: .whitespaces) }
                    : nil
            }
    }

    public func createAvd(_ avd: Avd) throws {
        guard FileManager.default.isExecutableFile(atPath: config.avdmanager.path) else {
            throw EngineError.sdkMissing(tool: "avdmanager")
        }
        if listAvds().contains(avd.name) { return }
        try runCommand(config.avdmanager, [
            "create", "avd", "-n", avd.name,
            "-k", config.systemImage, "-d", avd.deviceProfile, "--force",
        ], extraPATH: config.toolDirs, env: config.toolEnv)
    }

    // MARK: boot / stop

    public func emulatorRunning() -> Bool { adb.deviceOnline() }

    /// Boot an AVD detached and wait for `sys.boot_completed`. Returns the
    /// emulator process. `-no-snapshot` keeps behaviour deterministic (and is
    /// required so Magisk changes survive — snapshots would discard them).
    @discardableResult
    public func boot(_ avd: Avd, logFile: URL, timeout: TimeInterval = 180) throws -> Process {
        guard FileManager.default.isExecutableFile(atPath: config.emulator.path) else {
            throw EngineError.sdkMissing(tool: "emulator")
        }
        let args = ["-avd", avd.name, "-no-snapshot"] + gpu.emulatorArgs()
        let proc = try launchDetached(config.emulator, args,
                                      extraPATH: config.toolDirs, env: config.toolEnv,
                                      logFile: logFile)
        try adb.adb(["wait-for-device"])
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if (try? adb.shell(["getprop", "sys.boot_completed"])) == "1" {
                _ = try? adb.shell(["input", "keyevent", "82"]) // dismiss keyguard
                return proc
            }
            Thread.sleep(forTimeInterval: 2)
        }
        return proc // booted enough to return; caller can re-check readiness
    }

    public func stop() {
        _ = try? adb.adb(["emu", "kill"])
    }

    // MARK: doctor

    public func doctor() -> DoctorReport {
        let fm = FileManager.default
        let checks: [(String, URL)] = [
            ("sdkmanager", config.sdkmanager),
            ("avdmanager", config.avdmanager),
            ("adb", config.adb),
            ("emulator", config.emulator),
            ("aapt2", config.aapt2),
        ]
        let tools = checks.map {
            ToolCheck(name: $0.0, path: $0.1,
                      present: fm.isExecutableFile(atPath: $0.1.path))
        }
        return DoctorReport(
            sdkRoot: config.sdkRoot,
            api: config.api,
            systemImage: config.systemImage,
            tools: tools,
            avds: listAvds(),
            emulatorRunning: emulatorRunning(),
            rootStatus: adb.rootStatus(),
            isRootable: config.isRootable
        )
    }
}
