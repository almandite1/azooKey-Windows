use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// `%LOCALAPPDATA%\Azookey` — where per-user RUNTIME state belongs: logs,
/// crash dumps, the WebView2 profile. Deliberately not `get_config_root`:
/// that one is `%APPDATA%` (roaming), which is for settings a user would
/// want to follow them between machines, not for a browser profile.
///
/// `None` when `LOCALAPPDATA` is unset or empty, so the caller picks its own
/// fallback instead of silently getting a root-relative `Azookey` folder.
pub fn local_data_root() -> Option<PathBuf> {
    local_data_root_in(std::env::var_os("LOCALAPPDATA"))
}

/// Takes the environment value explicitly so tests never read or race on a
/// real environment variable (same reason as the `*_in`/`*_to` config
/// helpers below).
fn local_data_root_in(base: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = base?;
    if base.is_empty() {
        return None;
    }
    Some(Path::new(&base).join("Azookey"))
}

const SETTINGS_FILENAME: &str = "settings.json";
const SETTINGS_BACKUP_FILENAME: &str = "settings.json.bak";

/// Schema version of `settings.json`, independent of the application version
/// in `workspace.package` — bump it only when the settings schema itself
/// changes, and add the corresponding migration to `AppConfig::new`.
const CONFIG_VERSION: &str = "0.1.0";

/// Parse a `x.y.z` version into a tuple so versions compare semantically
/// rather than lexically ("0.10.0" > "0.9.0"). Anything that is not three
/// numeric components is unknown, and an unknown version is treated as
/// "possibly newer than us" — see `AppConfig::new`.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// True when the stored version is newer than the schema this build knows.
/// An unparsable version counts as newer: we cannot prove it is safe to
/// rewrite, and rewriting is the destructive direction.
fn is_newer_than_current(stored: &str) -> bool {
    let current = parse_version(CONFIG_VERSION);
    match (parse_version(stored), current) {
        (Some(stored), Some(current)) => stored > current,
        // stored unparsable (or CONFIG_VERSION malformed, which is a bug) —
        // stay on the non-destructive side
        _ => true,
    }
}

/// Outcome of reading `settings.json` off disk, kept separate from
/// `AppConfig` so `new` can tell "no file yet" from "file we could not
/// parse" — the two need very different handling.
enum LoadOutcome {
    Missing,
    Parsed(AppConfig),
    /// Unreadable or invalid JSON. The file must not be silently overwritten.
    Malformed,
}

/// Field names are mirrored by the Swift engine's `SettingsFile` decoder
/// (server-swift/Sources/azookey-server/azookey_server.swift) — keep them
/// in sync when adding settings the engine reads.
/// `#[serde(default)]` here and on `AppConfig` makes a settings file written
/// by an older build degrade per-field instead of failing the whole parse and
/// resetting everything — the same per-key tolerance the Swift `SettingsFile`
/// decoder already has.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct ZenzaiConfig {
    pub enable: bool,
    pub profile: String,
    pub backend: String,
}

impl Default for ZenzaiConfig {
    fn default() -> Self {
        ZenzaiConfig {
            enable: false,
            profile: "".to_string(),
            backend: "cpu".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub version: String,
    pub zenzai: ZenzaiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            version: CONFIG_VERSION.to_string(),
            zenzai: ZenzaiConfig::default(),
        }
    }
}

impl AppConfig {
    /// Persist to `settings.json`, refusing to overwrite a file written by a
    /// newer build of the settings schema — rewriting it would drop every key
    /// this build does not know about.
    pub fn write(&self) -> Result<(), String> {
        self.write_to(&get_config_root())
    }

    fn write_to(&self, config_root: &Path) -> Result<(), String> {
        let config_path = config_root.join(SETTINGS_FILENAME);

        if let LoadOutcome::Parsed(stored) = Self::load_from(config_root) {
            if is_newer_than_current(&stored.version) {
                return Err(format!(
                    "{} was written by a newer version ({}) than this build supports ({}); \
                     refusing to overwrite it",
                    config_path.display(),
                    stored.version,
                    CONFIG_VERSION
                ));
            }
        }

        // whatever version the caller happens to be holding, what lands on
        // disk is this build's schema — the settings app round-trips the
        // whole config through the frontend, version field included
        let stamped = AppConfig {
            version: CONFIG_VERSION.to_string(),
            ..self.clone()
        };
        let config_str = serde_json::to_string_pretty(&stamped)
            .map_err(|e| format!("failed to serialize settings: {e}"))?;
        std::fs::write(&config_path, config_str)
            .map_err(|e| format!("failed to write {}: {e}", config_path.display()))
    }

