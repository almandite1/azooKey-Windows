//! Keeping one child process alive: spawning it, watching it, and applying
//! [`crate::policy`]'s verdicts. Everything here does I/O; the decisions do
//! not live here.

use std::env;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;

use crate::job::assign_to_child_job;
use crate::logging::{log_err, log_info};
use crate::policy::{
    MAX_CONSECUTIVE_WATCHDOG_KILLS, MAX_RESTARTS_IN_WINDOW, PING_INTERVAL, PING_TIMEOUT,
    RESTART_WINDOW, RestartDecision, RestartPolicy, Verdict, WatchdogPolicy,
};

/// Runs one child's supervisor and, if it gives up, ends the whole launcher.
///
/// If a supervisor gives up (spawn failure, or a crash/hang loop that blew
/// its budget) the IME is dead until something restarts it — but the
/// single-instance mutex is held for as long as this launcher lives, so a
/// manual relaunch would just exit immediately. Exiting the process releases
/// that mutex (letting a fresh launch recover) and, via the job's
/// KILL_ON_JOB_CLOSE, tears down the other child so it can't linger and hold
/// the pipe names.
pub(crate) async fn run_supervisor(exe: &'static str, prefix: &'static str, pipe_name: String) {
    if supervise(exe, prefix, &pipe_name).await == SuperviseOutcome::GaveUp {
        log_err(&format!(
            "{prefix} is unrecoverable; exiting the launcher so a fresh start can take over"
        ));
        std::process::exit(1);
    }
}

/// Runs one child's supervisor and, if it gives up, lets the rest of the
/// stack carry on.
///
/// For a child the IME does not need. [`run_supervisor`] ends the launcher
/// because a dead server or UI means no input at all, so releasing the
/// single-instance mutex is the only route back. That reasoning does not
/// transfer: the plugin host going away costs the user their add-on
/// candidates and nothing else, and the conversion path treats an absent
/// host exactly like a switched-off one. Tearing down a working IME
/// because an optional process could not be kept alive would turn a
/// cosmetic failure into a total one — and, through the job object, would
/// do it by killing the very server that was still working.
pub(crate) async fn run_optional_supervisor(
    exe: &'static str,
    prefix: &'static str,
    pipe_name: String,
) {
    if supervise(exe, prefix, &pipe_name).await == SuperviseOutcome::GaveUp {
        log_err(&format!(
            "{prefix} is unrecoverable; carrying on without it (conversion is unaffected)"
        ));
    }
}

/// Why a supervisor loop stopped.
#[derive(Debug, PartialEq, Eq)]
enum SuperviseOutcome {
    /// The child exited cleanly and on purpose. Nothing to recover. (The
    /// UIAccess re-exec no longer takes this path: the spawned ui.exe stays
    /// alive as a shim that mirrors the UIAccess child's exit code, so this
    /// supervisor keeps covering the process that actually draws the UI.)
    Exited,
    /// The child is unrecoverable — spawn failure, or a crash/hang loop that
    /// exhausted the restart budget. The launcher should stand down.
    GaveUp,
}

/// Keeps a child process running: restarts it when it exits abnormally or
/// stops answering health checks, with exponential backoff, and gives up on
/// a tight crash/hang loop.
async fn supervise(exe: &'static str, prefix: &'static str, pipe_name: &str) -> SuperviseOutcome {
    let mut policy = RestartPolicy::new();

    loop {
        let Some(mut child) = start_process(exe, prefix) else {
            // spawn failure (e.g. missing binary) won't fix itself
            log_err(&format!("{prefix} could not be started; giving up"));
            return SuperviseOutcome::GaveUp;
        };

        let started_at = Instant::now();
        let saw_healthy = Arc::new(AtomicBool::new(false));

        let hung = tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if s.success() => {
                        log_info(&format!("{prefix} exited normally"));
                        return SuperviseOutcome::Exited;
                    }
                    Ok(s) => {
                        log_err(&format!("{prefix} exited abnormally: {s}"));
                        false
                    }
                    Err(e) => {
                        // can't observe the child anymore: treat as
                        // unrecoverable rather than spin-restarting blind
                        log_err(&format!("{prefix} wait failed: {e}"));
                        return SuperviseOutcome::GaveUp;
                    }
                }
            }
            _ = watchdog(pipe_name, prefix, saw_healthy.clone()) => {
                log_err(&format!("{prefix} stopped answering health checks; killing it"));
                // tokio's kill() forces termination and reaps the child
                if let Err(e) = child.kill().await {
                    log_err(&format!("{prefix} kill failed: {e}"));
                }
                true
            }
        };

        let backoff = match policy.on_child_stopped(
            hung,
            saw_healthy.load(Ordering::SeqCst),
            started_at.elapsed(),
            Instant::now(),
        ) {
            RestartDecision::GiveUpHangLoop => {
                log_err(&format!(
                    "{prefix} was killed by the watchdog {MAX_CONSECUTIVE_WATCHDOG_KILLS} times without ever becoming healthy; giving up"
                ));
                return SuperviseOutcome::GaveUp;
            }
            RestartDecision::GiveUpCrashLoop => {
                log_err(&format!(
                    "{prefix} crashed {MAX_RESTARTS_IN_WINDOW} times within {RESTART_WINDOW:?}; giving up"
                ));
                return SuperviseOutcome::GaveUp;
            }
            RestartDecision::RetryAfter(backoff) => backoff,
        };

        log_err(&format!("{prefix} restarting in {backoff:?}"));
        tokio::time::sleep(backoff).await;
    }
}

