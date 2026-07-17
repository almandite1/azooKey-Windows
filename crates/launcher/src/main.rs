use shared::AppConfig;
use std::env;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;

/// give up when a child keeps crashing this many times within RESTART_WINDOW
const MAX_RESTARTS_IN_WINDOW: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

// -- watchdog --
/// how often the health of a child is checked
const PING_INTERVAL: Duration = Duration::from_secs(10);
/// hard deadline for a single health check
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// this many failures in a row (after the child was healthy once) = hung
const MAX_CONSECUTIVE_PING_FAILURES: u32 = 3;
/// a child that never answers a single ping gets this long before it is
/// declared hung (covers dictionary/model loading at startup)
const STARTUP_GRACE: Duration = Duration::from_secs(120);
/// give up when the watchdog kills a child this many times in a row
/// without the child ever becoming healthy in between
const MAX_CONSECUTIVE_WATCHDOG_KILLS: u32 = 5;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // single-instance guard: the scheduled task and a manual start can race,
    // and two launchers would fight over the (machine-global) pipe names —
    // the loser's server dies on first_pipe_instance and burns through its
    // restart budget. Global\ namespace matches the pipes' global scope.
    match another_instance_running(w!("Global\\AzookeyLauncherSingleton")) {
        Ok(true) => {
            eprintln!("another azooKey launcher is already running; exiting");
            return Ok(());
        }
        Ok(false) => {}
        // an unlikely mutex failure must not keep the IME from starting
        Err(e) => eprintln!("single-instance check failed ({e}); continuing anyway"),
    }

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

    let mut new_path = env::var("PATH").unwrap_or_else(|_| String::new());
    new_path = format!("{};{}", backend_path_str, new_path);
    env::set_var("PATH", &new_path);

    // a crashed OR HUNG server would otherwise leave the IME dead in every
    // application until re-login, so both children are supervised: exits
    // are restarted with backoff, and a health-check watchdog kills a child
    // that stops answering (the kill then flows into the same restart path)
    let server_handle = tokio::spawn(supervise(
        "azookey-server.exe",
        "[server]",
        shared::pipe::SERVER_PIPE,
    ));
    let ui_handle = tokio::spawn(supervise("ui.exe", "[ui]", shared::pipe::UI_PIPE));

    let _ = server_handle.await;
    let _ = ui_handle.await;

    Ok(())
}

