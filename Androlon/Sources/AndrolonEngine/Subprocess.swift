import Foundation

/// Result of running a child process. stdout and stderr are merged into
/// `output` via a single pipe — one reader means no pipe-buffer deadlock and no
/// concurrency gymnastics, which matters for chatty tools like sdkmanager.
public struct CommandResult: Sendable {
    public let status: Int32
    public let output: String
    public var ok: Bool { status == 0 }
    public var trimmed: String { output.trimmingCharacters(in: .whitespacesAndNewlines) }
}

public enum ShellError: Error, CustomStringConvertible {
    case launchFailed(tool: String, underlying: String)
    case nonZero(tool: String, result: CommandResult)

    public var description: String {
        switch self {
        case let .launchFailed(tool, underlying):
            return "failed to launch \(tool): \(underlying)"
        case let .nonZero(tool, result):
            return "\(tool) exited \(result.status):\n\(result.trimmed)"
        }
    }
}

/// Run `executable args`, blocking until exit. `extraPATH` is prepended to the
/// child PATH so bundled tools (adb/emulator) resolve; `env` overrides vars.
@discardableResult
public func runCommand(
    _ executable: URL,
    _ args: [String],
    extraPATH: [URL] = [],
    env: [String: String] = [:]
) throws -> CommandResult {
    let proc = Process()
    proc.executableURL = executable
    proc.arguments = args

    var environment = ProcessInfo.processInfo.environment
    if !extraPATH.isEmpty {
        let prefix = extraPATH.map(\.path).joined(separator: ":")
        environment["PATH"] = prefix + ":" + (environment["PATH"] ?? "")
    }
    for (key, value) in env { environment[key] = value }
    proc.environment = environment

    let pipe = Pipe()
    proc.standardOutput = pipe
    proc.standardError = pipe

    do {
        try proc.run()
    } catch {
        throw ShellError.launchFailed(tool: executable.lastPathComponent,
                                      underlying: error.localizedDescription)
    }

    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    proc.waitUntilExit()
    return CommandResult(status: proc.terminationStatus,
                         output: String(decoding: data, as: UTF8.self))
}

/// Launch a long-running process (the emulator) detached, returning it so the
/// caller can track/terminate. Output is redirected to `logFile`.
public func launchDetached(
    _ executable: URL,
    _ args: [String],
    extraPATH: [URL] = [],
    env: [String: String] = [:],
    logFile: URL
) throws -> Process {
    FileManager.default.createFile(atPath: logFile.path, contents: nil)
    guard let handle = try? FileHandle(forWritingTo: logFile) else {
        throw ShellError.launchFailed(tool: executable.lastPathComponent,
                                      underlying: "cannot open log \(logFile.path)")
    }
    let proc = Process()
    proc.executableURL = executable
    proc.arguments = args
    var environment = ProcessInfo.processInfo.environment
    if !extraPATH.isEmpty {
        let prefix = extraPATH.map(\.path).joined(separator: ":")
        environment["PATH"] = prefix + ":" + (environment["PATH"] ?? "")
    }
    for (key, value) in env { environment[key] = value }
    proc.environment = environment
    proc.standardOutput = handle
    proc.standardError = handle
    do {
        try proc.run()
    } catch {
        throw ShellError.launchFailed(tool: executable.lastPathComponent,
                                      underlying: error.localizedDescription)
    }
    return proc
}
