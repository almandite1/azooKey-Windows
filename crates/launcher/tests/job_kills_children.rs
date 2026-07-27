//! Integration check for the kill-on-close job the launcher ties its
//! children to (orphan prevention).
//!
//! This is the guarantee the whole scheme rests on: when the launcher
//! process ends — crash included — closing its last job handle must take the
//! children down with it, so no orphaned server/ui keeps the machine-global
//! pipe names open.
//!
//! It drives `shared::job::KillOnCloseJob`, the same type the launcher and
//! `ui` use. It used to reproduce the API sequence by hand, because a bin
//! crate's integration test cannot call the crate's own helpers — so what it
//! proved was that the sequence works, not that the shipped code uses it.

use std::os::windows::io::AsRawHandle;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shared::job::KillOnCloseJob;
use windows::Win32::Foundation::{CloseHandle, HANDLE};

#[test]
fn closing_the_job_kills_assigned_children() {
    // a child that would otherwise stay alive for a minute
    let mut child = Command::new("cmd")
        .args(["/c", "ping", "-n", "60", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the test child");

    let job = KillOnCloseJob::create().expect("creating the job failed");
    // SAFETY: a live handle to a child this test just spawned.
    unsafe { job.assign(HANDLE(child.as_raw_handle())) }.expect("assigning the child failed");

    // sanity: the child is alive while the job handle is still open
    assert!(
        child.try_wait().expect("try_wait failed").is_none(),
        "child should still be running before the job handle is closed"
    );

    // Closing the last job handle is what happens when the launcher process
    // ends. The type does not do this on drop — on purpose, since it would
    // kill the children — so the test takes the raw handle to close it.
    unsafe { CloseHandle(job.into_raw()).expect("CloseHandle failed") };

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
