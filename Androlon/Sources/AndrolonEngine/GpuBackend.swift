import Foundation

/// Abstraction over the guest→host GPU virtualization path. The whole point of
/// the "phased" gaming decision: v1 ships `GfxstreamBackend` (stock emulator),
/// and a future `VenusBackend` (crosvm/libkrun + Venus→MoltenVK) slots in behind
/// this protocol with no change to DisplayService or the app layer.
public protocol GpuBackend: Sendable {
    var name: String { get }
    var isImplemented: Bool { get }
    /// Extra `emulator` args this backend needs (v1). A VM-based backend will
    /// instead expose its own launch path; the app layer only knows this protocol.
    func emulatorArgs() -> [String]
}

/// v1: Google's emulator GPU path — guest Vulkan/GLES → Metal via ANGLE/gfxstream.
/// Smooth for most 2D/mid-tier 3D titles on Apple Silicon.
public struct GfxstreamBackend: GpuBackend {
    public let name = "gfxstream"
    public let isImplemented = true
    public init() {}
    public func emulatorArgs() -> [String] {
        // `-gpu auto` selects host (Metal-backed) rendering; gfxstream is the
        // default guest transport on recent system images.
        ["-gpu", "auto"]
    }
}

/// Roadmap (M5): near-native Vulkan via virtio-gpu Venus → MoltenVK. Placeholder
/// so call sites and tests can already reference it.
public struct VenusBackend: GpuBackend {
    public let name = "venus"
    public let isImplemented = false
    public init() {}
    public func emulatorArgs() -> [String] { [] }
}
