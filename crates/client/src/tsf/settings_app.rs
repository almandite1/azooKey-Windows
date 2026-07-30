//! Starting the settings app — what the language-bar menu's 設定 item does
//! (issue #98).
//!
//! This is the first place `crates/client` starts a process, and it does it on
//! the host application's UI thread, inside a TSF callback. So it spawns and
//! returns: nothing waits for the child, and a failure to start is a `warn!`
//! rather than an error, because a menu click must not report a failure back
//! into a text application.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::globals::DllModule;

/// Tauri names the binary after `mainBinaryName` (#97); before that it was
/// `frontend.exe`. Deliberately not looked for under the old name — an install
/// old enough to have it has no menu to launch it from.
pub const SETTINGS_EXE: &str = "Azookey.exe";

/// Starts the settings app, or logs why it could not.
pub fn open() {
    let Some(path) = locate() else {
        tracing::warn!(
            "could not work out where {SETTINGS_EXE} is; the settings app was not started"
        );
        return;
    };

    // Handles closed, not inherited from a text application that has no
    // console anyway. The Child is dropped immediately: this thread is the
    // host's UI thread and must not wait for anything, and the settings app is
    // meant to outlive the click that opened it.
    match Command::new(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_child) => tracing::info!("started the settings app: {}", path.display()),
        Err(error) => tracing::warn!("could not start {}: {error}", path.display()),
    }
}

/// The settings app's path: next to this DLL.
///
/// Setup puts both in `{app}` — the settings app used to be placed by a Tauri
/// NSIS chained from the Inno installer, which could choose a different
/// directory and had to be asked where via its uninstall key. That chain is
/// gone (#104): one installer places both files, in one directory, in one run,
/// so there is no second location left to consult.
///
/// Never a bare name handed to `CreateProcess`: its search order includes
/// `PATH`, and the launcher prepends the backend directory to `PATH` (see
/// `supervisor.rs::start_process`, which resolves its children for the same
/// reason).
fn locate() -> Option<PathBuf> {
    // Handed back whether or not the file is there: it could appear between
    // now and the spawn, and CreateProcess's error message is more use in a
    // log than our silence.
    Some(settings_exe_in(&dll_directory()?))
}

/// `<directory>\Azookey.exe`.
///
/// A trailing separator is left alone: `Path::join` already handles it, and
/// trimming one would turn `C:\` into `C:`, which names the current directory
/// of that drive rather than its root.
fn settings_exe_in(directory: &str) -> PathBuf {
    Path::new(directory).join(SETTINGS_EXE)
}

/// The directory our own DLL was loaded from.
fn dll_directory() -> Option<String> {
    let path = match DllModule::get_path() {
        Ok(path) => path,
        // before DllMain has run (unit tests), or a module handle we cannot
        // resolve
        Err(error) => {
            tracing::warn!("could not resolve our own module path: {error:?}");
            return None;
        }
    };

    Path::new(&path)
        .parent()
        .map(|directory| directory.to_string_lossy().into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The same path whichever way the directory is spelled — a trailing
    /// separator is what `GetModuleFileName`'s parent will and will not have,
    /// depending on where the DLL sits.
    #[test]
    fn the_spelling_of_the_directory_makes_no_difference() {
        let plain = settings_exe_in(r"C:\Program Files\Azookey");
        for spelling in [r"C:\Program Files\Azookey\", r"C:\Program Files\Azookey/"] {
            assert_eq!(settings_exe_in(spelling), plain, "for {spelling}");
        }
    }

    /// A drive root must survive as one: `C:` alone names the current
    /// directory of that drive, not its root, so the separator cannot be
    /// trimmed away.
    #[test]
    fn a_drive_root_is_still_a_drive_root() {
        assert_eq!(
            settings_exe_in(r"C:\"),
            Path::new(r"C:\").join(SETTINGS_EXE)
        );
    }

    /// Whatever it resolves to, it ends in the name the bundler actually
    /// produces — the #97 mistake, which was one name in the config and
    /// another on disk.
    #[test]
    fn the_name_that_is_started_is_the_one_that_ships() {
        assert_eq!(SETTINGS_EXE, "Azookey.exe");
        assert!(
            settings_exe_in(r"C:\anywhere").ends_with(SETTINGS_EXE),
            "the resolved path must name the settings app"
        );
    }

    /// Setup places the settings app in the same directory as the TIP, so
    /// nothing outside this module decides where it is. If that ever stops
    /// being true, `locate` is where it has to be taught otherwise.
    #[test]
    fn the_settings_app_is_looked_for_beside_the_dll() {
        assert_eq!(
            settings_exe_in(r"C:\Program Files\Azookey"),
            Path::new(r"C:\Program Files\Azookey").join(SETTINGS_EXE),
            "the only candidate directory is the one the DLL was loaded from"
        );
    }
}
