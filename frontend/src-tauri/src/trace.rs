//! Logging for the settings app: a per-process file under
//! `%LOCALAPPDATA%\Azookey\logs`, next to the other components'.
//!
//! This binary has no console — it is built for the Windows GUI subsystem —
//! so anything written to stdout or stderr goes nowhere at all. That mattered
//! more than it sounds: the settings diagnostics in `shared` are the only
//! notice a user gets that their settings.json could not be read and was
//! moved aside, and this is the process most likely to be the one that finds
//! out. They went to `eprintln!`, i.e. into the void, in exactly the process
//! where they were needed.
//!
//! Modelled on `crates/server/src/trace.rs`, minus the stdout half for the
//! reason above.

use shared::logs::{log_dir, prune_old_logs};
use std::sync::Mutex;

/// Keeps this component's logs to itself, and so decides which ones rotation
/// may delete.
const LOG_PREFIX: &str = "settings-";

fn open_log_file() -> Option<std::fs::File> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    prune_old_logs(&dir, LOG_PREFIX);
    let name = format!(
        "{LOG_PREFIX}{}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    std::fs::File::create(dir.join(name)).ok()
}

/// Installs the file logger. Best-effort: losing logging must never be what
/// stops the settings app from opening.
pub(crate) fn setup_logger() {
    let Some(file) = open_log_file() else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(Mutex::new(file))
        .try_init();
    log_panics();
}

/// Sends panics to the log file instead of to the void.
///
/// A panic in this process is invisible three times over, and the three
/// compound: the default hook writes to stderr, which a GUI-subsystem binary
/// does not have; a panic inside a `#[tauri::command]` kills the spawned task
/// rather than returning, so the `invoke` promise on the other side never
/// settles; and a promise that never settles renders as nothing at all —
/// no error, no toast, just a control that stays disabled.
///
/// That is not hypothetical. Every server call the settings app made
/// panicked for nine days on a runtime-in-runtime, and the only symptom
/// anybody could see was a button that did nothing (see `ipc.rs`). One line
/// in a log would have named it immediately.
///
/// The previous hook still runs afterwards, so a debug build keeps printing
/// to the console it does have.
fn log_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // force_capture, not capture: RUST_BACKTRACE is not set for a user's
        // double-click, and a panic with no location is most of the value
        // gone.
        tracing::error!(
            "PANIC: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Somewhere for the subscriber to write that the test can read back.
    #[derive(Clone)]
    struct Captured(Arc<StdMutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The hook is six lines, which is exactly the sort of thing that gets
    /// shipped unverified — and the whole reason it exists is that an
    /// unverified six lines cost nine days. If it silently failed to emit,
    /// the log would look the same as a process that never panicked, which is
    /// the state it was written to distinguish from.
    #[test]
    fn a_panic_reaches_the_subscriber() {
        let captured = Captured(Arc::new(StdMutex::new(Vec::new())));

        super::log_panics();
        {
            let writer = captured.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            // scoped to this thread, and the hook runs on the thread that
            // panicked — so a global subscriber, which the test binary may
            // already have, is neither needed nor disturbed
            let _guard = tracing::subscriber::set_default(subscriber);
            let _ = std::panic::catch_unwind(|| panic!("a panic on purpose"));
        }
        // back to the default hook, so a later #[should_panic] elsewhere in
        // this binary is not writing into a dropped subscriber
        let _ = std::panic::take_hook();

        let log = String::from_utf8_lossy(
            &captured
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned();

        assert!(log.contains("PANIC:"), "nothing was logged: {log}");
        assert!(
            log.contains("a panic on purpose"),
            "the message has to survive, or the log names no cause: {log}"
        );
        assert!(
            log.contains("trace.rs"),
            "and the backtrace, or it names no place: {log}"
        );
    }
}
