import Foundation

public enum EngineError: Error, CustomStringConvertible {
    case sdkMissing(tool: String)
    case notRootable(String)
    case ramdiskMissing(URL)
    case apkNotFound(URL)
    case packageUnresolved(URL)
    case backendUnimplemented(String)

    public var description: String {
        switch self {
        case let .sdkMissing(tool):
            return "\(tool) not found — run `androlonctl setup` first."
        case let .notRootable(type):
            return "image '\(type)' is a locked user build — use google_apis or aosp for root."
        case let .ramdiskMissing(url):
            return "system-image ramdisk not found at \(url.path) — run setup."
        case let .apkNotFound(url):
            return "APK not found: \(url.path)"
        case let .packageUnresolved(url):
            return "could not determine package name for \(url.lastPathComponent)"
        case let .backendUnimplemented(name):
            return "GPU backend '\(name)' is not implemented yet."
        }
    }
}
