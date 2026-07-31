mod ipc;
mod trace;

use serde::{Deserialize, Serialize};
use shared::AppConfig;
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

#[derive(Debug)]
pub struct AppState {
    // connected lazily: the server may not be running when the settings
    // app starts, and that must not crash or hang the app
    ipc: Mutex<Option<ipc::IPCService>>,
    /// Held across the read-modify-write of a patch.
    ///
    /// Every save reads the file, changes one key and writes it back. Two of
    /// those interleaving lose one of the two changes, and the settings app
    /// issues them as fast as a user can click.
    save: Mutex<()>,
}

impl AppState {
    fn new() -> Self {
        // Deliberately NOT AppConfig::new(). That creates-or-migrates the
        // file, and its recovery path moves an unreadable settings.json aside
        // and writes the defaults — so merely OPENING the settings app threw
        // away a broken file before the user was told anything about it, and
        // before `get_config` had a chance to report it. Resetting is now
        // something the user asks for; see `reset_config`.
        AppState {
            ipc: Mutex::new(None),
            save: Mutex::new(()),
        }
    }
}

/// What a save actually achieved, rather than one bit for both halves.
///
/// The file and the running IME are two different things to have failed at,
/// and treating them as one is what put the settings app out of step with
/// the disk: a save that wrote the file and then could not reach the IME
/// came back as a plain error, so the UI put the switch back — while the
/// file said the opposite, and the next restart applied it.
#[derive(Debug, Serialize)]
pub struct SaveOutcome {
    /// The file on disk now holds the change.
    saved: bool,
    /// The running IME has been told. False with `saved` true means the
    /// change takes effect when it next starts.
    notified: bool,
    /// Present when something went wrong, for the UI to show.
    error: Option<String>,
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

/// The sections a config document has to carry. `AppConfig` is deliberately
/// tolerant of a settings FILE that is missing them — an older build wrote
/// it, and the defaults are the right answer there. Anywhere we are about to
/// WRITE, the same tolerance means something else entirely: a document that
/// has lost a section gets the defaults filled in and then persisted, which
/// resets every setting in it.
const REQUIRED_SECTIONS: [&str; 2] = ["zenzai", "plugins"];

/// The config to save, or why this document is not one.
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

/// Sets one dotted path in a settings document.
///
/// Every segment but the last has to exist and be an object: a patch is a
/// change to a setting that is already there, so a typo'd path is a mistake
/// to report rather than a new key to invent. The last segment must exist
/// too, for the same reason.
///
/// It also must not be a whole SECTION. Replacing `zenzai` in one go would be
/// a patch in name only — it carries every other key in that section along
/// with it, which is the lost update this exists to prevent.
fn set_at_path(
    document: &mut serde_json::Value,
    key_path: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let mut segments: Vec<&str> = key_path.split('.').collect();
    let leaf = segments
        .pop()
        .filter(|segment| !segment.is_empty())
        .ok_or("the settings key is empty")?;

    let mut cursor = document;
    for segment in segments {
        cursor = cursor
            .get_mut(segment)
            .filter(|section| section.is_object())
            .ok_or_else(|| format!("settings have no section \"{segment}\" (in \"{key_path}\")"))?;
    }
    let object = cursor
        .as_object_mut()
        .ok_or_else(|| format!("\"{key_path}\" does not name a setting"))?;
    match object.get(leaf) {
        None => return Err(format!("settings have no key \"{key_path}\"")),
        Some(current) if current.is_object() => {
            return Err(format!(
                "\"{key_path}\" is a section, not a setting; patch the keys inside it \
                 instead, or the other keys in it are overwritten with whatever the \
                 caller happened to be holding"
            ));
        }
        Some(_) => {}
    }
    object.insert(leaf.to_string(), value);
    Ok(())
}

/// Read, change one key, write — all on this side of the boundary.
///
/// The frontend used to do the read-modify-write itself and send the whole
/// document back, which had three problems at once: two saves in flight lost
/// one of the two changes, a request that dropped a section reset it, and the
/// round trip meant the value written was computed from what the UI had read
/// some time earlier. A patch says only what changed.
fn patch_on_disk(key_path: &str, value: serde_json::Value) -> Result<(), String> {
    let current = AppConfig::try_read()?;
    let mut document = serde_json::to_value(&current)
        .map_err(|e| format!("could not read the current settings back: {e}"))?;
    set_at_path(&mut document, key_path, value)?;
    let patched = config_from_payload(document)?;
    // refused when settings.json was written by a newer build — report it
    // instead of silently dropping the keys we do not know about
    patched.write()
}

#[tauri::command]
async fn patch_config(
    state: tauri::State<'_, AppState>,
    key_path: String,
    value: serde_json::Value,
) -> Result<SaveOutcome, String> {
    {
        let _saving = state.save.lock().unwrap_or_else(PoisonError::into_inner);
        patch_on_disk(&key_path, value)?;
    }
    Ok(notify_server(&state))
}

/// Moves an unreadable settings.json aside and starts again from the
/// defaults — the thing that used to happen the moment the app opened.
///
/// It is a command now because it destroys something: it is only correct
/// once a user has been shown that their file could not be read and has said
/// to go ahead.
#[tauri::command]
async fn reset_config(state: tauri::State<'_, AppState>) -> Result<SaveOutcome, String> {
    {
        let _saving = state.save.lock().unwrap_or_else(PoisonError::into_inner);
        // this is the call whose recovery path backs the old file up first
        AppConfig::new();
    }
    Ok(notify_server(&state))
}

/// Tells the running IME to re-read the file. Never fatal: the file is
/// already saved by the time this runs, so the worst case is a change that
/// takes effect at the next start, and saying so is the whole job here.
fn notify_server(state: &AppState) -> SaveOutcome {
    let mut ipc_guard = state.ipc.lock().unwrap_or_else(PoisonError::into_inner);

    if ipc_guard.is_none() {
        match ipc::IPCService::new() {
            Ok(service) => *ipc_guard = Some(service),
            Err(e) => {
                return SaveOutcome {
                    saved: true,
                    notified: false,
                    error: Some(format!(
                        "saved, but the IME could not be reached ({e}); it will pick the \
                         settings up when it next starts"
                    )),
                };
            }
        }
    }

    let Some(ipc) = ipc_guard.as_mut() else {
        return SaveOutcome {
            saved: true,
            notified: false,
            error: Some("saved, but the IME could not be reached".to_string()),
        };
    };

    match ipc.update_config() {
        Ok(()) => SaveOutcome {
            saved: true,
            notified: true,
            error: None,
        },
        Err(failure) => {
            let message = failure.message().to_string();
            // only a dead connection costs the channel; a timeout is the
            // single-threaded engine being busy, and rebuilding the runtime
            // for that is what made the next keystroke expensive
            if failure.connection_lost() {
                *ipc_guard = None;
            }
            SaveOutcome {
                saved: true,
                notified: false,
                error: Some(format!("saved, but {message}")),
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Capability {
    cpu: bool,
    cuda: bool,
    vulkan: bool,
}

/// The directories a backend's DLLs could be found in: everything on `PATH`,
/// plus where this executable lives.
///
/// NOT the current directory, which is what it used to be. The settings app
/// is normally started from the language-bar menu, and a process started that
/// way inherits the cwd of whatever text application the menu was opened in —
/// so the extra directory searched was a word processor's documents folder or
/// worse. Next to the executable is where the backends actually ship.
fn search_directories(path_var: &str, executable: Option<&Path>) -> Vec<PathBuf> {
    path_var
        .split(';')
        .filter(|entry| !entry.trim().is_empty())
        .map(PathBuf::from)
        .chain(executable.and_then(Path::parent).map(Path::to_path_buf))
        .collect()
}

/// Whether every one of `files` is in one of `directories`.
///
/// Taken as arguments so this is testable: the two backend probes were the
/// same six-line PATH walk written out twice, and neither could be exercised
/// without arranging the machine's own PATH.
fn all_present(directories: &[PathBuf], files: &[&str]) -> bool {
    files
        .iter()
        .all(|file| directories.iter().any(|dir| dir.join(file).exists()))
}

fn capability_in(directories: &[PathBuf]) -> Capability {
    Capability {
        // the CPU backend ships with the product and needs nothing found
        cpu: true,
        cuda: all_present(directories, &["cudart64_12.dll", "cublas64_12.dll"]),
        vulkan: all_present(directories, &["vulkan-1.dll"]),
    }
}

#[tauri::command]
fn check_capability() -> Capability {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let executable = std::env::current_exe().ok();
    capability_in(&search_directories(&path_var, executable.as_deref()))
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
            patch_config,
            reset_config,
            check_capability
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, Capability, all_present, capability_in, config_from_payload, search_directories,
    };
    use std::path::{Path, PathBuf};

    fn document() -> serde_json::Value {
        serde_json::json!({
            "version": "0.1.0",
            "zenzai": {
                "enable": false, "profile": "", "backend": "cpu",
                "inference_limit": 1, "context_size": 1024,
                "topic": "", "style": "", "preference": ""
            },
            "plugins": { "enable": false, "entries": [] }
        })
    }

    fn patched(key_path: &str, value: serde_json::Value) -> Result<serde_json::Value, String> {
        let mut doc = document();
        super::set_at_path(&mut doc, key_path, value)?;
        Ok(doc)
    }

    /// A patch changes the key it names and nothing else. That is the whole
    /// reason it exists: the frontend used to send the whole document back,
    /// so every save carried a full copy of state it had read some time
    /// earlier, and two saves in flight lost one of the two changes.
    #[test]
    fn a_patch_changes_only_the_key_it_names() {
        let doc = patched("zenzai.profile", serde_json::json!("私は猫だ")).expect("patch");

        assert_eq!(doc["zenzai"]["profile"], "私は猫だ");
        assert_eq!(doc["zenzai"]["backend"], "cpu", "its neighbours are intact");
        assert_eq!(doc["zenzai"]["enable"], false);
        assert_eq!(
            doc["plugins"]["enable"], false,
            "and so is the other section"
        );
        assert_eq!(doc["version"], "0.1.0");
    }

    #[test]
    fn a_patch_reaches_either_section() {
        assert_eq!(
            patched("plugins.enable", serde_json::json!(true)).expect("patch")["plugins"]["enable"],
            true
        );
        assert_eq!(
            patched("zenzai.inference_limit", serde_json::json!(5)).expect("patch")["zenzai"]["inference_limit"],
            5
        );
    }

    /// A path that names nothing is a mistake in the caller, and inventing the
    /// key would persist that mistake into the user's settings file.
    #[test]
    fn a_patch_to_a_key_that_does_not_exist_is_refused() {
        for bad in [
            "zenzai.nonesuch",
            "nonesuch.enable",
            "zenzai.profile.deeper",
            "",
            "zenzai",
        ] {
            assert!(
                patched(bad, serde_json::json!(true)).is_err(),
                "\"{bad}\" should not be patchable"
            );
        }
    }

    /// The result of a patch still has to be a whole, savable document.
    #[test]
    fn a_patched_document_is_still_a_complete_config() {
        let doc = patched("zenzai.enable", serde_json::json!(true)).expect("patch");
        let config = config_from_payload(doc).expect("a patched document must be savable");
        assert!(config.zenzai.enable);
    }

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
                "context_size": 2048,
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
        assert_eq!(config.zenzai.context_size, 2048);
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

    /// The backend probe searched `PATH` plus the CURRENT DIRECTORY, and the
    /// settings app is normally started from the language-bar menu — which
    /// gives it the cwd of whatever text application the menu was opened in.
    /// So the extra directory was some document folder, and the one place the
    /// backends actually ship was not searched at all.
    #[test]
    fn the_backend_search_looks_next_to_the_executable_not_in_the_cwd() {
        let dirs = search_directories(
            r"C:\Windows\System32;C:\tools",
            Some(Path::new(r"C:\Program Files\Azookey\Azookey.exe")),
        );

        assert!(dirs.contains(&PathBuf::from(r"C:\Program Files\Azookey")));
        assert!(dirs.contains(&PathBuf::from(r"C:\Windows\System32")));
        assert!(dirs.contains(&PathBuf::from(r"C:\tools")));
        assert!(
            !dirs.contains(&std::env::current_dir().unwrap_or_default()),
            "the current directory is not ours to interpret"
        );
    }

    /// An empty or absent PATH must not turn into a search of the drive root:
    /// splitting "" on ';' yields one empty entry, and joining a filename onto
    /// it produces a relative path that resolves against the cwd again.
    #[test]
    fn empty_path_entries_are_dropped() {
        assert!(search_directories("", None).is_empty());
        assert_eq!(search_directories(r"C:\a;;  ;C:\b", None).len(), 2);
    }

    /// The TypeScript `AppConfig` is hand-written, and a key path that does
    /// not exist is refused by `patch_config` at runtime — which the user sees
    /// as a setting that silently does not work. The two spellings are pinned
    /// to each other here so a rename on the Rust side fails the build instead.
    #[test]
    fn the_typescript_config_names_the_same_keys_as_the_rust_one() {
        let source = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/lib/config.ts"),
        )
        .expect("read frontend/src/lib/config.ts");

        // the fields of one `export interface Name { ... }` block
        let fields_of = |name: &str| -> Vec<String> {
            let start = source
                .find(&format!("export interface {name} {{"))
                .unwrap_or_else(|| panic!("config.ts should declare {name}"));
            let body = &source[start..];
            let end = body.find('}').expect("the interface should be closed");
            body[..end]
                .lines()
                .skip(1)
                .filter_map(|line| line.split(':').next())
                .map(|field| field.trim().trim_end_matches('?').to_string())
                .filter(|field| !field.is_empty() && !field.starts_with("//"))
                .collect()
        };

        let document = serde_json::to_value(AppConfig::default()).expect("serialize the defaults");
        let object = document.as_object().expect("the config is an object");

        for (interface, section) in [("ZenzaiConfig", "zenzai"), ("PluginsConfig", "plugins")] {
            let mut declared = fields_of(interface);
            let mut actual: Vec<String> = object[section]
                .as_object()
                .unwrap_or_else(|| panic!("{section} is an object"))
                .keys()
                .cloned()
                .collect();
            declared.sort();
            actual.sort();
            assert_eq!(
                declared, actual,
                "{interface} in config.ts does not match shared::AppConfig's {section}"
            );
        }

        let mut declared = fields_of("AppConfig");
        let mut actual: Vec<String> = object.keys().cloned().collect();
        declared.sort();
        actual.sort();
        assert_eq!(declared, actual, "AppConfig in config.ts has drifted");
    }

    /// CUDA needs BOTH of its DLLs; one is not a working backend.
    #[test]
    fn a_backend_needs_every_dll_it_names() {
        let dir = std::env::temp_dir().join(format!("azk-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the fixture directory");
        let dirs = vec![dir.clone()];

        std::fs::write(dir.join("cudart64_12.dll"), "x").expect("write a fixture dll");
        assert!(
            !all_present(&dirs, &["cudart64_12.dll", "cublas64_12.dll"]),
            "half of CUDA is not CUDA"
        );

        std::fs::write(dir.join("cublas64_12.dll"), "x").expect("write a fixture dll");
        assert!(all_present(&dirs, &["cudart64_12.dll", "cublas64_12.dll"]));

        let capability: Capability = capability_in(&dirs);
        assert!(capability.cpu, "the CPU backend ships with the product");
        assert!(capability.cuda);
        assert!(!capability.vulkan);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
