//! Keeping one child process alive: spawning it, watching it, and applying
//! [`crate::policy`]'s verdicts. Everything here does I/O; the decisions do
//! not live here.

use std::env;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
pub(crate) async fn run_supervisor(child: SupervisedChild) {
    supervise(child.exe, child.prefix, &child.pipe, child.startup_grace).await;
    // supervise only ever returns by giving up
    if child.fatal {
        log_err(&format!(
            "{} is unrecoverable; exiting the launcher so a fresh start can take over",
            child.prefix
        ));
        std::process::exit(1);
    }
    log_err(&format!("{} {GAVE_UP_NON_FATALLY}", child.prefix));
}

/// One supervised process and everything that differs between the three.
///
/// A struct rather than three call sites with positional arguments: the
/// difference between the fatal and the non-fatal child used to be which of
/// two nearly identical functions was called, which is a one-word edit that
/// turns "the add-ons are gone" into "the IME is gone" and compiles.
pub(crate) struct SupervisedChild {
    pub(crate) exe: &'static str,
    pub(crate) prefix: &'static str,
    pub(crate) pipe: String,
    /// Whether giving up on this child ends the launcher.
    ///
    /// True for the two the IME cannot work without. False means an absent
    /// child costs its own feature and nothing else — and, critically, does
    /// not take the still-working server down through the job object.
    pub(crate) fatal: bool,
    /// How long it may take to answer its first health check. The engine
    /// loads a dictionary and a model; the plugin host loads nothing.
    pub(crate) startup_grace: Duration,
}

/// Why `fatal: false` exists, for a child the IME does not need.
///
/// A dead server or UI means no input at all, so ending the launcher —
/// releasing the single-instance mutex — is the only route back. That
/// reasoning does not transfer: the plugin host going away costs the user
/// their add-on candidates and nothing else, and the conversion path treats
/// an absent host exactly like a switched-off one. Tearing down a working IME
/// because an optional process could not be kept alive would turn a cosmetic
/// failure into a total one — and, through the job object, would do it by
/// killing the very server that was still working.
///
/// What a non-fatal give-up does NOT do, worth knowing before reading a bug
/// report: it does not try again for the life of the launcher. A re-arm after
/// some long cooldown would be reasonable and is not implemented; until then
/// the recovery is a logon, and the log line below is the only sign anything
/// is missing.
///
/// The sentence a field log is searched for when an add-on is missing and
/// nobody knows why. Pinned by a test because it is the only evidence
/// this state produces.
const GAVE_UP_NON_FATALLY: &str =
    "is unrecoverable; carrying on without it (conversion is unaffected)";

/// How long a child is given to actually die after a kill that reported an
/// error. Short: this is confirming a termination that has already been
/// requested, not waiting for a graceful shutdown.
const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether the child is gone within `timeout`.
///
/// `wait()` on an already-reaped child returns immediately; on a live one it
/// blocks, which is what the timeout is for.
async fn exited_within(child: &mut Child, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, child.wait()).await, Ok(Ok(_)))
}

