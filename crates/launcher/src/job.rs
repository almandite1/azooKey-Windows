//! The kill-on-close job object the supervised children are assigned to.

use std::sync::OnceLock;

use tokio::process::Child;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::core::PCWSTR;

use crate::logging::log_err;

/// A job object with KILL_ON_JOB_CLOSE: the children are added to it, so if
/// the launcher dies (crash or kill) instead of exiting cleanly, Windows
/// closes the last job handle and tears the children down too. Without this,
/// orphaned server/ui processes keep the machine-global pipe names open, and
/// the next logon's launcher — which the single-instance mutex lets through,
/// since it only guards launchers — burns its whole restart budget losing
/// first_pipe_instance to the orphan.
///
/// The handle is deliberately leaked into a OnceLock and never closed: the
/// job must outlive every child, i.e. live exactly as long as this process.
static CHILD_JOB: OnceLock<JobHandle> = OnceLock::new();

struct JobHandle(HANDLE);
// the job handle is only ever passed to AssignProcessToJobObject, which is
// thread-safe; children are assigned from the per-child supervisor tasks
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// Creates the kill-on-close job the children are assigned to. A failure is
/// not fatal — the launcher still supervises, it just loses the guarantee
/// that its children die with it.
pub(crate) fn init_child_job() {
    let job = unsafe {
        match CreateJobObjectW(None, PCWSTR::null()) {
            Ok(job) => job,
            Err(e) => {
                log_err(&format!(
                    "CreateJobObject failed ({e}); children won't be tied to the launcher's lifetime"
                ));
                return;
            }
        }
    };

    let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation:
            windows::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
        ..Default::default()
    };

    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if let Err(e) = ok {
        log_err(&format!(
            "SetInformationJobObject failed ({e}); children won't be tied to the launcher's lifetime"
        ));
        return;
    }

    let _ = CHILD_JOB.set(JobHandle(job));
}

/// Adds a freshly spawned child to the kill-on-close job, if the job exists.
pub(crate) fn assign_to_child_job(child: &Child, prefix: &str) {
    let Some(job) = CHILD_JOB.get() else {
        return;
    };
    let Some(raw) = child.raw_handle() else {
        log_err(&format!("{prefix} has no handle to assign to the job"));
        return;
    };
    if let Err(e) = unsafe { AssignProcessToJobObject(job.0, HANDLE(raw)) } {
        log_err(&format!("{prefix} could not be assigned to the job ({e})"));
    }
}
