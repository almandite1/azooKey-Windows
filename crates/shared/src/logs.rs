//! Where the components write their logs, and how many they keep.
//!
//! `%LOCALAPPDATA%\Azookey\logs` was hand-assembled in four crates (server,
//! client, launcher, e2e) and the rotation was a near-verbatim copy in two of
//! them, off-by-one trick and cap included. Anything that reads those logs —
//! the E2E scan, a bug report — depends on all of them agreeing, so the
//! directory and the retention policy live here.

use std::path::{Path, PathBuf};

/// How many logs a component keeps, itself included.
pub const MAX_LOG_FILES: usize = 10;

/// `%LOCALAPPDATA%\Azookey\logs`, or `None` when `LOCALAPPDATA` is unset or
/// empty (see [`crate::local_data_root`]). The directory is not created here:
/// only the writers do that, and only the readers care whether it exists yet.
pub fn log_dir() -> Option<PathBuf> {
    crate::local_data_root().map(|root| root.join("logs"))
}

/// Deletes the oldest `<prefix>-*.log` files in `dir` until there is room for
/// one more, so a component's logs cannot grow without bound.
///
/// `prefix` keeps each component to its own files: `server-` must not rotate
/// away `launcher-`, and neither may touch anything that is not a log.
///
/// Best-effort throughout. A directory that cannot be read (it does not exist
/// yet, it was deleted underneath us) is not an error — logging must never be
/// what stops a component from starting.
pub fn prune_old_logs(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".log"))
        })
        .collect();
    // timestamped names sort chronologically
    logs.sort();
    // `>=` and the `+ 1`: at exactly the cap one file still goes, because the
    // caller is about to create one.
    if logs.len() >= MAX_LOG_FILES {
        for old in &logs[..logs.len() + 1 - MAX_LOG_FILES] {
            let _ = std::fs::remove_file(old);
        }
    }
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
            let dir = std::env::temp_dir().join(format!("azk-prune-{}-{tag}", std::process::id()));
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

    #[test]
    fn prune_keeps_only_the_newest_logs() {
        let dir = LogDir::with_logs("many", 12);
        std::fs::write(dir.0.join("unrelated.txt"), "x").expect("write a non-log file");
        std::fs::write(dir.0.join("launcher-20260101-000000-1.log"), "x")
            .expect("write another component's log");

        prune_old_logs(&dir.0, "server-");

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

        prune_old_logs(&dir.0, "server-");

        let remaining = dir.server_logs();
        assert_eq!(remaining.len(), MAX_LOG_FILES - 1);
        assert!(remaining[0].contains("20260102"), "kept {remaining:?}");
    }

    #[test]
    fn a_directory_below_the_cap_is_left_alone() {
        let dir = LogDir::with_logs("below-cap", MAX_LOG_FILES - 1);

        prune_old_logs(&dir.0, "server-");

        assert_eq!(dir.server_logs().len(), MAX_LOG_FILES - 1);
    }

    /// The prefix is what keeps components out of each other's files: a
    /// launcher rotation must leave every server log where it is.
    #[test]
    fn another_components_prefix_rotates_nothing_of_ours() {
        let dir = LogDir::with_logs("other-prefix", 12);

        prune_old_logs(&dir.0, "launcher-");

        assert_eq!(dir.server_logs().len(), 12);
    }

    /// Logging is best-effort: a log directory that does not exist yet (or
    /// was deleted underneath us) must not take a component's startup with it.
    #[test]
    fn a_missing_directory_is_not_an_error() {
        let missing = std::env::temp_dir().join(format!("azk-prune-{}-none", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);

        prune_old_logs(&missing, "server-");
    }
}
