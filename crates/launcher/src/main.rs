use shared::AppConfig;
use std::env;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
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

/// keep at most this many launcher session logs
const MAX_LOG_FILES: usize = 10;

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
fn init_child_job() {
    let job = unsafe {
        match CreateJobObjectW(None, PCWSTR::null()) {
            Ok(job) => job,
            Err(e) => {
                log_err(&format!("CreateJobObject failed ({e}); children won't be tied to the launcher's lifetime"));
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
fn assign_to_child_job(child: &Child, prefix: &str) {
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

// The launcher normally runs headless from the logon scheduled task, so
// console output is lost — everything is also teed into
// %LOCALAPPDATA%\Azookey\logs\launcher-<timestamp>-<pid>.log. This is the
// only record of server crashes, watchdog kills, and restarts in the field.
static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

fn init_log_file() {
    let Some(base) = env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = Path::new(&base).join("Azookey").join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    prune_old_logs(&dir);

    let name = format!(
        "launcher-{}-{}.log",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    );
    if let Ok(file) = std::fs::File::create(dir.join(name)) {
        let _ = LOG_FILE.set(Mutex::new(file));
    }
}

fn prune_old_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("launcher-") && n.ends_with(".log"))
        })
        .collect();
    // timestamped names sort chronologically
    logs.sort();
    if logs.len() >= MAX_LOG_FILES {
        for old in &logs[..logs.len() + 1 - MAX_LOG_FILES] {
            let _ = std::fs::remove_file(old);
        }
    }
}

fn log_to_file(line: &str) {
    if let Some(file) = LOG_FILE.get() {
        let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(
            file,
            "[{}] {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            line
        );
    }
}

fn log_info(line: &str) {
    println!("{line}");
    log_to_file(line);
}

fn log_err(line: &str) {
    eprintln!("{line}");
    log_to_file(line);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_log_file();

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
    init_child_job();

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
    let server_handle = tokio::spawn(run_supervisor(
        "azookey-server.exe",
        "[server]",
        shared::pipe::server_pipe(),
    ));
    let ui_handle = tokio::spawn(run_supervisor("ui.exe", "[ui]", shared::pipe::ui_pipe()));

    let _ = server_handle.await;
    let _ = ui_handle.await;

    Ok(())
}

/// Runs one child's supervisor and, if it gives up, ends the whole launcher.
///
/// If a supervisor gives up (spawn failure, or a crash/hang loop that blew
/// its budget) the IME is dead until something restarts it — but the
/// single-instance mutex is held for as long as this launcher lives, so a
/// manual relaunch would just exit immediately. Exiting the process releases
/// that mutex (letting a fresh launch recover) and, via the job's
/// KILL_ON_JOB_CLOSE, tears down the other child so it can't linger and hold
/// the pipe names.
async fn run_supervisor(exe: &'static str, prefix: &'static str, pipe_name: String) {
    if supervise(exe, prefix, &pipe_name).await == SuperviseOutcome::GaveUp {
        log_err(&format!(
            "{prefix} is unrecoverable; exiting the launcher so a fresh start can take over"
        ));
        std::process::exit(1);
    }
}

/// Why a supervisor loop stopped.
#[derive(Debug, PartialEq, Eq)]
enum SuperviseOutcome {
    /// The child exited cleanly and on purpose (e.g. the UI re-executing
    /// itself with a UIAccess token). Nothing to recover.
    Exited,
    /// The child is unrecoverable — spawn failure, or a crash/hang loop that
    /// exhausted the restart budget. The launcher should stand down.
    GaveUp,
}

/// Keeps a child process running: restarts it when it exits abnormally or
/// stops answering health checks, with exponential backoff, and gives up on
/// a tight crash/hang loop.
async fn supervise(exe: &'static str, prefix: &'static str, pipe_name: &str) -> SuperviseOutcome {
    let mut recent_restarts: Vec<Instant> = Vec::new();
    let mut backoff = Duration::from_secs(1);
    let mut consecutive_watchdog_kills: u32 = 0;

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

        // a hang loop is slower than the 60s crash window (detection alone
        // takes ~45s), so count watchdog kills separately: reaching a
        // healthy state is what proves a restart was worthwhile
        if hung && !saw_healthy.load(Ordering::SeqCst) {
            consecutive_watchdog_kills += 1;
            if consecutive_watchdog_kills >= MAX_CONSECUTIVE_WATCHDOG_KILLS {
                log_err(&format!(
                    "{prefix} was killed by the watchdog {MAX_CONSECUTIVE_WATCHDOG_KILLS} times without ever becoming healthy; giving up"
                ));
                return SuperviseOutcome::GaveUp;
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
            log_err(&format!(
                "{prefix} crashed {MAX_RESTARTS_IN_WINDOW} times within {RESTART_WINDOW:?}; giving up"
            ));
            return SuperviseOutcome::GaveUp;
        }
        recent_restarts.push(now);

        log_err(&format!("{prefix} restarting in {backoff:?}"));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
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
            log_err(&format!("Failed to start {}: {}", exe, e));
            return None;
        }
    };

    // tie the child to the launcher's lifetime before anything else, so a
    // launcher crash in the next instant still can't orphan it
    assign_to_child_job(&child, prefix);

    if let Some(stdout) = child.stdout.take() {
        let prefix = prefix.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log_info(&format!("{}: {}", prefix, line));
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let prefix = prefix.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log_err(&format!("{}: {}", prefix, line));
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
    fn prune_keeps_only_the_newest_logs() {
        let dir = std::env::temp_dir().join(format!("azk-prune-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for day in 1..=12 {
            let name = format!("launcher-202601{day:02}-000000-1.log");
            std::fs::write(dir.join(name), "x").unwrap();
        }
        std::fs::write(dir.join("unrelated.txt"), "x").unwrap();

        prune_old_logs(&dir);

        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("launcher-"))
            .collect();
        remaining.sort();

        // room is left for the new session's file: 12 -> MAX_LOG_FILES - 1
        assert_eq!(remaining.len(), MAX_LOG_FILES - 1);
        // the oldest files are the ones deleted
        assert!(remaining[0].contains("20260104"));
        // non-log files are untouched
        assert!(dir.join("unrelated.txt").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn second_mutex_holder_detects_the_first() {
        // a test-only name so a real running launcher can't interfere
        let name = w!("Local\\AzookeyLauncherSingletonTest");
        assert!(!another_instance_running(name).unwrap());
        // the first handle is still open in this process, so a second
        // acquisition sees ERROR_ALREADY_EXISTS — same as a second process
        assert!(another_instance_running(name).unwrap());
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
