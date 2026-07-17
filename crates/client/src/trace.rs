use std::fmt::Write as _;
use tracing::field::{Field, Visit};
use tracing_core::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt};
use windows::{core::PCWSTR, Win32::System::Diagnostics::Debug::OutputDebugStringW};

use crate::extension::StringExt as _;
use crate::globals::DllModule;
use crate::tracing_chrome::{ChromeLayerBuilder, EventOrSpan};

fn log_folder() -> Option<std::path::PathBuf> {
    // %LOCALAPPDATA%\Azookey\logs — never a hardcoded dev-machine path
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(std::path::Path::new(&base).join("Azookey").join("logs"))
}

pub struct StringVisitor<'a> {
    string: &'a mut String,
}

impl<'a> Visit for StringVisitor<'a> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // do nothing
        if field.name() == "message" {
            write!(self.string, "{:?}", value).unwrap();
        }
    }
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

pub fn setup_logger() -> anyhow::Result<()> {
    #[cfg(not(debug_assertions))]
    {
        // release: warnings and errors only, no file I/O
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
        return Ok(());
    }
    let Some(folder) = log_folder() else {
        return Ok(());
    };
    if std::fs::create_dir_all(&folder).is_err() {
        return Ok(());
    }
    let timestamp = chrono::Local::now().format("%Y-%m-%d-%H.%M.%S");
    let path = folder.join(format!("{}.json", timestamp));

    let writer = {
        if let Ok(file) = std::fs::File::create(&path) {
            file
        } else {
            return Ok(());
        }
    };

    let builder = ChromeLayerBuilder::new()
        .file(writer)
        .include_locations(true)
        .include_args(true)
        .name_fn(Box::new(|event_or_span| match event_or_span {
            EventOrSpan::Event(event) => {
                let message = {
                    let mut message = String::new();
                    event.record(&mut StringVisitor {
                        string: &mut message,
                    });
                    message
                };

                let (level, file, line) = {
                    let metadeta = event.metadata();
                    let level = metadeta.level().as_str();
                    let file = metadeta.file().unwrap_or_default();
                    let line = metadeta.line().unwrap_or_default();

                    (level, file, line)
                };

                let str = format!("[{}: {}:{}] {}", level, file, line, message);
                let wide: Vec<u16> = str.as_str().to_wide_16();
                unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };

                message
            }
            EventOrSpan::Span(span) => span.metadata().name().to_string(),
        }));

    let (chrome_layer, sender) = builder.build();

    DllModule::get()?.sender = Some(sender);

    // ignore traces from other crates
    let filter = Targets::new()
        .with_target("azookey_windows", LevelFilter::DEBUG)
        .with_default(LevelFilter::OFF);

    tracing_subscriber::registry()
        .with(filter)
        .with(chrome_layer)
        .init();

    Ok(())
}
