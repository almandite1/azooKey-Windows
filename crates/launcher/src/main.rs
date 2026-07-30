//! The supervisor that keeps the engine and the candidate window alive.
//!
//! Started at logon by a scheduled task. Four concerns, one per module:
//! [`logging`] (the session log file), [`job`] (the kill-on-close job the
//! children are tied to), [`policy`] (the pure restart/hang decisions) and
//! [`supervisor`] (the loop that applies them). This file is startup: the
//! single-instance guard, the backend PATH, and the three supervisors —
//! two of which may end the launcher when they give up, and one of which
//! may not.

mod job;
mod logging;
mod policy;
mod supervisor;

use std::env;

use shared::AppConfig;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{PCWSTR, w};

use logging::{init_log_file, log_err, log_info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_log_file();

    // The first line of every session, before anything can go wrong. Without
    // it, "no log for this boot" was ambiguous between the launcher never
    // being started (a logon trigger that did not fire) and the launcher
    // starting and dying before it had anything to report — and telling those
    // apart is most of the diagnosis (issue #79).
    log_info(&format!(
        "azooKey launcher {} starting (pid {}, exe {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("<unknown: {e}>")),
    ));

    // single-instance guard: the scheduled task and a manual start can race,
    // and two launchers would fight over this session's pipe names — the
    // loser's server dies on first_pipe_instance and burns through its
    // restart budget. Local\ = one launcher PER SESSION, matching the
    // session-scoped pipe names (B19): another user's session runs its own
    // whole stack instead of being locked out by this one.
    match another_instance_running(w!("Local\\AzookeyLauncherSingleton")) {
        Ok(true) => {
            log_err("another azooKey launcher is already running; exiting");
            return Ok(());
        }
        Ok(false) => {}
        // an unlikely mutex failure must not keep the IME from starting
        Err(e) => log_err(&format!(
            "single-instance check failed ({e}); continuing anyway"
        )),
    }

    // children are added to this job so a launcher crash can't orphan them
    job::init_child_job();

    prepend_backend_to_path()?;

    // a crashed OR HUNG server would otherwise leave the IME dead in every
    // application until re-login, so both children are supervised: exits
    // are restarted with backoff, and a health-check watchdog kills a child
    // that stops answering (the kill then flows into the same restart path)
    // One table, because the three children differ only in these five things
    // and everything else about supervising them is identical. `fatal` used to
    // be expressed as which of two near-identical functions was called, and
    // the plugin host is NOT allowed to take the launcher down with it: an
    // absent plugin host is what the server already expects whenever the
    // feature is off, so losing it costs the user their add-on candidates and
    // nothing else.
    //
    // The plugin host is started unconditionally rather than only when
    // plugins.enable is set — otherwise turning the feature on in the settings
    // app would appear to do nothing until the next logon, since the server
    // reloads that setting live and the launcher does not.
    let children = vec![
        supervisor::SupervisedChild {
            exe: "azookey-server.exe",
            prefix: "[server]",
            pipe: shared::pipe::server_pipe(),
            fatal: true,
            // reads a dictionary and the zenz model before it answers anything
            startup_grace: policy::STARTUP_GRACE,
        },
        supervisor::SupervisedChild {
            exe: "ui.exe",
            prefix: "[ui]",
            pipe: shared::pipe::ui_pipe(),
            fatal: true,
            startup_grace: policy::STARTUP_GRACE,
        },
        supervisor::SupervisedChild {
            exe: "plugin-host.exe",
            prefix: "[plugin-host]",
            pipe: shared::pipe::plugin_pipe(),
            fatal: false,
            // opens a pipe and serves builtins; the engine's two minutes meant
            // a broken host cost ten before it was even given up on
            startup_grace: policy::FAST_STARTUP_GRACE,
        },
    ];

    let handles: Vec<_> = children
        .into_iter()
        .map(|child| tokio::spawn(supervisor::run_supervisor(child)))
        .collect();

    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

/// Puts the configured llama backend directory at the front of `PATH`, so
/// each child inherits a `PATH` on which it can find the backend DLLs.
fn prepend_backend_to_path() -> anyhow::Result<()> {
    let config = AppConfig::new();

    let exe_path = env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable path has no parent directory"))?
        .to_path_buf();
    let backend_dir = match config.zenzai.backend.as_str() {
        "cpu" => "llama_cpu",
        "cuda" => "llama_cuda",
        "vulkan" => "llama_vulkan",
        _ => "llama_cpu",
    };

    let backend_path = exe_path.join(backend_dir);
    let backend_path_str = backend_path.to_string_lossy();

    let existing = env::var("PATH").unwrap_or_default();
    let new_path = format!("{};{}", backend_path_str, existing);
    // set_var is unsafe as of edition 2024: it is UB if another thread reads
    // the environment concurrently. Sound here — this runs before either
    // tokio::spawn in main, so the only other threads in the process are the
    // runtime's idle workers, which never touch the environment while parked.
    // The later reads that matter (each child Command inheriting this PATH so
    // it can find the llama backend DLLs) happen on tasks spawned after this
    // point, and the spawn supplies the happens-before edge.
    unsafe { env::set_var("PATH", &new_path) };

    Ok(())
}

/// Returns true when another process already owns the named mutex.
/// The handle is intentionally kept open (never closed) so the mutex lives
/// exactly as long as this process — that lifetime IS the lock.
fn another_instance_running(name: PCWSTR) -> windows::core::Result<bool> {
    unsafe {
        let _handle = CreateMutexW(None, false, name)?;
        Ok(GetLastError() == ERROR_ALREADY_EXISTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_mutex_holder_detects_the_first() {
        // a test-only name so a real running launcher can't interfere
        let name = w!("Local\\AzookeyLauncherSingletonTest");
        assert!(!another_instance_running(name).unwrap());
        // the first handle is still open in this process, so a second
        // acquisition sees ERROR_ALREADY_EXISTS — same as a second process
        assert!(another_instance_running(name).unwrap());
    }
}
