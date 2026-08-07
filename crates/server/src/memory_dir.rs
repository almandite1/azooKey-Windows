//! The learning directory: creating it, and keeping other applications out
//! of it.
//!
//! What lives in `%APPDATA%\Azookey\memory` is a record of what the user has
//! typed and confirmed — names, addresses, whatever they write. It is the
//! most sensitive thing this project stores, and by default a file under
//! `%APPDATA%` is readable by every process the user runs.
//!
//! Windows offers no way to isolate two processes of the SAME user at the
//! same integrity level: same-user is the boundary the OS enforces on, and
//! inside it everything is mutually readable. DPAPI does not help — anything
//! the server can decrypt under the user's key, any other process of that
//! user can decrypt too. What IS available is the ELEVATION boundary: the
//! server runs elevated (the launcher's scheduled task asks for
//! `HighestAvailable`, see `installer/Azookey Startup.xml`), so a DACL
//! naming only SYSTEM and Administrators leaves the server able to read and
//! write while the user's ordinary, medium-integrity applications get
//! ACCESS_DENIED.
//!
//! That boundary does not exist on a standard-user install, where
//! `HighestAvailable` is a no-op. There the ACL is skipped rather than
//! applied: applying it would lock out the server itself, and there is no
//! protection to be had at that point anyway. Learning still works; the
//! warning in the log is the honest statement of what it costs.

use std::path::{Path, PathBuf};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            ACL,
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION,
                SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_ELEVATION,
            TOKEN_QUERY, TokenElevation,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{HSTRING, PCWSTR},
};

/// The memory directory's DACL, in SDDL.
///
/// `D:PAI` — a DACL that is Protected (`P`), so the inheritable ACEs
/// `%APPDATA%` grants to the user do NOT flow into it; that inheritance is
/// exactly what would otherwise make the history readable. `AI` marks it as
/// auto-inherit-aware, which is what `SetNamedSecurityInfoW` writes anyway.
///
/// Two ACEs, both `OICI` — OBJECT_INHERIT | CONTAINER_INHERIT — so the files
/// the converter creates inside are covered as well as the directory itself,
/// and `FA` (FILE_ALL_ACCESS):
/// - `SY`, SYSTEM
/// - `BA`, the Administrators group, which is what an elevated server's token
///   carries
///
/// Nothing else. In particular no `BU` (Users), no `WD` (Everyone), no `AU`
/// (Authenticated Users), and no per-user SID — the user's own unelevated
/// processes are precisely who this keeps out.
const MEMORY_DIR_SDDL: &str = "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// Creates `%APPDATA%\Azookey\memory` if it is not there, locks its DACL
/// down, and answers with the path.
///
/// `None` only when there is no `%APPDATA%` to hang it off or the directory
/// could not be created — the engine's own guard turns that into learning
/// staying off, rather than a memory store appearing somewhere arbitrary.
///
/// Called from three places, all of them before something that needs the
/// directory to exist: the startup `LoadConfig`, every `UpdateConfig`, and
/// the reset handler. Cheap and idempotent on purpose, so "the user deleted
/// the folder by hand" heals on the next save rather than needing a restart.
///
/// A failure to APPLY the ACL is logged and otherwise ignored. The protection
/// is best-effort by nature (it does not exist at all without elevation), and
/// an IME that stops learning because a security call returned an error would
/// be trading a real feature for nothing.
pub(crate) fn ensure_secured_memory_dir() -> Option<PathBuf> {
    let directory = shared::learning_memory_dir()?;
    if let Err(e) = std::fs::create_dir_all(&directory) {
        tracing::warn!(
            "failed to create the learning directory {}: {e}; learning stays off",
            directory.display()
        );
        return None;
    }
    protect(&directory);
    Some(directory)
}

