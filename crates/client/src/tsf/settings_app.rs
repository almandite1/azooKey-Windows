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

use windows::{
    Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
    },
    core::{PCWSTR, w},
};

use crate::globals::DllModule;

/// Tauri names the binary after `mainBinaryName` (#97); before that it was
/// `frontend.exe`. Deliberately not looked for under the old name — an install
/// old enough to have it has no menu to launch it from.
pub const SETTINGS_EXE: &str = "Azookey.exe";

/// Where the settings app's own installer records its directory. The settings
/// app is packaged by Tauri's NSIS, which the outer Inno setup chains
/// silently, so this — not our own location — is the authority on where it
/// went. `InstallLocation` is the same value `Installer.iss` reads to chain
/// the uninstaller.
const UNINSTALL_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Azookey");

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

/// The settings app's path, if one of the places it can be has it.
///
/// The registered install directory comes first, and that is not a matter of
/// taste: the chained Tauri installer runs silently and therefore always uses
/// ITS default directory, wherever the user pointed the outer installer. The
/// two agree only in a default install. Falling back to our own directory
/// covers a tree no installer ever touched — a developer's `build\`.
///
/// Never a bare name handed to `CreateProcess`: its search order includes
/// `PATH`, and the launcher prepends the backend directory to `PATH` (see
/// `supervisor.rs::start_process`, which resolves its children for the same
/// reason).
fn locate() -> Option<PathBuf> {
    let candidates = [install_location(), dll_directory()];

    let mut first = None;
    for directory in candidates.into_iter().flatten() {
        let candidate = settings_exe_in(&directory);
        // one GetFileAttributes per candidate, on a menu click; the
        // alternative is starting the wrong path and reporting nothing
        if candidate.is_file() {
            return Some(candidate);
        }
        first = first.or(Some(candidate));
    }

    // Nothing was there to be found. Hand back the first guess anyway and let
    // CreateProcess be the one to say so — the file could have appeared, and
    // its error message is more use in a log than our silence.
    first
}

/// `<directory>\Azookey.exe`.
fn settings_exe_in(directory: &str) -> PathBuf {
    Path::new(unquote(directory)).join(SETTINGS_EXE)
}

/// NSIS writes `InstallLocation` **quoted** (`"C:\Program Files\Azookey"`) and
/// no path API strips that for us — `Installer.iss` runs the same value
/// through Inno's `RemoveQuotes`.
///
/// A trailing separator is left alone: `Path::join` already handles it, and
/// trimming one would turn `C:\` into `C:`, which names the current directory
/// of that drive rather than its root.
fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"').trim()
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

/// `InstallLocation` from the settings app's uninstall key, HKLM then HKCU.
///
/// HKCU is the fallback `Installer.iss` keeps for the same key: installs made
/// before the settings app switched to a perMachine NSIS landed in the
/// elevating administrator's HKCU.
fn install_location() -> Option<String> {
    read_install_location(HKEY_LOCAL_MACHINE).or_else(|| read_install_location(HKEY_CURRENT_USER))
}

fn read_install_location(root: HKEY) -> Option<String> {
    let value = w!("InstallLocation");

    // RegGetValueW counts BYTES, terminator included, and reports back how
    // many it wrote. Ask for the size, then for the value: hard-coding a
    // buffer would truncate a deep install directory into a path that exists
    // nowhere.
    let mut bytes: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            root,
            UNINSTALL_KEY,
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if status.is_err() {
        return None;
    }

    let mut buffer: Vec<u16> = vec![0; (bytes as usize).div_ceil(2)];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            root,
            UNINSTALL_KEY,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status.is_err() {
        return None;
    }

    // trim to what was written, then drop the terminator the count includes
    let written = (bytes as usize / 2).min(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..written]);
    let text = text.trim_end_matches('\0').to_string();

    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The quoting is real and measured: the key holds
    /// `"C:\Program Files\Azookey"`, quotes included. Joining onto that
    /// produces a path with a quote character in it, which exists nowhere.
    #[test]
    fn the_registrys_quoted_directory_becomes_a_usable_path() {
        assert_eq!(
            settings_exe_in(r#""C:\Program Files\Azookey""#),
            Path::new(r"C:\Program Files\Azookey").join(SETTINGS_EXE)
        );
    }

    /// The same path whichever way it is spelled, so the two candidate
    /// directories cannot look different while being the same place.
    #[test]
    fn the_spelling_of_the_directory_makes_no_difference() {
        let plain = settings_exe_in(r"C:\Program Files\Azookey");
        for spelling in [
            r"C:\Program Files\Azookey\",
            r"C:\Program Files\Azookey/",
            r#" "C:\Program Files\Azookey\" "#,
        ] {
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

    /// Reading the registry must not panic, whether or not this machine has
    /// the settings app installed — the value may be absent, empty, or of
    /// another type.
    #[test]
    fn the_registry_lookup_answers_without_panicking() {
        if let Some(directory) = install_location() {
            assert!(
                !directory.trim().is_empty(),
                "an empty directory must be reported as absent, not as a path"
            );
        }
    }
}
