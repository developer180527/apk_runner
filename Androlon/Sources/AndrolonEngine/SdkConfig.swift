import Foundation

/// Locations and versions for a self-contained Android SDK, mirroring the
/// variables at the top of `apkrun.sh`. Everything lives under `sdkRoot` so the
/// engine never touches a system-wide Android install.
public struct SdkConfig: Sendable {
    public var sdkRoot: URL
    public var api: Int
    /// `google_apis` (userdebug, rootable, no Play Store) | `aosp` (userdebug,
    /// lightest) | `google_apis_playstore` (locked user build, NOT rootable).
    public var imageType: String
    public var abi: String
    public var buildTools: String

    public init(
        sdkRoot: URL,
        api: Int = 37,                       // Android 17
        imageType: String = "google_apis",
        abi: String = "arm64-v8a",
        buildTools: String = "37.0.0"
    ) {
        self.sdkRoot = sdkRoot
        self.api = api
        self.imageType = imageType
        self.abi = abi
        self.buildTools = buildTools
    }

    /// Default config: SDK under `./.android-sdk` relative to the current dir,
    /// overridable via `APKRUN_SDK` to stay consistent with the bash prototype.
    public static func makeDefault() -> SdkConfig {
        let env = ProcessInfo.processInfo.environment
        let root: URL
        if let override = env["APKRUN_SDK"] {
            root = URL(filePath: override)
        } else {
            root = URL(filePath: FileManager.default.currentDirectoryPath)
                .appending(path: ".android-sdk")
        }
        let api = env["APKRUN_API"].flatMap(Int.init) ?? 37
        return SdkConfig(
            sdkRoot: root,
            api: api,
            imageType: env["APKRUN_IMG_TYPE"] ?? "google_apis"
        )
    }

    // Package coordinate strings for sdkmanager / avdmanager.
    public var systemImage: String { "system-images;android-\(api);\(imageType);\(abi)" }
    public var platform: String { "platforms;android-\(api)" }

    // Tool directories.
    public var cmdlineToolsBin: URL { sdkRoot.appending(path: "cmdline-tools/latest/bin") }
    public var platformToolsBin: URL { sdkRoot.appending(path: "platform-tools") }
    public var emulatorBin: URL { sdkRoot.appending(path: "emulator") }
    public var buildToolsBin: URL { sdkRoot.appending(path: "build-tools/\(buildTools)") }

    // Individual tools.
    public var adb: URL { platformToolsBin.appending(path: "adb") }
    public var emulator: URL { emulatorBin.appending(path: "emulator") }
    public var sdkmanager: URL { cmdlineToolsBin.appending(path: "sdkmanager") }
    public var avdmanager: URL { cmdlineToolsBin.appending(path: "avdmanager") }
    public var aapt2: URL { buildToolsBin.appending(path: "aapt2") }

    /// The system-image ramdisk rootAVD patches with Magisk.
    public var ramdiskImage: URL {
        sdkRoot.appending(path: "system-images/android-\(api)/\(imageType)/\(abi)/ramdisk.img")
    }

    /// Play-Store images are `user` builds and cannot be rooted.
    public var isRootable: Bool { imageType != "google_apis_playstore" }

    /// All tool dirs, for prepending onto a child process PATH.
    public var toolDirs: [URL] { [platformToolsBin, emulatorBin, cmdlineToolsBin, buildToolsBin] }

    /// Environment a child SDK tool expects.
    public var toolEnv: [String: String] {
        ["ANDROID_SDK_ROOT": sdkRoot.path, "ANDROID_HOME": sdkRoot.path]
    }
}
