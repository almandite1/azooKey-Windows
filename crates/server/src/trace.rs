//! Logging setup: tracing to stdout (captured by the launcher) plus a
//! per-process file under %LOCALAPPDATA%\Azookey\logs, so a manually
//! started server keeps its logs too — previously bare println output was
//! lost whenever the server was not spawned by the launcher.

use std::sync::Mutex;

use shared::logs::{log_dir, prune_old_logs};
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt as _};

/// Distinguishes this component's logs from the launcher's and the client's
/// in the shared directory — and so decides which ones rotation may delete.
const LOG_PREFIX: &str = "server-";

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

pub(crate) fn setup_logger() {
    // stdout always; the file only when the log dir is writable — losing
    // file logging must never keep the server from starting
    let writer = match open_log_file() {
        Some(file) => BoxMakeWriter::new(std::io::stdout.and(Mutex::new(file))),
        None => BoxMakeWriter::new(std::io::stdout),
    };
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(writer)
        .try_init();
}
