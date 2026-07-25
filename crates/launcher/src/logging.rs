//! The launcher's log file.
//!
//! It normally runs headless from the logon scheduled task, so console output
//! is lost — everything is also teed into
//! `%LOCALAPPDATA%\Azookey\logs\launcher-<timestamp>-<pid>.log`. That file is
//! the only record of server crashes, watchdog kills and restarts in the
//! field.

use std::io::Write as _;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Distinguishes this component's logs from the server's and the client's in
/// the shared directory — and so decides which ones rotation may delete. The
/// retention policy itself is `shared::logs`.
pub(crate) const LOG_PREFIX: &str = "launcher-";

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub(crate) fn init_log_file() {
    // %LOCALAPPDATA% first, then the temp directory. The fallback is what
    // keeps the failure path observable: a launcher whose log directory is
    // unusable used to run with no record at all, so a missing log could not
    // be read as "it never started" (issue #79). A log in the wrong place is
    // strictly better than no log.
    let localappdata = shared::logs::log_dir().filter(|dir| std::fs::create_dir_all(dir).is_ok());
    let fallback = std::env::temp_dir().join("Azookey-logs");
    let Some(dir) = localappdata.or_else(|| {
        std::fs::create_dir_all(&fallback)
            .is_ok()
            .then_some(fallback)
    }) else {
        return;
    };

    shared::logs::prune_old_logs(&dir, LOG_PREFIX);

    let name = format!(
        "{LOG_PREFIX}{}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    if let Ok(file) = std::fs::File::create(dir.join(name)) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
}

fn log_to_file(line: &str) {
    if let Some(file) = LOG_FILE.get() {
        let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(
            file,
            "[{}] {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            line
        );
    }
}

pub(crate) fn log_info(line: &str) {
    println!("{line}");
    log_to_file(line);
}

pub(crate) fn log_err(line: &str) {
    eprintln!("{line}");
    log_to_file(line);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rotation itself is tested in `shared::logs`; what belongs here is
    /// that the launcher asks for ITS OWN prefix. A wrong one would rotate
    /// away another component's logs and never its own.
    #[test]
    fn the_launcher_rotates_only_launcher_logs() {
        let dir = std::env::temp_dir().join(format!("azk-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for day in 1..=12 {
            std::fs::write(
                dir.join(format!("{LOG_PREFIX}202601{day:02}-000000-1.log")),
                "x",
            )
            .unwrap();
            std::fs::write(dir.join(format!("server-202601{day:02}-000000-1.log")), "x").unwrap();
        }

        shared::logs::prune_old_logs(&dir, LOG_PREFIX);

        let names = |prefix: &str| {
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(prefix))
                .collect();
            names.sort();
            names
        };

        let remaining = names(LOG_PREFIX);
        // room is left for the new session's file: 12 -> MAX_LOG_FILES - 1
        assert_eq!(remaining.len(), shared::logs::MAX_LOG_FILES - 1);
        assert!(remaining[0].contains("20260104"), "kept {remaining:?}");
        assert_eq!(names("server-").len(), 12, "another component's logs stay");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
