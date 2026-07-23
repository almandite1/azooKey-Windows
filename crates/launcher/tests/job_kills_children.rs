//! Integration check for the kill-on-close job the launcher ties its
//! children to (orphan prevention).
//!
//! This is the guarantee the whole scheme rests on: when the launcher
//! process ends — crash included — closing its last job handle must take
//! the children down with it, so no orphaned server/ui keeps the
//! machine-global pipe names open. A bin crate's tests can't call the
//! launcher's own helpers, so this reproduces the same API sequence
//! (CreateJobObject → KILL_ON_JOB_CLOSE → AssignProcessToJobObject → close)
//! and proves the child actually dies.

use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::core::PCWSTR;

#[test]
fn closing_the_job_kills_assigned_children() {
    // a child that would otherwise stay alive for a minute
    let mut child = Command::new("cmd")
        .args(["/c", "ping", "-n", "60", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the test child");

    let job = unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).expect("CreateJobObject failed");

        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .expect("SetInformationJobObject failed");

        AssignProcessToJobObject(job, HANDLE(child.as_raw_handle()))
            .expect("AssignProcessToJobObject failed");

        job
    };

    // sanity: the child is alive while the job handle is still open
    assert!(
        child.try_wait().expect("try_wait failed").is_none(),
        "child should still be running before the job handle is closed"
    );

    // dropping the launcher's last job handle is what happens when the
    // launcher process ends; KILL_ON_JOB_CLOSE must reap the child
    unsafe { CloseHandle(job).expect("CloseHandle failed") };

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("try_wait failed").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closing the job did not kill the child within 5s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