    /// Read the stored settings, falling back to defaults for anything that
    /// cannot be read. Never writes.
    pub fn read() -> Self {
        match Self::load_from(&get_config_root()) {
            LoadOutcome::Parsed(config) => config,
            LoadOutcome::Missing | LoadOutcome::Malformed => AppConfig::default(),
        }
    }

    fn load_from(config_root: &Path) -> LoadOutcome {
        let config_path = config_root.join(SETTINGS_FILENAME);
        if !config_path.exists() {
            return LoadOutcome::Missing;
        }
        // a hand-edited or truncated settings.json must not crash the
        // launcher/server/settings app at startup
        let config_str = match std::fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read {}: {e}", config_path.display());
                return LoadOutcome::Malformed;
            }
        };
        // Windows editors readily save UTF-8 with a BOM, and serde_json
        // rejects one — without this a hand-edit through Notepad looks like a
        // corrupt file and costs the user their settings
        let config_str = config_str.strip_prefix('\u{feff}').unwrap_or(&config_str);
        match serde_json::from_str(config_str) {
            Ok(config) => LoadOutcome::Parsed(config),
            Err(e) => {
                eprintln!("invalid {}: {e}", config_path.display());
                LoadOutcome::Malformed
            }
        }
    }

    /// Load the settings at process start, creating or migrating the file
    /// only when that is provably safe. A file from a *newer* build is used
    /// read-only: writing it back would serialize it through this build's
    /// schema and silently drop the keys we do not know about.
    pub fn new() -> Self {
        Self::new_in(&get_config_root())
    }

    fn new_in(config_root: &Path) -> Self {
        if !config_root.exists() {
            if let Err(e) = std::fs::create_dir_all(config_root) {
                eprintln!("failed to create {}: {e}", config_root.display());
                return AppConfig::default();
            }
        }

        match Self::load_from(config_root) {
            LoadOutcome::Missing => {
                let config = AppConfig::default();
                config.log_write_failure(config_root);
                config
            }
            LoadOutcome::Malformed => {
                // preserve whatever the user had before replacing it — the
                // file is the only copy of their settings
                let from = config_root.join(SETTINGS_FILENAME);
                let to = config_root.join(SETTINGS_BACKUP_FILENAME);
                if let Err(e) = std::fs::rename(&from, &to) {
                    // nothing was backed up, so do not overwrite either
                    eprintln!(
                        "failed to back up {} to {}: {e}; leaving it untouched and \
                         running with defaults",
                        from.display(),
                        to.display()
                    );
                    return AppConfig::default();
                }
                eprintln!("backed up unreadable settings to {}", to.display());
                let config = AppConfig::default();
                config.log_write_failure(config_root);
                config
            }
            LoadOutcome::Parsed(mut config) => {
                if is_newer_than_current(&config.version) {
                    eprintln!(
                        "settings.json version {} is newer than this build ({}); \
                         using it read-only",
                        config.version, CONFIG_VERSION
                    );
                } else if config.version != CONFIG_VERSION {
                    // older schema: serde already filled in the fields it did
                    // not have, so stamping the current version persists the
                    // migration
                    config.version = CONFIG_VERSION.to_string();
                    config.log_write_failure(config_root);
                }
                config
            }
        }
    }

    /// Write during startup: a failure to persist must not take down the IME,
    /// so it is logged rather than propagated.
    fn log_write_failure(&self, config_root: &Path) {
        if let Err(e) = self.write_to(config_root) {
            eprintln!("{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The `*_in`/`*_to` variants take the config root explicitly so these
    /// tests never touch `%APPDATA%` or race on an environment variable.
    struct TempConfigRoot(PathBuf);

    impl TempConfigRoot {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = format!(
                "azookey-config-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).expect("create temp config root");
            TempConfigRoot(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn settings(&self) -> PathBuf {
            self.0.join(SETTINGS_FILENAME)
        }

        fn write_settings(&self, contents: &str) {
            std::fs::write(self.settings(), contents).expect("write fixture");
        }

        fn read_settings(&self) -> String {
            std::fs::read_to_string(self.settings()).expect("read settings")
        }
    }

    impl Drop for TempConfigRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_semantic_versions() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert!(parse_version("0.10.0") > parse_version("0.9.0"));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn missing_file_is_created_with_defaults() {
        let root = TempConfigRoot::new();

        let config = AppConfig::new_in(root.path());

        assert_eq!(config.version, CONFIG_VERSION);
        assert!(!config.zenzai.enable);
        assert!(root.settings().exists());
    }

    #[test]
    fn older_version_is_migrated_and_stamped() {
        let root = TempConfigRoot::new();
        // backend is absent: an older schema that did not have the field yet
        root.write_settings(r#"{"version":"0.0.1","zenzai":{"enable":true,"profile":"p"}}"#);

        let config = AppConfig::new_in(root.path());

        assert!(config.zenzai.enable, "existing values must survive");
        assert_eq!(config.zenzai.profile, "p");
        assert_eq!(config.zenzai.backend, "cpu", "missing field gets a default");
        assert_eq!(config.version, CONFIG_VERSION);
        assert!(root.read_settings().contains(CONFIG_VERSION));
    }

    #[test]
    fn current_version_is_not_rewritten() {
        let root = TempConfigRoot::new();
        let original = serde_json::to_string_pretty(&AppConfig::default()).expect("serialize");
        root.write_settings(&original);

        AppConfig::new_in(root.path());

        assert_eq!(root.read_settings(), original, "file must be left alone");
    }

    #[test]
    fn newer_version_is_read_only() {
        let root = TempConfigRoot::new();
        let original = r#"{"version":"99.0.0","zenzai":{"enable":true,"profile":"p","backend":"cuda"},"future_field":123}"#;
        root.write_settings(original);

        let config = AppConfig::new_in(root.path());

        assert!(config.zenzai.enable, "known fields are still readable");
        assert_eq!(config.zenzai.backend, "cuda");
        assert_eq!(config.version, "99.0.0", "version must not be stamped down");
        assert_eq!(
            root.read_settings(),
            original,
            "a newer file must never be rewritten"
        );
    }

    #[test]
    fn writing_over_a_newer_version_is_refused() {
        let root = TempConfigRoot::new();
        let original = r#"{"version":"99.0.0","zenzai":{"enable":true,"profile":"p","backend":"cuda"},"future_field":123}"#;
        root.write_settings(original);

        let result = AppConfig::default().write_to(root.path());

        assert!(result.is_err(), "explicit save must be rejected too");
        assert_eq!(root.read_settings(), original);
    }

    #[test]
    fn unparsable_version_is_treated_as_newer() {
        let root = TempConfigRoot::new();
        let original = r#"{"version":"abc","zenzai":{"enable":true,"profile":"","backend":"cpu"}}"#;
        root.write_settings(original);

        AppConfig::new_in(root.path());

        assert_eq!(
            root.read_settings(),
            original,
            "an unknown version is not safe to rewrite"
        );
    }

    #[test]
    fn a_utf8_bom_is_tolerated() {
        let root = TempConfigRoot::new();
        root.write_settings(
            "\u{feff}{\"version\":\"0.1.0\",\"zenzai\":{\"enable\":true,\"profile\":\"p\",\"backend\":\"cpu\"}}",
        );

        let config = AppConfig::new_in(root.path());

        assert!(config.zenzai.enable, "a BOM is not corruption");
        assert!(
            !root.path().join(SETTINGS_BACKUP_FILENAME).exists(),
            "a BOM must not trigger the malformed path"
        );
    }

    #[test]
    fn writing_always_stamps_the_current_version() {
        let root = TempConfigRoot::new();
        // a caller holding a version it read elsewhere must not be able to
        // push that version onto a file this build wrote
        let config = AppConfig {
            version: "99.0.0".to_string(),
            ..AppConfig::default()
        };

        config.write_to(root.path()).expect("write");

        let written = AppConfig::new_in(root.path());
        assert_eq!(written.version, CONFIG_VERSION);
    }

    #[test]
    fn malformed_json_is_backed_up_before_reset() {
        let root = TempConfigRoot::new();
        let original = r#"{"version":"#;
        root.write_settings(original);

        let config = AppConfig::new_in(root.path());

        assert_eq!(config.version, CONFIG_VERSION);
        let backup = std::fs::read_to_string(root.path().join(SETTINGS_BACKUP_FILENAME))
            .expect("backup must exist");
        assert_eq!(backup, original, "the user's file is the only copy");
        assert!(root.read_settings().contains(CONFIG_VERSION));
    }

    #[test]
    fn local_data_root_is_the_azookey_folder_under_the_given_base() {
        let root = local_data_root_in(Some(r"C:\Users\someone\AppData\Local".into()))
            .expect("a set base yields a root");
        assert_eq!(root, Path::new(r"C:\Users\someone\AppData\Local\Azookey"));
    }

    /// Issue #54: the caller must be able to tell "no usable base" apart from
    /// a path, so it can choose a fallback it knows is writable — an empty
    /// base would otherwise yield a root-relative `Azookey` folder.
    #[test]
    fn local_data_root_is_none_without_a_usable_base() {
        assert_eq!(local_data_root_in(None), None);
        assert_eq!(local_data_root_in(Some("".into())), None);
    }
}
