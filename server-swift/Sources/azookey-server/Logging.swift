import Foundation

/// The engine's log output.
///
/// Deliberately still `print` to stdout: the Rust server captures stdout and
/// tees it into `%LOCALAPPDATA%\Azookey\logs\server-*.log` (crates/server's
/// trace.rs), which is where an engine-side message has to end up. What this
/// adds is a level and a tag, so the two bare `print` calls the engine had —
/// one about settings, one about the zenzai model — are recognisable in a log
/// that is otherwise all `tracing` output from the Rust side.
enum EngineLogLevel: String {
    case info = "INFO"
    case error = "ERROR"
}

func enginePrint(level: EngineLogLevel, _ message: String) {
    print("[engine \(level.rawValue)] \(message)")
}
