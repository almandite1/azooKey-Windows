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

#[cfg(test)]
mod tests {
    use super::{MAX_LOG_FILES, prune_old_logs};
    use std::path::PathBuf;

    /// A throwaway log directory holding `count` timestamped server logs,
    /// newest last. Named per test so the (parallel) test threads cannot
    /// prune each other's fixtures.
    struct LogDir(PathBuf);

    impl LogDir {
        fn with_logs(tag: &str, count: usize) -> Self {
            let dir =
                std::env::temp_dir().join(format!("azk-server-prune-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the fixture directory");
            for i in 1..=count {
                let name = format!("server-202601{i:02}-000000-{i}.log");
                std::fs::write(dir.join(name), "x").expect("write a fixture log");
            }
            LogDir(dir)
        }

        fn server_logs(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .expect("read the fixture directory")
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("server-"))
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for LogDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The launcher has the same rotation (and the same test); the server's
    /// copy had none, so a divergence would have gone unnoticed until a log
    /// directory grew without bound.
    #[test]
    fn prune_keeps_only_the_newest_logs() {
        let dir = LogDir::with_logs("many", 12);
        std::fs::write(dir.0.join("unrelated.txt"), "x").expect("write a non-log file");
        std::fs::write(dir.0.join("launcher-20260101-000000-1.log"), "x")
            .expect("write another component's log");

        prune_old_logs(&dir.0);

        let remaining = dir.server_logs();
        // room is left for the log this session is about to create:
        // 12 -> MAX_LOG_FILES - 1
        assert_eq!(remaining.len(), MAX_LOG_FILES - 1);
        // the oldest go first; names sort chronologically
        assert!(remaining[0].contains("20260104"), "kept {remaining:?}");
        // only this component's logs are rotated
        assert!(dir.0.join("unrelated.txt").exists());
        assert!(dir.0.join("launcher-20260101-000000-1.log").exists());
    }

    /// `>=` and the `+ 1`: at exactly the cap one file still goes, because
    /// the caller is about to add one.
    #[test]
    fn a_directory_exactly_at_the_cap_loses_its_oldest_log() {
        let dir = LogDir::with_logs("at-cap", MAX_LOG_FILES);

        prune_old_logs(&dir.0);

        let remaining = dir.server_logs();
        assert_eq!(remaining.len(), MAX_LOG_FILES - 1);
        assert!(remaining[0].contains("20260102"), "kept {remaining:?}");
    }

    #[test]
    fn a_directory_below_the_cap_is_left_alone() {
        let dir = LogDir::with_logs("below-cap", MAX_LOG_FILES - 1);

        prune_old_logs(&dir.0);

        assert_eq!(dir.server_logs().len(), MAX_LOG_FILES - 1);
    }

    /// Logging is best-effort: a log directory that does not exist yet (or
    /// was deleted underneath us) must not take the server's startup with it.
    #[test]
    fn a_missing_directory_is_not_an_error() {
        let missing =
            std::env::temp_dir().join(format!("azk-server-prune-{}-none", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);

        prune_old_logs(&missing);
    }
}
