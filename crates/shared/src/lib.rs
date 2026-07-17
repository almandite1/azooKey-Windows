use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod pipe;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/azookey.rs"));
    include!(concat!(env!("OUT_DIR"), "/window.rs"));
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("azookey_service_descriptor");
}

fn get_config_root() -> PathBuf {
    // APPDATA is always set on a normal Windows session; fall back to a
    // relative path rather than panicking in whichever process loads us
    let appdata = PathBuf::from(std::env::var("APPDATA").unwrap_or_default());
    appdata.join("Azookey")
}

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ZenzaiConfig {
    pub enable: bool,
    pub profile: String,
    pub backend: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub version: String,
    pub zenzai: ZenzaiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            version: "0.1.0".to_string(),
            zenzai: ZenzaiConfig {
                enable: false,
                profile: "".to_string(),
                backend: "cpu".to_string(),
            },
        }
    }
}

impl AppConfig {
    pub fn write(&self) {
        let config_path = get_config_root().join(SETTINGS_FILENAME);
        // failure to persist settings must not take down the IME
        match serde_json::to_string_pretty(self) {
            Ok(config_str) => {
                if let Err(e) = std::fs::write(&config_path, config_str) {
                    eprintln!("failed to write {}: {e}", config_path.display());
                }
            }
            Err(e) => eprintln!("failed to serialize settings: {e}"),
        }
    }

    pub fn read() -> Self {
        let config_path = get_config_root().join(SETTINGS_FILENAME);
        if !config_path.exists() {
            return AppConfig::default();
        }
        // a hand-edited or truncated settings.json must not crash the
        // launcher/server/settings app at startup — fall back to defaults
        let config_str = match std::fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read {}: {e}", config_path.display());
                return AppConfig::default();
            }
        };
        serde_json::from_str(&config_str).unwrap_or_else(|e| {
            eprintln!("invalid settings.json, using defaults: {e}");
            AppConfig::default()
        })
    }

    pub fn new() -> Self {
        let config_path = get_config_root();
        if !config_path.exists() {
            if let Err(e) = std::fs::create_dir_all(&config_path) {
                eprintln!("failed to create {}: {e}", config_path.display());
                return AppConfig::default();
            }
        }
        let config = AppConfig::read();
        config.write();
        config
    }
}
