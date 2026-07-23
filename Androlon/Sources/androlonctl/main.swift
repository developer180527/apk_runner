import AndrolonEngine
import Foundation

// Thin CLI over AndrolonEngine, mirroring apkrun.sh so the Swift port can be
// exercised before the AppKit app exists. The AppKit shell (M2) will call the
// same engine APIs.

let config = SdkConfig.makeDefault()
let engine = EmulatorService(config: config)
let adb = AdbService(config: config)

func printDoctor() {
    let r = engine.doctor()
    print("SDK root : \(r.sdkRoot.path)")
    print("API level: \(r.api)   image: \(r.systemImage)")
    for t in r.tools {
        let mark = t.present ? "✓" : "✗"
        print("  \(mark) \(t.name)\(t.present ? " -> \(t.path.path)" : " (run: androlonctl setup)")")
    }
    print("AVDs: \(r.avds.isEmpty ? "(none — run: androlonctl avd)" : r.avds.joined(separator: ", "))")
    print("Emulator running: \(r.emulatorRunning ? "yes" : "no")")
    print("Rootable image  : \(r.isRootable ? "yes" : "no (locked user build)")")
    print("Root status     : \(r.rootStatus.label)   (none | adbd | magisk)")
}

func usage() {
    print("""
    androlonctl — Androlon engine CLI (Android 17 / API 37, arm64, Apple Silicon)

      androlonctl doctor              show what's installed / missing + root status
      androlonctl setup               install SDK packages (SDK cmdline-tools must exist)
      androlonctl avd                 create phone + desktop AVDs
      androlonctl run <apk> [--desktop | --coherence | --phone]
      androlonctl root status         report current root state
      androlonctl stop                shut the emulator down

    Env: APKRUN_API=\(config.api)  APKRUN_IMG_TYPE=\(config.imageType)  APKRUN_SDK=\(config.sdkRoot.path)
    """)
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data(("✗ " + message + "\n").utf8))
    exit(1)
}

let args = Array(CommandLine.arguments.dropFirst())

do {
    switch args.first {
    case "doctor", nil, "-h", "--help", "help":
        if args.first == "doctor" { printDoctor() } else { usage() }

    case "setup":
        print("› Installing SDK packages (\(config.systemImage))…")
        try engine.installPackages()
        print("✓ SDK provisioned at \(config.sdkRoot.path)")

    case "avd":
        try engine.createAvd(.phone)
        try engine.createAvd(.desktop)
        print("✓ AVDs ready: \(engine.listAvds().joined(separator: ", "))")

    case "run":
        guard args.count >= 2 else { fail("usage: androlonctl run <apk> [--desktop|--coherence|--phone]") }
        let apk = URL(filePath: args[1])
        guard FileManager.default.fileExists(atPath: apk.path) else {
            throw EngineError.apkNotFound(apk)
        }
        let mode: WindowMode = args.contains("--coherence") ? .coherence
            : args.contains("--phone") ? .phone : .desktop
        let avd: Avd = (mode == .phone) ? .phone : .desktop
        let log = config.sdkRoot.deletingLastPathComponent().appending(path: ".emulator.log")
        print("› Booting \(avd.name) [\(mode.rawValue)] with GPU backend '\(GfxstreamBackend().name)'…")
        try engine.boot(avd, logFile: log)
        try adb.install(apk)
        let pkg = adb.packageName(of: apk)
        if pkg.isEmpty { throw EngineError.packageUnresolved(apk) }
        guard let component = adb.launchComponent(for: pkg) else {
            throw EngineError.packageUnresolved(apk)
        }
        // Coherence mode would allocate a virtual display per app; v1 uses display 0.
        try adb.launch(component: component, onDisplay: nil)
        print("✓ Launched \(component)")

    case "root":
        switch args.dropFirst().first {
        case "status", nil:
            print("root: \(adb.rootStatus().label)")
        case "adbd":
            try adb.enableAdbdRoot()
            print("✓ adbd root: \(adb.rootStatus().label)")
        default:
            fail("usage: androlonctl root [status|adbd]")
        }

    case "stop":
        engine.stop()
        print("✓ stop signal sent")

    default:
        fail("unknown command: \(args[0])  (try: androlonctl --help)")
    }
} catch let error as EngineError {
    fail(error.description)
} catch let error as ShellError {
    fail(error.description)
} catch {
    fail(error.localizedDescription)
}
