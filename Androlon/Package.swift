// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "Androlon",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "AndrolonEngine", targets: ["AndrolonEngine"]),
        .executable(name: "androlonctl", targets: ["androlonctl"]),
    ],
    targets: [
        // The engine: subprocess-orchestration port of apkrun.sh (setup → AVD →
        // boot → install → launch → root). No AppKit deps so it stays testable.
        .target(name: "AndrolonEngine"),
        // Thin CLI front-end that exercises the engine, mirroring apkrun.sh.
        .executableTarget(name: "androlonctl", dependencies: ["AndrolonEngine"]),
    ]
)
