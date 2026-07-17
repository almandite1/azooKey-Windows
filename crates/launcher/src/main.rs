use shared::AppConfig;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, thread};

/// give up when a child keeps crashing this many times within RESTART_WINDOW
const MAX_RESTARTS_IN_WINDOW: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

fn main() -> anyhow::Result<()> {
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

    // a crashed server would otherwise leave the IME dead in every
    // application until re-login, so both children are supervised and
    // restarted with backoff
    let server_handle = thread::spawn(|| supervise("azookey-server.exe", "[server]"));
    let ui_handle = thread::spawn(|| supervise("ui.exe", "[ui]"));

    let _ = server_handle.join();
    let _ = ui_handle.join();

    Ok(())
}

/// Keeps a child process running: restarts it when it exits abnormally,
/// with exponential backoff, and gives up on a tight crash loop.
fn supervise(exe: &str, prefix: &str) {
    let mut recent_restarts: Vec<Instant> = Vec::new();
    let mut backoff = Duration::from_secs(1);

    loop {
        let Some(mut child) = start_process(exe, prefix) else {
            // spawn failure (e.g. missing binary) won't fix itself
            eprintln!("{prefix} could not be started; giving up");
            return;
        };

        let started_at = Instant::now();
        match child.wait() {
            Ok(status) if status.success() => {
                println!("{prefix} exited normally");
                return;
            }
            Ok(status) => eprintln!("{prefix} exited abnormally: {status}"),
            Err(e) => {
                eprintln!("{prefix} wait failed: {e}");
                return;
            }
        }

        // a stable stretch resets the backoff
        if started_at.elapsed() >= RESTART_WINDOW {
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
        thread::sleep(backoff);
        backoff = (backoff * 2).min(MAX_BACKOFF);
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
        let stdout_reader = BufReader::new(stdout);
        let prefix_stdout = prefix.to_string();
        thread::spawn(move || {
            for line in stdout_reader.lines().map_while(Result::ok) {
                println!("{}: {}", prefix_stdout, line);
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let stderr_reader = BufReader::new(stderr);
        let prefix_stderr = prefix.to_string();
        thread::spawn(move || {
            for line in stderr_reader.lines().map_while(Result::ok) {
                eprintln!("{}: {}", prefix_stderr, line);
            }
        });
    }

    Some(child)
}
