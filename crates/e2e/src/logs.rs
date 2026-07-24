//! Scanning the engine's logs for signs that something went wrong quietly.
//!
//! **The TIP is not covered.** In release builds `crates/client/src/trace.rs`
//! writes nothing to disk — the DLL runs inside every application, so it
//! forwards to `OutputDebugStringW` instead, where only a debugger sees it.
//! So the checklist's "client ログに panic / Activate failed が無い" cannot be
//! answered from files at all; what remains is the server, ui and launcher
//! logs, which is still where an engine-side panic would land.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

/// Markers that always mean a defect. Deliberately short: the fault scenarios
/// kill the engine on purpose, so launcher's own "crashed"/"restarting" lines
/// are expected and must NOT be flagged — only an actual panic is unambiguous.
const MARKERS: [&str; 2] = ["panicked at", "Activate failed"];

/// Where the engine keeps its logs.
pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Azookey").join("logs"))
}

/// A marker found in a log.
#[derive(Debug)]
pub struct Hit {
    pub file: String,
    pub line: String,
}

/// Every marker occurrence in the log directory, and how many files were read.
pub fn scan() -> Result<(Vec<Hit>, usize)> {
    let dir = log_dir().context("LOCALAPPDATA is not set")?;
    if !dir.is_dir() {
        // no directory means nothing has logged yet, which is not a failure
        return Ok((Vec::new(), 0));
    }

    let mut hits = Vec::new();
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).with_context(|| format!("cannot read {dir:?}"))? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        files += 1;

        // lossy: a log truncated mid-character must not abort the scan
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                // a live process holds its own log open; that is not a defect
                eprintln!("   (skipped {}: {e})", path.display());
                continue;
            }
        };

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for line in text.lines() {
            if MARKERS.iter().any(|marker| line.contains(marker)) {
                hits.push(Hit {
                    file: name.clone(),
                    line: line.trim().to_string(),
                });
            }
        }
    }

    Ok((hits, files))
}
