//! Kill-on-close job objects: tying a child process's lifetime to its
//! parent's.
//!
//! Two components need exactly this and each had its own copy, one of which
//! said in a comment that it was "exactly like the launcher's":
//!
//! * `launcher` puts `azookey-server.exe` and `ui.exe` in one, so a launcher
//!   crash cannot orphan them. An orphan keeps the machine-global pipe names
//!   open, and the next logon's launcher — which the single-instance mutex
//!   lets through, since it only guards launchers — burns its whole restart
//!   budget losing `first_pipe_instance` to it.
//! * `ui` puts the UIAccess child it re-execs in one, so the child dies with
//!   the supervision shim the launcher is actually watching.
//!
//! The third copy is `launcher/tests/job_kills_children.rs`, which
//! reproduced the API sequence because a bin crate's integration test cannot
//! call the crate's own helpers. It can call this.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::core::PCWSTR;

/// A job object whose members are killed when its last handle closes.
///
/// The handle is deliberately NOT closed on drop, and the type is not
/// `Drop` at all: closing the last handle is what kills the members, so a
/// job that went out of scope would take the children with it. Both callers
/// park it somewhere that lives as long as the process (a `OnceLock`, or a
/// local held until `ExitProcess`). [`Self::into_raw`] is there for the
/// caller that would rather hold the bare handle.
#[derive(Debug)]
pub struct KillOnCloseJob(HANDLE);

// The handle is only ever passed to AssignProcessToJobObject, which is
// thread-safe; the launcher assigns from its per-child supervisor tasks.
unsafe impl Send for KillOnCloseJob {}
unsafe impl Sync for KillOnCloseJob {}

impl KillOnCloseJob {
    /// Creates the job. The caller decides what a failure means — for both of
    /// ours it is not fatal: supervision still works, only the
    /// die-with-the-parent guarantee is lost.
    pub fn create() -> windows::core::Result<Self> {
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null())?;

            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };

            if let Err(error) = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) {
                // a job without the limit set is worse than none: it would
                // silently not kill anything
                let _ = CloseHandle(job);
                return Err(error);
            }

            Ok(Self(job))
        }
    }

    /// Adds a process to the job.
    ///
    /// # Safety
    /// `process` must be a live process handle with
    /// `PROCESS_SET_QUOTA | PROCESS_TERMINATE` access.
    pub unsafe fn assign(&self, process: HANDLE) -> windows::core::Result<()> {
        unsafe { AssignProcessToJobObject(self.0, process) }
    }

    /// The raw handle, for a caller that has its own place to keep it.
    /// Closing it kills the members — see the type's note.
    pub fn into_raw(self) -> HANDLE {
        self.0
    }
}
