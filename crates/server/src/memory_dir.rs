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
//!
//! One carve-out, deliberate: the user can DELETE this directory even though
//! they cannot read it. Without it the folder would be undeletable from an
//! ordinary session — the installer does not touch `%APPDATA%\Azookey`, so it
//! would outlive an uninstall with nothing but an elevated shell able to
//! remove it. See [`MEMORY_DIR_SDDL`] for what that costs and what it does
//! not.

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
/// The first two ACEs are the protection. Both `OICI` — OBJECT_INHERIT |
/// CONTAINER_INHERIT, so the files the converter creates inside are covered
/// as well as the directory itself — and both `FA` (FILE_ALL_ACCESS):
/// - `SY`, SYSTEM
/// - `BA`, the Administrators group, which is what an elevated server's token
///   carries
///
/// The last two let `BU` (Users) — the user's own unelevated processes — throw
/// the directory away without ever reading it. Not one ACE granting `SD`
/// (DELETE) and no more, which is the obvious shape and does not work:
/// deleting a folder means enumerating it first, and enumeration is
/// FILE_LIST_DIRECTORY. Without that bit `Remove-Item -Recurse` and Explorer
/// both fail before they get to the delete.
///
/// So, split by what each right MEANS on each kind of object:
/// - `(A;CI;0x1101c1;;;BU)` — the directory, and any subdirectory.
///   `CI` and NOT `OI` is the whole point: bit `0x1` is FILE_LIST_DIRECTORY
///   on a container and FILE_READ_DATA on a file, so an object-inheritable
///   copy of this ACE would hand the user's processes the history itself.
/// - `(A;OIIO;0x110180;;;BU)` — files only (`IO`, inherit-only, keeps it off
///   the directory). No `0x1`, so no FILE_READ_DATA.
///
/// The masks, bit by bit, because every one of them was needed to make an
/// ordinary `Remove-Item -Recurse -Force` work and none of them reads a byte:
///
/// | bit | on the directory | on a file |
/// |---|---|---|
/// | `0x00000001` | FILE_LIST_DIRECTORY — enumerate, to find what to delete | not granted |
/// | `0x00000040` | FILE_DELETE_CHILD — delete the files without rights on them | — |
/// | `0x00000080` | FILE_READ_ATTRIBUTES | same; the listing already showed these |
/// | `0x00000100` | FILE_WRITE_ATTRIBUTES | same — `-Force` clears ReadOnly before deleting, and fails here without it |
/// | `0x00010000` | DELETE — remove the folder once empty | DELETE — Explorer renames into the Recycle Bin, which needs it on the file |
/// | `0x00100000` | SYNCHRONIZE — every synchronous handle open wants it | same |
///
/// What this costs: an unelevated process can now see the NAMES, sizes and
/// timestamps of the files inside, and can destroy or backdate them. The
/// names are fixed (`memory.louds` and friends) and the directory's own
/// timestamp was already visible from `%APPDATA%\Azookey`, which is the
/// user's. Contents stay unreadable, which is the property that matters —
/// and destroying the history was never protected anyway, the reset RPC
/// being on a pipe any local process can open.
///
/// Still nothing for `WD` (Everyone), `AU` (Authenticated Users), or any
/// per-user SID, and no read bit for anyone but SYSTEM and Administrators.
const MEMORY_DIR_SDDL: &str =
    "D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;CI;0x1101c1;;;BU)(A;OIIO;0x110180;;;BU)";

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

    /// FILE_READ_DATA on a file. Also FILE_LIST_DIRECTORY on a directory —
    /// the same bit, which is why every assertion below has to know whether
    /// the ACE it is looking at can reach a file.
    const READ_DATA: u32 = 0x0000_0001;
    /// FILE_READ_EA, READ_CONTROL, and the two generic reads. Anything here
    /// is either a read or a step towards one.
    const OTHER_READS: u32 = 0x0000_0008 | 0x0002_0000 | 0x8000_0000 | 0x1000_0000;

    /// One ACE of [`MEMORY_DIR_SDDL`], split into the fields the assertions
    /// care about. Hand-rolled rather than parsed by Windows so the tests
    /// stay pure — see the note in wrappers.rs about FFI in this crate.
    struct Ace {
        flags: String,
        rights: String,
        sid: String,
    }

    impl Ace {
        /// The access mask as a number, whether it was written in hex or as
        /// one of SDDL's mnemonics.
        fn mask(&self) -> u32 {
            if let Some(hex) = self.rights.strip_prefix("0x") {
                return u32::from_str_radix(hex, 16)
                    .unwrap_or_else(|e| panic!("unreadable mask {}: {e}", self.rights));
            }
            match self.rights.as_str() {
                "FA" => 0x001F_01FF, // FILE_ALL_ACCESS
                "SD" => 0x0001_0000, // DELETE
                other => panic!(
                    "this test knows hex masks, FA and SD; teach it {other} \
                     before using it"
                ),
            }
        }

        /// Whether this ACE can land on a FILE: either it is object-inherited
        /// into one, or -- for the ACE on the directory itself -- it cannot.
        fn reaches_a_file(&self) -> bool {
            self.flags.contains("OI")
        }
    }

    fn aces() -> Vec<Ace> {
        let body = MEMORY_DIR_SDDL
            .strip_prefix("D:PAI")
            .expect("the DACL header");
        body.split_terminator(')')
            .map(|ace| {
                let ace = ace.trim_start_matches('(');
                let fields: Vec<&str> = ace.split(';').collect();
                assert_eq!(fields.len(), 6, "an SDDL ACE has six fields: {ace}");
                assert_eq!(fields[0], "A", "every ACE here must be an allow: {ace}");
                Ace {
                    flags: fields[1].to_string(),
                    rights: fields[2].to_string(),
                    sid: fields[5].to_string(),
                }
            })
            .collect()
    }

    /// The protection itself: the two principals that may read the history,
    /// and the inheritance that gets the rule onto the files rather than
    /// leaving it on the folder.
    #[test]
    fn system_and_administrators_have_full_control_of_the_files_inside() {
        for sid in ["SY", "BA"] {
            let ace = aces()
                .into_iter()
                .find(|a| a.sid == sid)
                .unwrap_or_else(|| panic!("{sid} must be granted access: {MEMORY_DIR_SDDL}"));
            assert_eq!(ace.rights, "FA", "{sid} needs full control");
            assert!(
                ace.flags.contains("OI") && ace.flags.contains("CI"),
                "{sid}'s ACE must be inherited by the files inside, or \
                 memory.louds is created with %APPDATA%'s permissions: \
                 {MEMORY_DIR_SDDL}"
            );
        }
    }

    /// THE test. Every other assertion in this module is a way of getting
    /// here: no principal outside the pair may end up with a right that
    /// reads a file, whatever it is granted for deleting one.
    #[test]
    fn nobody_else_can_read_a_file_in_it() {
        for ace in aces() {
            if ace.sid == "SY" || ace.sid == "BA" {
                continue;
            }
            let mask = ace.mask();
            assert_eq!(
                mask & OTHER_READS,
                0,
                "{} is granted a read right ({:#x}): {MEMORY_DIR_SDDL}",
                ace.sid,
                mask & OTHER_READS
            );
            if ace.reaches_a_file() {
                // On a file this bit is FILE_READ_DATA, which is the history
                // itself. On the directory it is FILE_LIST_DIRECTORY, which
                // is only the file names -- and is what makes the folder
                // deletable at all.
                assert_eq!(
                    mask & READ_DATA,
                    0,
                    "{}'s ACE is object-inherited, so bit 0x1 is \
                     FILE_READ_DATA on every file in the directory: \
                     {MEMORY_DIR_SDDL}",
                    ace.sid
                );
            }
        }
    }

    /// The carve-out has to actually work, and "grant DELETE" alone does not.
    /// Every right asserted here was arrived at by watching an unelevated
    /// `Remove-Item -Recurse -Force` fail without it.
    #[test]
    fn an_unelevated_user_can_delete_the_directory() {
        const LIST: u32 = 0x0000_0001;
        const DELETE_CHILD: u32 = 0x0000_0040;
        const WRITE_ATTRIBUTES: u32 = 0x0000_0100;
        const DELETE: u32 = 0x0001_0000;
        const SYNCHRONIZE: u32 = 0x0010_0000;

        let users: Vec<Ace> = aces().into_iter().filter(|a| a.sid == "BU").collect();
        assert!(
            !users.is_empty(),
            "Users must be able to remove the folder, or an uninstall leaves \
             something behind that only an admin can delete: {MEMORY_DIR_SDDL}"
        );

        let directory = users
            .iter()
            .find(|a| !a.reaches_a_file())
            .expect("an ACE that applies to the directory itself")
            .mask();
        assert_ne!(
            directory & LIST,
            0,
            "the folder has to be enumerable to be emptied"
        );
        assert_ne!(directory & DELETE, 0, "and then removed");
        assert_ne!(directory & SYNCHRONIZE, 0, "a synchronous open needs this");
        assert_ne!(
            directory & DELETE_CHILD,
            0,
            "and its files deleted, which FILE_DELETE_CHILD on the parent \
             grants without touching the files' own permissions"
        );

        let file = users
            .iter()
            .find(|a| a.reaches_a_file())
            .expect("an ACE inherited by the files inside")
            .mask();
        assert_ne!(file & DELETE, 0, "Explorer renames into the Recycle Bin");
        assert_ne!(file & SYNCHRONIZE, 0, "a synchronous open needs this");
        assert_ne!(
            file & WRITE_ATTRIBUTES,
            0,
            "`Remove-Item -Force` clears ReadOnly before deleting, and fails \
             with ACCESS_DENIED without this"
        );
    }

    /// The principals that would undo the whole thing outright. `BU` is
    /// handled above, on exactly what it is granted; these have no business
    /// appearing at all.
    #[test]
    fn the_broadest_groups_are_not_granted_anything() {
        for principal in [";;;WD)", ";;;AU)", ";;;IU)"] {
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
}
