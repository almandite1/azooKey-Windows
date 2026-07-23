use tracing_core::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt};

// release-only: DebugOutputWriter forwards to OutputDebugStringW
#[cfg(not(debug_assertions))]
use crate::extension::StringExt as _;
#[cfg(not(debug_assertions))]
use windows::{Win32::System::Diagnostics::Debug::OutputDebugStringW, core::PCWSTR};

// debug-only: file logging (and its folder) exist only in debug builds
#[cfg(debug_assertions)]
fn log_folder() -> Option<std::path::PathBuf> {
    // %LOCALAPPDATA%\Azookey\logs — never a hardcoded dev-machine path
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::Path::new(&base).join("Azookey").join("logs"))
}

/// Forwards formatted tracing output to OutputDebugStringW. Used in release
/// builds: the DLL runs inside every application, so writing log FILES from
/// here would contend across processes — debugger output is side-effect-free
/// and can be captured in the field with DebugView when diagnosing.
#[cfg(not(debug_assertions))]
struct DebugOutputWriter;

#[cfg(not(debug_assertions))]
impl std::io::Write for DebugOutputWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let wide: Vec<u16> = format!("azookey: {}", text).as_str().to_wide_16();
        unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Two cfg'd definitions instead of cfg blocks inside one body: with blocks,
// whichever compiles last ends with a `return` that release clippy flags as
// needless (the other block vanishes) — separate functions sidestep that.

/// release: warnings and errors only to OutputDebugStringW, no file I/O
#[cfg(not(debug_assertions))]
pub fn setup_logger() -> anyhow::Result<()> {
    let filter = Targets::new()
        .with_target("azookey_windows", LevelFilter::WARN)
        .with_default(LevelFilter::OFF);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_writer(|| DebugOutputWriter);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init();
    Ok(())
}

/// debug: full trace to a per-PID text log file
#[cfg(debug_assertions)]
pub fn setup_logger() -> anyhow::Result<()> {
    {
        let Some(folder) = log_folder() else {
            return Ok(());
        };
        if std::fs::create_dir_all(&folder).is_err() {
            return Ok(());
        }
        // Plain per-process text log. The old ChromeLayer JSON writer corrupted
        // its own output (duplicated ".json.json..." filenames, truncated/empty
        // files), which made field diagnosis impossible. One appendable text
        // file per PID is robust and greppable. The host exe's name is part of
        // the filename: a PID alone cannot be attributed once the process
        // exits, which made per-host diagnosis (which app rejected the TIP?)
        // impossible in the field.
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "unknown".to_string());
        let path = folder.join(format!("client-{}-{}.log", exe, std::process::id()));
        let Ok(file) = std::fs::File::create(&path) else {
            return Ok(());
        };

        // ignore traces from other crates
        let filter = Targets::new()
            .with_target("azookey_windows", LevelFilter::DEBUG)
            .with_default(LevelFilter::OFF);

        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            // log every instrumented call on entry (with its arguments):
            // field diagnosis needs to see which TSF callbacks fired and
            // with what key codes, not only the explicit debug! events
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
            .with_writer(std::sync::Mutex::new(file));

        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init();

        Ok(())
    }
}