/// Applies [`MEMORY_DIR_SDDL`] to `directory`, or explains in the log why it
/// did not.
fn protect(directory: &Path) {
    match is_elevated() {
        Ok(true) => {}
        Ok(false) => {
            // Not a failure — it is the standard-user install, and the honest
            // thing is to say what the user is getting rather than to apply
            // an ACL that would lock the server out of its own directory.
            tracing::warn!(
                "this process is not elevated, so {} is left with the \
                 permissions it inherited: the learning history is readable by \
                 the applications this user runs. Same-user isolation is not \
                 something Windows offers without the elevation boundary.",
                directory.display()
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                "could not determine whether this process is elevated ({e}); \
                 leaving the learning directory's permissions alone"
            );
            return;
        }
    }

    if let Err(e) = apply_dacl(directory) {
        tracing::warn!(
            "failed to restrict the permissions on {}: {e}; the learning \
             history may be readable by other applications this user runs",
            directory.display()
        );
    }
}

/// Whether this process's token is elevated.
fn is_elevated() -> windows::core::Result<bool> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;

        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut std::ffi::c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );

        let _ = CloseHandle(token);
        queried?;
        Ok(elevation.TokenIsElevated != 0)
    }
}

/// Parses [`MEMORY_DIR_SDDL`] and writes its DACL onto the directory,
/// protected so `%APPDATA%`'s inheritable ACEs do not come back.
fn apply_dacl(directory: &Path) -> windows::core::Result<()> {
    let sddl = HSTRING::from(MEMORY_DIR_SDDL);
    let path = HSTRING::from(directory.as_os_str());

    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION,
            &mut descriptor,
            None,
        )?;

        // The DACL points INTO the descriptor, so the LocalFree below must
        // not happen until SetNamedSecurityInfoW has copied it.
        let result = (|| {
            let mut present = windows::core::BOOL::default();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut defaulted = windows::core::BOOL::default();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)?;

            SetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            )
            .ok()
        })();

        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}

/// The DACL is the whole protection, and Windows only ever looks at it when
/// the directory is created or a handle is opened — so a typo here shows up
/// as "learning still works", with the hardening silently gone. Same
/// reasoning, and the same shape, as the pipe SDDL tests in lib.rs.
#[cfg(test)]
mod tests {
    use super::MEMORY_DIR_SDDL;

    #[test]
    fn only_system_and_administrators_are_granted_anything() {
        assert!(MEMORY_DIR_SDDL.contains(";;;SY)"), "{MEMORY_DIR_SDDL}");
        assert!(MEMORY_DIR_SDDL.contains(";;;BA)"), "{MEMORY_DIR_SDDL}");
        assert_eq!(
            MEMORY_DIR_SDDL.matches("(A;").count(),
            2,
            "exactly two allow ACEs, and no more: {MEMORY_DIR_SDDL}"
        );
    }

    /// The principals that would undo the whole thing. `BU` and `AU` cover
    /// the user's own unelevated processes, which are what this keeps out;
    /// `WD` covers everyone.
    #[test]
    fn the_users_groups_are_not_granted_anything() {
        for principal in [";;;BU)", ";;;WD)", ";;;AU)", ";;;IU)"] {
            assert!(
                !MEMORY_DIR_SDDL.contains(principal),
                "{principal} must not appear: {MEMORY_DIR_SDDL}"
            );
        }
    }

    /// Without `P` the DACL inherits from `%APPDATA%`, which grants the user
    /// full control — the ACEs above would be added to that rather than
    /// replacing it, and the directory would stay readable.
    #[test]
    fn the_dacl_is_protected_from_inheritance() {
        assert!(
            MEMORY_DIR_SDDL.starts_with("D:PAI"),
            "the DACL must be protected: {MEMORY_DIR_SDDL}"
        );
    }

    /// The files inside are the point, not the directory: without `OI` the
    /// converter's `memory.louds` would be created with inherited
    /// permissions.
    #[test]
    fn both_aces_are_inherited_by_the_files_inside() {
        assert_eq!(
            MEMORY_DIR_SDDL.matches("OICI").count(),
            2,
            "every ACE must carry OBJECT_INHERIT and CONTAINER_INHERIT: \
             {MEMORY_DIR_SDDL}"
        );
    }
}