/// Keeps a child process running: restarts it when it exits abnormally or
/// stops answering health checks, with exponential backoff.
///
/// Returns only when the child is unrecoverable — a spawn failure, or a
/// crash/hang loop that exhausted the restart budget. What giving up costs is
/// the caller's to decide.
async fn supervise(
    exe: &'static str,
    prefix: &'static str,
    pipe_name: &str,
    startup_grace: Duration,
) {
    let mut policy = RestartPolicy::new();

    loop {
        let Some(mut child) = start_process(exe, prefix) else {
            // spawn failure (e.g. missing binary) won't fix itself
            log_err(&format!("{prefix} could not be started; giving up"));
            return;
        };

        let started_at = Instant::now();
        let saw_healthy = Arc::new(AtomicBool::new(false));

        let hung = tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if s.success() => {
                        // Restarted, not taken at its word. Nothing here exits
                        // on purpose, so a clean exit is a child that stopped
                        // for a reason we did not see — and returning left the
                        // launcher holding the singleton mutex with no server
                        // behind it, which is the state the give-up path calls
                        // "the only way back". Same budget as a crash: a child
                        // that keeps exiting cleanly still gives up in the end.
                        log_err(&format!(
                            "{prefix} exited normally, which nothing does on purpose; \
                             restarting it"
                        ));
                        false
                    }
                    Ok(s) => {
                        log_err(&format!("{prefix} exited abnormally: {s}"));
                        false
                    }
                    Err(e) => {
                        // can't observe the child anymore: treat as
                        // unrecoverable rather than spin-restarting blind
                        log_err(&format!("{prefix} wait failed: {e}"));
                        return;
                    }
                }
            }
            _ = watchdog(pipe_name, prefix, saw_healthy.clone(), startup_grace) => {
                log_err(&format!("{prefix} stopped answering health checks; killing it"));
                // tokio's kill() forces termination and reaps the child
                if let Err(e) = child.kill().await {
                    log_err(&format!("{prefix} kill failed: {e}"));
                    // A failed kill used to fall straight through to the
                    // restart, which starts a SECOND process while the first
                    // still holds the pipe. The newcomer cannot take
                    // first_pipe_instance, dies immediately, and burns the
                    // crash budget until the supervisor gives up — so a kill
                    // that did not work presented as an unrecoverable child.
                    // Confirm it is really gone; if not, leave it for the next
                    // watchdog round rather than racing it.
                    if !exited_within(&mut child, KILL_CONFIRM_TIMEOUT).await {
                        log_err(&format!(
                            "{prefix} is still running after a failed kill; skipping this \
                             restart so a second instance does not fight it for the pipe"
                        ));
                        continue;
                    }
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
                return;
            }
            RestartDecision::GiveUpCrashLoop => {
                log_err(&format!(
                    "{prefix} crashed {MAX_RESTARTS_IN_WINDOW} times within {RESTART_WINDOW:?}; giving up"
                ));
                return;
            }
            RestartDecision::RetryAfter(backoff) => backoff,
        };

        log_err(&format!("{prefix} restarting in {backoff:?}"));
        tokio::time::sleep(backoff).await;
    }
}

/// Resolves only when the peer is declared hung. Sets `saw_healthy` as soon
/// as one health check succeeds.
async fn watchdog(
    pipe_name: &str,
    prefix: &'static str,
    saw_healthy: Arc<AtomicBool>,
    startup_grace: Duration,
) {
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
    let mut policy = WatchdogPolicy::with_startup_grace(Instant::now(), startup_grace);

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
    use super::{GAVE_UP_NON_FATALLY, SupervisedChild, run_supervisor};
    use crate::policy::FAST_STARTUP_GRACE;

    /// The message says three things a reader needs: that it is over,
    /// that the launcher is staying, and that typing still works. Losing
    /// any of them turns the log into "something failed" — which is what
    /// sends someone looking for an IME bug that is not there.
    #[test]
    fn the_give_up_message_says_the_ime_is_unaffected() {
        assert!(GAVE_UP_NON_FATALLY.contains("unrecoverable"));
        assert!(GAVE_UP_NON_FATALLY.contains("carrying on without it"));
        assert!(GAVE_UP_NON_FATALLY.contains("conversion is unaffected"));
    }

    /// The whole difference `fatal` makes, and the only way to assert it: a
    /// fatal child ends the PROCESS when it gives up, so a non-fatal one that
    /// accidentally took the same path would kill this test binary rather than
    /// fail an assertion. Reaching the line after the await is the proof.
    ///
    /// A name nothing can spawn makes the supervisor give up immediately,
    /// which is the same verdict a crash loop reaches the slow way.
    #[tokio::test]
    async fn an_optional_child_that_cannot_start_does_not_end_the_launcher() {
        run_supervisor(SupervisedChild {
            exe: "azookey-no-such-binary-should-ever-exist.exe",
            prefix: "[test]",
            pipe: r"\\.\pipe\azookey_test_nonexistent".to_string(),
            fatal: false,
            startup_grace: FAST_STARTUP_GRACE,
        })
        .await;

        // if the variant exited, nothing below would run
    }
}
