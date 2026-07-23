import Foundation

/// Wraps `adb`: device state, root, install, launch, shell, props. Direct port
/// of the adb-facing helpers in `apkrun.sh`.
public struct AdbService: Sendable {
    public let config: SdkConfig
    public init(config: SdkConfig) { self.config = config }

    @discardableResult
    public func adb(_ args: [String]) throws -> CommandResult {
        try runCommand(config.adb, args, extraPATH: config.toolDirs, env: config.toolEnv)
    }

    /// `adb shell …`, returning trimmed combined output.
    @discardableResult
    public func shell(_ args: [String]) throws -> String {
        try adb(["shell"] + args).trimmed
    }

    /// Is an emulator/device in `device` state?
    public func deviceOnline() -> Bool {
        guard let result = try? adb(["get-state"]) else { return false }
        return result.trimmed == "device"
    }

    /// Classify current root: magisk (app-visible su) > adbd (root shell) > none.
    public func rootStatus() -> RootStatus {
        guard deviceOnline() else { return .none }
        if let su = try? shell(["su", "-c", "id"]), su.contains("uid=0") { return .magisk }
        if let uid = try? shell(["id", "-u"]), uid == "0" { return .adbd }
        return .none
    }

    /// Elevate adbd to root (userdebug images only).
    public func enableAdbdRoot() throws {
        guard config.isRootable else {
            throw EngineError.notRootable(config.imageType)
        }
        try adb(["root"])
        try adb(["wait-for-device"])
    }

    /// Install (or reinstall) an APK, granting runtime permissions.
    public func install(_ apk: URL) throws {
        try adb(["install", "-r", "-g", apk.path])
    }

    /// Best-effort package name via aapt2 (empty string if unavailable).
    public func packageName(of apk: URL) -> String {
        guard FileManager.default.isExecutableFile(atPath: config.aapt2.path) else { return "" }
        return (try? runCommand(config.aapt2, ["dump", "packagename", apk.path]))?.trimmed ?? ""
    }

    /// Resolve `pkg/.Activity` for the launcher entry point.
    public func launchComponent(for pkg: String) -> String? {
        let out = try? shell([
            "cmd", "package", "resolve-activity", "--brief",
            "-c", "android.intent.category.LAUNCHER", pkg,
        ])
        guard let line = out?.split(separator: "\n").last.map(String.init),
              line.contains("/") else { return nil }
        return line
    }

    /// Start an app, optionally on a specific virtual display (Coherence mode).
    public func launch(component: String, onDisplay display: Int? = nil) throws {
        var args = ["am", "start", "-n", component]
        if let display { args += ["--display", String(display)] }
        try shell(args)
    }
}
