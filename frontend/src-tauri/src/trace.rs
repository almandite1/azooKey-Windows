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
}
