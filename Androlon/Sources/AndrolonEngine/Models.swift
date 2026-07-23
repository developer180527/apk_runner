import Foundation

/// App-visible root state of the running Android instance.
public enum RootStatus: String, Sendable {
    case none    // no root
    case adbd    // `adb root` shell only (userdebug)
    case magisk  // systemless su visible to apps

    public var label: String { rawValue }
}

/// How Android surfaces are presented on the host.
public enum WindowMode: String, Sendable {
    case desktop    // whole Android desktop in one macOS window
    case coherence  // one macOS window per app (per virtual display)
    case phone      // single phone-shaped window
}

/// A configured AVD the engine manages.
public struct Avd: Sendable, Equatable {
    public let name: String
    public let deviceProfile: String
    public init(name: String, deviceProfile: String) {
        self.name = name
        self.deviceProfile = deviceProfile
    }

    public static let phone = Avd(name: "androlon_phone", deviceProfile: "pixel_7")
    public static let desktop = Avd(name: "androlon_desktop", deviceProfile: "10.1in WXGA (Tablet)")
}

/// Presence check for one required tool.
public struct ToolCheck: Sendable {
    public let name: String
    public let path: URL
    public let present: Bool
}

/// Aggregated health snapshot — the Swift equivalent of `apkrun.sh doctor`,
/// reused by the CLI now and the AppKit Settings panel later.
public struct DoctorReport: Sendable {
    public let sdkRoot: URL
    public let api: Int
    public let systemImage: String
    public let tools: [ToolCheck]
    public let avds: [String]
    public let emulatorRunning: Bool
    public let rootStatus: RootStatus
    public let isRootable: Bool
}
