mod ipc;
mod trace;

use serde::{Deserialize, Serialize};
use shared::AppConfig;
use std::{
    path::PathBuf,
    sync::{Mutex, PoisonError},
};

#[derive(Debug)]
pub struct AppState {
    // connected lazily: the server may not be running when the settings
    // app starts, and that must not crash or hang the app
    ipc: Mutex<Option<ipc::IPCService>>,
}

impl AppState {
    fn new() -> Self {
        // creates or migrates settings.json once, at startup; every later
        // read goes back to the file
        AppConfig::new();
        AppState {
            ipc: Mutex::new(None),
        }
    }
}

/// Read from disk on every call rather than from a snapshot taken at
/// startup. The frontend saves by reading the whole config, changing one
/// key and writing it back, so a snapshot means anything edited elsewhere
/// while the settings app is open — by hand, or by a future version of the
/// app — is silently reverted the next time the user flips a switch.
#[tauri::command]
fn get_config() -> Result<AppConfig, String> {
    AppConfig::try_read()
}

/// The sections a save has to carry. `AppConfig` is deliberately tolerant of
/// a settings FILE that is missing them — an older build wrote it, and the
/// defaults are the right answer there. At this boundary the same tolerance
/// means something else entirely: a payload that omits `zenzai` gets the
/// defaults filled in and then WRITTEN, so a frontend bug that dropped a
/// section from the request would quietly reset every setting in it.
const REQUIRED_SECTIONS: [&str; 2] = ["zenzai", "plugins"];

/// The config to save, or why this payload is not one.
fn config_from_payload(payload: serde_json::Value) -> Result<AppConfig, String> {
    let object = payload
        .as_object()
        .ok_or("the settings payload is not an object")?;
    for section in REQUIRED_SECTIONS {
        if !object
            .get(section)
            .is_some_and(serde_json::Value::is_object)
        {
            return Err(format!(
                "the settings payload has no \"{section}\" section; refusing to save, \
                 because writing it would reset every setting in it"
            ));
        }
    }
    serde_json::from_value(payload).map_err(|e| format!("the settings payload is unusable: {e}"))
}

#[tauri::command]
fn update_config(
    state: tauri::State<AppState>,
    new_config: serde_json::Value,
) -> Result<(), String> {
    let new_config = config_from_payload(new_config)?;
    // refused when settings.json was written by a newer build — report it
    // instead of silently dropping the keys we do not know about
    new_config.write()?;

    // the settings file is already saved at this point; notifying the
    // server is best-effort and reported to the frontend on failure
    let mut ipc_guard = state.ipc.lock().unwrap_or_else(PoisonError::into_inner);

    if ipc_guard.is_none() {
        match ipc::IPCService::new() {
            Ok(service) => *ipc_guard = Some(service),
            Err(e) => return Err(format!("cannot connect to azookey server: {e}")),
        }
    }

    if let Some(ipc) = ipc_guard.as_mut()
        && let Err(e) = ipc.update_config()
    {
        // drop the broken connection so the next call reconnects
        *ipc_guard = None;
        return Err(format!("failed to notify azookey server: {e}"));
    }

    Ok(())
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Capability {
    cpu: bool,
    cuda: bool,
    vulkan: bool,
}

#[tauri::command]
fn check_capability() -> Capability {
    // cuda:
    // cudart64_12.dll
    // cublas64_12.dll

    // vulkan:
    // vulkan-1.dllの存在確認

    let mut capability = Capability {
        cpu: true,
        cuda: false,
        vulkan: false,
    };

    // Check for CUDA availability
    let cuda_files = ["cudart64_12.dll", "cublas64_12.dll"];
    let cuda_available = cuda_files.iter().all(|file| {
        // Check if the file exists in system path or in the current directory
        std::env::var("PATH")
            .unwrap_or_default()
            .split(';')
            .map(PathBuf::from)
            .chain(std::iter::once(std::env::current_dir().unwrap_or_default()))
            .any(|path| path.join(file).exists())
    });
    capability.cuda = cuda_available;

    // Check for Vulkan availability
    let vulkan_file = "vulkan-1.dll";
    let vulkan_available = std::env::var("PATH")
        .unwrap_or_default()
        .split(';')
        .map(PathBuf::from)
        .chain(std::iter::once(std::env::current_dir().unwrap_or_default()))
        .any(|path| path.join(vulkan_file).exists());
    capability.vulkan = vulkan_available;

    capability
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // before AppState::new(), which is what migrates settings.json and so the
    // first thing with something to report
    trace::setup_logger();
    let app_state = AppState::new();

    tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_config,
            update_config,
            check_capability
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::config_from_payload;

    /// A whole document saves, and the round trip keeps what it was given.
    #[test]
    fn a_complete_payload_is_accepted() {
        let config = config_from_payload(serde_json::json!({
            "version": "0.1.0",
            "zenzai": {
                "enable": true,
                "profile": "p",
                "backend": "cuda",
                "inference_limit": 3,
                "topic": "t",
                "style": "s",
                "preference": "f"
            },
            "plugins": { "enable": true, "entries": [] }
        }))
        .expect("a complete payload is savable");

        assert!(config.zenzai.enable);
        assert_eq!(config.zenzai.backend, "cuda");
        assert_eq!(config.zenzai.inference_limit, 3);
        assert!(config.plugins.enable);
    }

    /// The bug: `AppConfig` fills a missing section in with defaults, which is
    /// right for a FILE an older build wrote and wrong for a request, because
    /// the defaults then get written over what the user had.
    #[test]
    fn a_payload_missing_a_section_is_refused() {
        for payload in [
            serde_json::json!({ "version": "0.1.0", "plugins": { "enable": true } }),
            serde_json::json!({ "version": "0.1.0", "zenzai": { "enable": true } }),
            serde_json::json!({ "version": "0.1.0" }),
        ] {
            let error = config_from_payload(payload.clone())
                .expect_err("an incomplete payload must not be saved");
            assert!(
                error.contains("refusing to save"),
                "for {payload}: got {error}"
            );
        }
    }

    /// A section of the wrong shape is not a section. `serde(default)` would
    /// not have caught this either — it would have failed the whole parse,
    /// which is a worse message for the same refusal.
    #[test]
    fn a_section_that_is_not_an_object_is_refused() {
        let error = config_from_payload(serde_json::json!({
            "version": "0.1.0",
            "zenzai": "on",
            "plugins": { "enable": false }
        }))
        .expect_err("a payload whose section is not an object must not be saved");
        assert!(error.contains("zenzai"), "got {error}");
    }
}
