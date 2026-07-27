//! Tying the supervised children to the launcher's own lifetime.
//!
//! The job object itself is [`shared::job::KillOnCloseJob`]; this is the
//! launcher's one instance of it and the policy around it.

use std::sync::OnceLock;

use shared::job::KillOnCloseJob;
use tokio::process::Child;
use windows::Win32::Foundation::HANDLE;

use crate::logging::log_err;

/// The children are added to this, so if the launcher dies (crash or kill)
/// instead of exiting cleanly, Windows closes the last job handle and tears
/// the children down too. Without it, orphaned server/ui processes keep the
/// machine-global pipe names open, and the next logon's launcher — which the
/// single-instance mutex lets through, since it only guards launchers —
/// burns its whole restart budget losing first_pipe_instance to the orphan.
///
/// Parked in a `OnceLock` and never dropped: closing the last handle is what
/// kills the members, so the job must live exactly as long as this process.
static CHILD_JOB: OnceLock<KillOnCloseJob> = OnceLock::new();

/// Creates the job the children are assigned to. A failure is not fatal —
/// the launcher still supervises, it just loses the guarantee that its
/// children die with it.
pub(crate) fn init_child_job() {
    match KillOnCloseJob::create() {
        Ok(job) => {
            let _ = CHILD_JOB.set(job);
        }
        Err(e) => log_err(&format!(
            "kill-on-close job creation failed ({e}); children won't be tied to the launcher's lifetime"
        )),
    }
}

/// Adds a freshly spawned child to the job, if the job exists.
pub(crate) fn assign_to_child_job(child: &Child, prefix: &str) {
    let Some(job) = CHILD_JOB.get() else {
        return;
    };
    let Some(raw) = child.raw_handle() else {
        log_err(&format!("{prefix} has no handle to assign to the job"));
        return;
    };
    // SAFETY: a handle tokio still owns for a child it spawned, so it is live
    // and carries full access.
    if let Err(e) = unsafe { job.assign(HANDLE(raw)) } {
        log_err(&format!("{prefix} could not be assigned to the job ({e})"));
    }
}