/// Resolves only when the peer is declared hung. Sets `saw_healthy` as soon
/// as one health check succeeds.
async fn watchdog(pipe_name: &str, prefix: &'static str, saw_healthy: Arc<AtomicBool>) {
    let Ok(channel) = shared::pipe::lazy_pipe_channel(pipe_name.to_string()) else {
        // cannot even build a channel: run without hang detection rather
        // than killing a possibly-fine child
        log_err(&format!(
            "watchdog for {pipe_name} disabled: failed to build channel"
        ));
        std::future::pending::<()>().await;
        unreachable!();
    };
    let mut client = HealthClient::new(channel);
    let mut policy = WatchdogPolicy::new(Instant::now());

    loop {
        tokio::time::sleep(PING_INTERVAL).await;

        // healthy = the RPC answered in time AND reports SERVING; the UI
        // flips itself to NOT_SERVING when its event loop stalls
        let ok = matches!(
            tokio::time::timeout(
                PING_TIMEOUT,
                client.check(HealthCheckRequest {
                    service: String::new(),
                }),
            )
            .await,
            Ok(Ok(response))
                if response.get_ref().status
                    == tonic_health::pb::health_check_response::ServingStatus::Serving as i32
        );

        if ok && !saw_healthy.swap(true, Ordering::SeqCst) {
            log_info(&format!("{prefix} health check ok"));
        }

        if policy.on_ping_result(ok, Instant::now()) == Verdict::Hung {
            return;
        }
    }
}

fn start_process(exe: &str, prefix: &str) -> Option<Child> {
    // Resolve the child against the launcher's OWN directory instead of
    // relying on CreateProcess's search order, which also consults PATH — and
    // the backend directory (llama_*) is prepended to PATH. Fall back to the
    // bare name if the launcher path can't be determined.
    let resolved = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(exe)));
    let mut command = match &resolved {
        Some(path) => Command::new(path),
        None => Command::new(exe),
    };

    let mut child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            log_err(&format!("Failed to start {}: {}", exe, e));
            return None;
        }
    };

    // tie the child to the launcher's lifetime before anything else, so a
    // launcher crash in the next instant still can't orphan it
    assign_to_child_job(&child, prefix);

    pump(child.stdout.take(), prefix, log_info);
    pump(child.stderr.take(), prefix, log_err);

    Some(child)
}

/// Forwards one of the child's pipes into the launcher's log, line by line.
///
/// stdout and stderr differ only in which log function they feed; they were
/// two verbatim copies of this block.
fn pump<R>(reader: Option<R>, prefix: &str, sink: fn(&str))
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(reader) = reader else {
        return;
    };
    let prefix = prefix.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            sink(&format!("{}: {}", prefix, line));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::run_optional_supervisor;

    /// The whole difference between the two variants, and the only way to
    /// assert it: `run_supervisor` ends the PROCESS when it gives up, so a
    /// non-fatal variant that accidentally took the same path would kill
    /// this test binary rather than fail an assertion. Reaching the line
    /// after the await is the proof.
    ///
    /// A name nothing can spawn makes the supervisor give up immediately,
    /// which is the same verdict a crash loop reaches the slow way.
    #[tokio::test]
    async fn an_optional_child_that_cannot_start_does_not_end_the_launcher() {
        run_optional_supervisor(
            "azookey-no-such-binary-should-ever-exist.exe",
            "[test]",
            r"\\.\pipe\azookey_test_nonexistent".to_string(),
        )
        .await;

        // if the variant exited, nothing below would run
    }
}
