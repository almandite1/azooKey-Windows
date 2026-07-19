//! Logging setup: tracing to stdout (captured by the launcher) plus a
//! per-process file under %LOCALAPPDATA%\Azookey\logs, so a manually
//! started server keeps its logs too — previously bare println output was
//! lost whenever the server was not spawned by the launcher.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt as _};

/// Same cap as the launcher's log rotation.
const MAX_LOG_FILES: usize = 10;

fn log_folder() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(Path::new(&base).join("Azookey").join("logs"))
}

fn prune_old_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("server-") && n.ends_with(".log"))
        })
        .collect();
    // timestamped names sort chronologically
    logs.sort();
    if logs.len() >= MAX_LOG_FILES {
        for old in &logs[..logs.len() + 1 - MAX_LOG_FILES] {
            let _ = std::fs::remove_file(old);
        }
    }
}

fn open_log_file() -> Option<std::fs::File> {
    let dir = log_folder()?;
    std::fs::create_dir_all(&dir).ok()?;
    prune_old_logs(&dir);
    let name = format!(
        "server-{}-{}.log",
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
