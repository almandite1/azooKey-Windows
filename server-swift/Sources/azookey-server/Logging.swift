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
    // Without this nothing above is ever read. `print` goes through C stdio,
    // and stdout here is the pipe the launcher pumps, not a terminal — so it
    // is fully buffered, and the Windows CRT has no line-buffered mode to ask
    // for (_IOLBF behaves as _IOFBF). Messages sat in a 4 KB buffer until it
    // filled or the process exited, which is why no [engine ...] line had ever
    // reached a log file. llama.cpp's output arrives only because it writes to
    // stderr, and the Rust side's because Rust buffers stdout by line itself.
    // Flushing per call is free at the rate this is called.
    fflush(stdout)
}