/// Keeps a child process running: restarts it when it exits abnormally or
/// stops answering health checks, with exponential backoff, and gives up on
/// a tight crash/hang loop.
async fn supervise(exe: &'static str, prefix: &'static str, pipe_name: &'static str) {
    let mut recent_restarts: Vec<Instant> = Vec::new();
    let mut backoff = Duration::from_secs(1);
    let mut consecutive_watchdog_kills: u32 = 0;

    loop {
        let Some(mut child) = start_process(exe, prefix) else {
            // spawn failure (e.g. missing binary) won't fix itself
            eprintln!("{prefix} could not be started; giving up");
            return;
        };

        let started_at = Instant::now();
        let saw_healthy = Arc::new(AtomicBool::new(false));

        let hung = tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(s) if s.success() => {
                        println!("{prefix} exited normally");
                        return;
                    }
                    Ok(s) => {
                        eprintln!("{prefix} exited abnormally: {s}");
                        false
                    }
                    Err(e) => {
                        eprintln!("{prefix} wait failed: {e}");
                        return;
                    }
                }
            }
            _ = watchdog(pipe_name, prefix, saw_healthy.clone()) => {
                eprintln!("{prefix} stopped answering health checks; killing it");
                // tokio's kill() forces termination and reaps the child
                if let Err(e) = child.kill().await {
                    eprintln!("{prefix} kill failed: {e}");
                }
                true
            }
        };

        // a hang loop is slower than the 60s crash window (detection alone
        // takes ~45s), so count watchdog kills separately: reaching a
        // healthy state is what proves a restart was worthwhile
        if hung && !saw_healthy.load(Ordering::SeqCst) {
            consecutive_watchdog_kills += 1;
            if consecutive_watchdog_kills >= MAX_CONSECUTIVE_WATCHDOG_KILLS {
                eprintln!(
                    "{prefix} was killed by the watchdog {MAX_CONSECUTIVE_WATCHDOG_KILLS} times without ever becoming healthy; giving up"
                );
                return;
            }
        } else if saw_healthy.load(Ordering::SeqCst) {
            consecutive_watchdog_kills = 0;
        }

        // a stable AND healthy stretch resets the backoff — "alive for a
        // minute" alone would also match a server that hangs right away
        if started_at.elapsed() >= RESTART_WINDOW && saw_healthy.load(Ordering::SeqCst) {
            backoff = Duration::from_secs(1);
            recent_restarts.clear();
        }

        let now = Instant::now();
        recent_restarts.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        if recent_restarts.len() >= MAX_RESTARTS_IN_WINDOW {
            eprintln!(
                "{prefix} crashed {MAX_RESTARTS_IN_WINDOW} times within {RESTART_WINDOW:?}; giving up"
            );
            return;
        }
        recent_restarts.push(now);

        eprintln!("{prefix} restarting in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Resolves only when the peer is declared hung. Sets `saw_healthy` as soon
/// as one health check succeeds.
async fn watchdog(pipe_name: &'static str, prefix: &'static str, saw_healthy: Arc<AtomicBool>) {
    let Ok(channel) = shared::pipe::lazy_pipe_channel(pipe_name) else {
        // cannot even build a channel: run without hang detection rather
        // than killing a possibly-fine child
        eprintln!("watchdog for {pipe_name} disabled: failed to build channel");
        std::future::pending::<()>().await;
        unreachable!();
    };
    let mut client = HealthClient::new(channel);
    let mut policy = WatchdogPolicy::new(Instant::now());

    loop {
        tokio::time::sleep(PING_INTERVAL).await;

        let ok = matches!(
            tokio::time::timeout(
                PING_TIMEOUT,
                client.check(HealthCheckRequest {
                    service: String::new(),
                }),
            )
            .await,
            Ok(Ok(_))
        );

        if ok && !saw_healthy.swap(true, Ordering::SeqCst) {
            println!("{prefix} health check ok");
        }

        if policy.on_ping_result(ok, Instant::now()) == Verdict::Hung {
            return;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Healthy,
    Hung,
}

/// Pure hang-detection policy, separated from I/O for unit testing.
struct WatchdogPolicy {
    started_at: Instant,
    ever_succeeded: bool,
    consecutive_failures: u32,
}

impl WatchdogPolicy {
    fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            ever_succeeded: false,
            consecutive_failures: 0,
        }
    }

    fn on_ping_result(&mut self, ok: bool, now: Instant) -> Verdict {
        if ok {
            self.ever_succeeded = true;
            self.consecutive_failures = 0;
            return Verdict::Healthy;
        }

        if !self.ever_succeeded {
            // startup grace: the pipe does not even exist while the child
            // is loading its dictionary/model, so failures don't count —
            // but a child that NEVER comes up is itself a hang
            if now.duration_since(self.started_at) <= STARTUP_GRACE {
                return Verdict::Healthy;
            }
            return Verdict::Hung;
        }

        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_PING_FAILURES {
            Verdict::Hung
        } else {
            Verdict::Healthy
        }
    }
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

fn start_process(exe: &str, prefix: &str) -> Option<Child> {
    let mut child = match Command::new(exe)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to start {}: {}", exe, e);
            return None;
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let prefix = prefix.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("{}: {}", prefix, line);
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let prefix = prefix.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{}: {}", prefix, line);
            }
        });
    }

    Some(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn second_mutex_holder_detects_the_first() {
        // a test-only name so a real running launcher can't interfere
        let name = w!("Local\\AzookeyLauncherSingletonTest");
        assert_eq!(another_instance_running(name).unwrap(), false);
        // the first handle is still open in this process, so a second
        // acquisition sees ERROR_ALREADY_EXISTS — same as a second process
        assert_eq!(another_instance_running(name).unwrap(), true);
    }

    #[test]
    fn failures_during_startup_grace_are_not_counted() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        for i in 1..=10 {
            assert_eq!(
                policy.on_ping_result(false, at(base, i * 10)),
                Verdict::Healthy,
                "failure at {}s should be within grace",
                i * 10
            );
        }
    }

    #[test]
    fn never_becoming_healthy_past_grace_is_hung() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        assert_eq!(policy.on_ping_result(false, at(base, 60)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 121)), Verdict::Hung);
    }

    #[test]
    fn hang_needs_consecutive_failures_after_health() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        assert_eq!(policy.on_ping_result(true, at(base, 10)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 20)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 30)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 40)), Verdict::Hung);
    }

    #[test]
    fn one_success_resets_the_failure_streak() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        assert_eq!(policy.on_ping_result(true, at(base, 10)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 20)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 30)), Verdict::Healthy);
        // recovers just in time
        assert_eq!(policy.on_ping_result(true, at(base, 40)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 50)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 60)), Verdict::Healthy);
        assert_eq!(policy.on_ping_result(false, at(base, 70)), Verdict::Hung);
    }

    #[test]
    fn success_after_grace_still_arms_normally() {
        let base = Instant::now();
        let mut policy = WatchdogPolicy::new(base);

        // slow startup, first success arrives after the grace window would
        // have expired for failures
        assert_eq!(
            policy.on_ping_result(false, at(base, 100)),
            Verdict::Healthy
        );
        assert_eq!(policy.on_ping_result(true, at(base, 110)), Verdict::Healthy);
        assert_eq!(
            policy.on_ping_result(false, at(base, 200)),
            Verdict::Healthy
        );
        assert_eq!(
            policy.on_ping_result(false, at(base, 210)),
            Verdict::Healthy
        );
        assert_eq!(policy.on_ping_result(false, at(base, 220)), Verdict::Hung);
    }
}
