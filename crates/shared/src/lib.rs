use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod job;
pub mod logs;
pub mod pipe;

/// The plugin API's version, in the one place both ends can see it.
///
/// It was a private constant in the server and another in the host, both
/// reading 1. Raising either alone compiled, passed every test, and left
/// the two speaking different languages — which the wire then handled by
/// having the host answer nothing, silently, forever. A shared definition
/// makes that particular mistake unavailable.
pub mod plugin_api {
    /// Bumped only for a change that is NOT additive. The proto evolves by
    /// adding fields; a reader that does not know a field ignores it, and
    /// that costs nobody a version. This is for the day something has to
    /// mean a different thing.
    pub const API_VERSION: u32 = 1;
}

/// The three proto packages are included flat into one module, so a
/// message name has to be unique across ALL of them — `plugin.proto` says
/// `PluginCandidate` rather than `Candidate` for that reason. It is also
/// why plugin.proto does not import service.proto: prost resolves a
/// cross-package reference to `super::azookey::Suggestion`, a path this
/// layout does not have.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/azookey.rs"));
    include!(concat!(env!("OUT_DIR"), "/window.rs"));
    include!(concat!(env!("OUT_DIR"), "/plugin.rs"));
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("azookey_service_descriptor");
}

/// `%APPDATA%\Azookey` — where per-user CONFIGURATION belongs, including the
/// settings file `AppConfig` owns.
///
/// `None` when `APPDATA` is unset or empty, so a caller picks its own
/// answer rather than silently reading a root-relative `Azookey` folder.
/// Same reasoning as [`local_data_root`], and the same shape.
///
/// There used to be a second, private helper for the `AppConfig` methods
/// that answered the same question with `unwrap_or_default()`, i.e. with a
/// RELATIVE path. So an environment without `APPDATA` gave three different
/// answers depending on which door you came in by: a relative `Azookey`
/// directory here, `None` in this function, and `nil` in the Swift reader.
/// One of those silently reads and writes settings next to the current
/// directory, which is nobody's profile.
pub fn config_root() -> Option<PathBuf> {
    config_root_in(std::env::var_os("APPDATA"))
}

/// Takes the environment value explicitly so tests never read or race on
/// a real environment variable (same reason as [`local_data_root_in`] and
/// the `*_in`/`*_to` config helpers).
pub fn config_root_in(base: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = base?;
    if base.is_empty() {
        return None;
    }
    Some(Path::new(&base).join("Azookey"))
}

/// `%LOCALAPPDATA%\Azookey` — where per-user RUNTIME state belongs: logs,
/// crash dumps, the WebView2 profile. Deliberately not [`config_root`]:
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

/// How many corrupt settings files are kept aside before one is dropped.
///
/// There used to be exactly one, under a fixed name, and that lost the only
/// copy of the user's real settings: corrupt once and the working file is
/// preserved as `.bak`; hand-edit the fresh defaults, corrupt them too, and
/// the second corrupt file overwrites the first backup. The numbered slots
/// below fix that, and when they are all full it is the NEWEST that goes —
/// see `back_up_unreadable_settings`.
const MAX_SETTINGS_BACKUPS: usize = 5;

/// How many times the model may re-run to improve one conversion.
///
/// Lives here because three places have to agree on it: this crate clamps
/// what it writes, the Swift engine clamps what it reads, and the settings
/// app offers presets inside the range. Only the engine used to bound it,
/// which meant any `u32` at all could be persisted and travel to the
/// converter — a limit of four billion is a hang, not a setting.
pub const ZENZAI_INFERENCE_LIMIT: std::ops::RangeInclusive<u32> = 1..=10;

/// How many tokens of KV cache the model gets, and with it the largest
/// composition the model can be asked about at all.
///
/// Bounded for the same reasons as the inference limit, and the far end is
/// harsher: the value is handed to llama.cpp as `n_ctx` and `n_batch`, so a
/// wild number is an allocation, potentially in VRAM. The floor is what the
/// converter hardcoded before it was configurable; anything longer than the
/// cache is not evaluated by the model at all and falls back to statistical
/// conversion, so a small value degrades quality rather than breaking.
///
/// This is a sanity bound, not the real ceiling. The engine clamps again to
/// what the loaded model was trained on — gpt2 position embeddings are a
/// table sized to the training context, and reading past it is not a
/// quality question — which for the zenz weights that ship here is 1024.
/// The range stays wider so a longer-context model would not need a change
/// on this side.
pub const ZENZAI_CONTEXT_SIZE: std::ops::RangeInclusive<u32> = 512..=4096;

/// The default context size, named because three places have to agree on it and
/// nothing would notice if they stopped: this crate's `Default`, the Swift
/// engine's own copy, and `fixtures/default-settings.json`. Not the range's
/// floor — unlike the inference limit, the useful default sits above it, at the
/// training context of the model that ships here.
pub const ZENZAI_CONTEXT_SIZE_DEFAULT: u32 = 1024;

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
    /// Boxed because it dwarfs the other two variants, which carry nothing:
    /// every `LoadOutcome` on the stack would otherwise be the size of a whole
    /// settings document, and the document grows every time a section is added.
    Parsed(Box<AppConfig>),
    /// Unreadable, or not JSON at all. The file must not be silently
    /// overwritten. Note how narrow this is now: a file that parses as JSON
    /// always comes back `Parsed`, however wrong its contents, because the
    /// decoding below falls back key by key.
    Malformed,
}

/// One leaf setting, taken from the JSON object it lives in, falling back to
/// its default when the key is absent OR holds the wrong type.
///
/// The whole reason this exists rather than `serde_json::from_str::<AppConfig>`:
/// serde fails the ENTIRE parse on one type mismatch. `"inference_limit": "5"`
/// — a plausible hand-edit, and settings.json is documented as hand-editable —
/// made the file unreadable, which made the next start move it aside and write
/// the defaults. One quoted number cost the user every setting they had. The
/// `#[serde(default)]` attributes never covered this; they only ever covered a
/// key being ABSENT, which is why the doc comment claiming per-field tolerance
/// was half true.
///
/// This is also the semantics the Swift reader has always had (its fields are
/// all `Optional` and applied one at a time), so the two ends of the same file
/// now agree.
fn lenient_field<T: serde::de::DeserializeOwned>(
    object: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
    section: &str,
    default: T,
) -> T {
    let Some(value) = object.and_then(|object| object.get(key)) else {
        return default;
    };
    match serde_json::from_value(value.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(
                "settings.json: {section}{key} is not a usable value ({error}); \
                 keeping the default for it and leaving every other setting alone"
            );
            default
        }
    }
}

/// The object at `key`, or `None` when the key is absent or is not an object.
/// A section that is not an object leaves every setting in it at its default,
/// rather than taking the rest of the file down with it.
fn lenient_section<'a>(
    object: Option<&'a serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    object.and_then(|object| object.get(key))?.as_object()
}

/// Field names are mirrored by the Swift engine's `SettingsFile` decoder
/// (server-swift/Sources/azookey-server/EngineConfig.swift) — keep them
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
    /// How many times the model may re-run to improve one conversion.
    /// The engine clamps this to 1..=10; the default stays at what the
    /// engine hardcoded before it was configurable, so upgrading does not
    /// silently make everyone's typing slower.
    pub inference_limit: u32,
    /// KV cache size in tokens. Read once, when the engine loads the model,
    /// so a change needs a restart to take effect — the same deal as
    /// `backend`, and for a related reason: swapping the model out at
    /// runtime tears down the llama backend underneath the replacement.
    pub context_size: u32,
    /// The three v3 context strings, the same shape as `profile`: short
    /// hints the model is given about what the user is writing about
    /// (`topic`), how (`style`) and what they tend to prefer
    /// (`preference`). Empty means "say nothing", as with `profile`.
    pub topic: String,
    pub style: String,
    pub preference: String,
}

impl Default for ZenzaiConfig {
    fn default() -> Self {
        ZenzaiConfig {
            enable: false,
            profile: "".to_string(),
            backend: "cpu".to_string(),
            inference_limit: *ZENZAI_INFERENCE_LIMIT.start(),
            context_size: ZENZAI_CONTEXT_SIZE_DEFAULT,
            topic: "".to_string(),
            style: "".to_string(),
            preference: "".to_string(),
        }
    }
}

impl ZenzaiConfig {
    /// Decoded one key at a time, so a single unusable value costs only
    /// itself — see [`lenient_field`].
    fn from_json(object: Option<&serde_json::Map<String, serde_json::Value>>) -> Self {
        let default = ZenzaiConfig::default();
        ZenzaiConfig {
            enable: lenient_field(object, "enable", "zenzai.", default.enable),
            profile: lenient_field(object, "profile", "zenzai.", default.profile),
            backend: lenient_field(object, "backend", "zenzai.", default.backend),
            inference_limit: lenient_field(
                object,
                "inference_limit",
                "zenzai.",
                default.inference_limit,
            ),
            context_size: lenient_field(object, "context_size", "zenzai.", default.context_size),
            topic: lenient_field(object, "topic", "zenzai.", default.topic),
            style: lenient_field(object, "style", "zenzai.", default.style),
            preference: lenient_field(object, "preference", "zenzai.", default.preference),
        }
    }
}

/// How the classic typo correction behaves. `automatic` leaves the choice to
/// the converter, which is what the engine did before this was settable.
///
/// A string rather than an enum for the same reason `zenzai.backend` is one:
/// settings.json is hand-editable, an unknown value must cost nothing, and the
/// side that consumes it decides what to do with one. The engine maps anything
/// it does not recognise back to `automatic`.
pub const TYPO_CORRECTION_MODES: [&str; 3] = ["automatic", "enabled", "disabled"];

/// What the converter is allowed to offer, beyond ordinary conversion.
///
/// Every default here reproduces the behaviour the engine had before these
/// became settings, so adding the section changes nothing until somebody turns
/// something on. Field names are mirrored by the Swift engine's `SettingsFile`
/// decoder (server-swift/Sources/azookey-server/EngineConfig.swift) — keep them
/// in sync when adding more.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct ConversionConfig {
    /// Half-width katakana (ｱｲｳ) mixed into the candidate list.
    pub half_width_kana: bool,
    /// Full-width alphanumerics (ＡＢＣ) mixed into the candidate list.
    pub full_width_roman: bool,
    /// While typing romaji for kana, also read the raw romaji as an English
    /// word. Catches the case of typing English without leaving Japanese mode.
    pub english_in_roman_input: bool,
    /// One of [`TYPO_CORRECTION_MODES`].
    pub typo_correction: String,
    /// Decorated letter candidates (𝐁𝐎𝐋𝐃, 𝒜𝓁𝓅𝒽𝒶). The converter only offers
    /// these when what is being composed is entirely roman letters or digits,
    /// so they cannot appear during ordinary Japanese conversion.
    pub typography: bool,
}

impl Default for ConversionConfig {
    fn default() -> Self {
        ConversionConfig {
            half_width_kana: false,
            full_width_roman: false,
            english_in_roman_input: false,
            typo_correction: TYPO_CORRECTION_MODES[0].to_string(),
            typography: false,
        }
    }
}

impl ConversionConfig {
    /// Decoded one key at a time, so a single unusable value costs only
    /// itself — see [`lenient_field`].
    fn from_json(object: Option<&serde_json::Map<String, serde_json::Value>>) -> Self {
        let default = ConversionConfig::default();
        ConversionConfig {
            half_width_kana: lenient_field(
                object,
                "half_width_kana",
                "conversion.",
                default.half_width_kana,
            ),
            full_width_roman: lenient_field(
                object,
                "full_width_roman",
                "conversion.",
                default.full_width_roman,
            ),
            english_in_roman_input: lenient_field(
                object,
                "english_in_roman_input",
                "conversion.",
                default.english_in_roman_input,
            ),
            typo_correction: lenient_field(
                object,
                "typo_correction",
                "conversion.",
                default.typo_correction,
            ),
            typography: lenient_field(object, "typography", "conversion.", default.typography),
        }
    }
}

/// One add-on the user has installed. Deliberately the smallest thing that
/// can identify a plugin and say whether it runs: anything a plugin
/// declares about itself (name, version, capabilities) belongs to its own
/// manifest, not to the user's settings file, so that installing a plugin
/// does not mean rewriting settings.json.
///
/// Per-field `#[serde(default)]` for the same reason as everything else
/// here: a hand-edited entry missing a key degrades to that key's default
/// rather than failing the whole file.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct PluginEntry {
    pub id: String,
    pub enabled: bool,
}

/// Add-on settings, off by default.
///
/// `enable` is read by the conversion server, at startup and on every
/// UpdateConfig, and decides whether the plugin host is asked anything at
/// all. `entries` is NOT read by anything in this build: the host runs a
/// fixed set of builtins, so setting an entry's `enabled` to false does
/// not stop it. It is here because the schema is the part that has to
/// land early, and per-plugin opt-in is what it will carry.
///
/// `enable` is the master switch, separate from the per-entry `enabled`:
/// turning the feature off must not require the user to disable every
/// plugin individually, and must not lose which ones they had on.
#[derive(Debug, Deserialize, Serialize, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct PluginsConfig {
    pub enable: bool,
    pub entries: Vec<PluginEntry>,
}

impl PluginsConfig {
    fn from_json(object: Option<&serde_json::Map<String, serde_json::Value>>) -> Self {
        let default = PluginsConfig::default();
        PluginsConfig {
            enable: lenient_field(object, "enable", "plugins.", default.enable),
            // element by element, not as one array: an entry somebody
            // mistyped should cost that entry, not the list. An entry with
            // no usable id is dropped rather than defaulted — a nameless
            // plugin identifies nothing.
            entries: match object.and_then(|object| object.get("entries")) {
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| match serde_json::from_value(item.clone()) {
                        Ok(entry) => Some(entry),
                        Err(error) => {
                            tracing::warn!(
                                "settings.json: dropping an unusable plugins.entries \
                                 element ({error}); the rest of the list is kept"
                            );
                            None
                        }
                    })
                    .collect(),
                Some(_) => {
                    tracing::warn!(
                        "settings.json: plugins.entries is not a list; treating it as empty"
                    );
                    Vec::new()
                }
                None => default.entries,
            },
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    pub version: String,
    pub zenzai: ZenzaiConfig,
    pub conversion: ConversionConfig,
    pub plugins: PluginsConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            version: CONFIG_VERSION.to_string(),
            zenzai: ZenzaiConfig::default(),
            conversion: ConversionConfig::default(),
            plugins: PluginsConfig::default(),
        }
    }
}

/// The config directory, or an explanation of why there is none. Every
/// no-argument `AppConfig` entry point goes through this rather than
/// inventing a relative fallback (see [`config_root`]).
fn config_root_or_error() -> Result<PathBuf, String> {
    config_root().ok_or_else(|| {
        "APPDATA is not set, so there is no per-user configuration directory to \
         read or write settings in"
            .to_string()
    })
}

impl AppConfig {
    /// This build's canonical form of these settings: the version stamped and
    /// every bounded value inside its range.
    ///
    /// One function so that what goes into the file and what goes to the
    /// engine cannot differ — they are the same document, produced here.
    fn normalized(&self) -> AppConfig {
        let mut config = AppConfig {
            // whatever version the caller happens to be holding, what lands
            // on disk is this build's schema — the settings app round-trips
            // the whole config through the frontend, version field included
            version: CONFIG_VERSION.to_string(),
            ..self.clone()
        };
        // Bounded here, not only by the reader. The engine clamps what it
        // decodes, but that is across an FFI boundary: until this existed, any
        // u32 the frontend or a hand-edit produced was persisted verbatim and
        // only the far side saved us.
        config.zenzai.inference_limit = config.zenzai.inference_limit.clamp(
            *ZENZAI_INFERENCE_LIMIT.start(),
            *ZENZAI_INFERENCE_LIMIT.end(),
        );
        config.zenzai.context_size = config
            .zenzai
            .context_size
            .clamp(*ZENZAI_CONTEXT_SIZE.start(), *ZENZAI_CONTEXT_SIZE.end());
        config
    }

    /// These settings as the JSON text the engine is handed.
    ///
    /// The engine used to open `settings.json` for itself, which meant one
    /// `UpdateConfig` read the file twice — once here, once over the FFI
    /// boundary — and a save landing between the two reads applied half of one
    /// version and half of the other. Now there is a single read, and the
    /// engine is given exactly the document this build would have written.
    pub fn to_engine_json(&self) -> Result<String, String> {
        serde_json::to_string(&self.normalized())
            .map_err(|e| format!("failed to serialize settings for the engine: {e}"))
    }

    /// Persist to `settings.json`, refusing to overwrite a file this build
    /// cannot account for — one written by a newer schema (whose unknown keys
    /// serializing through this build would drop), or one that is not JSON at
    /// all (which might be either).
    pub fn write(&self) -> Result<(), String> {
        self.write_to(&config_root_or_error()?)
    }

    fn write_to(&self, config_root: &Path) -> Result<(), String> {
        let config_path = config_root.join(SETTINGS_FILENAME);

        match Self::load_from(config_root) {
            LoadOutcome::Parsed(stored) if is_newer_than_current(&stored.version) => {
                return Err(format!(
                    "{} was written by a newer version ({}) than this build supports ({}); \
                     refusing to overwrite it",
                    config_path.display(),
                    stored.version,
                    CONFIG_VERSION
                ));
            }
            // The newer-version guard above can only fire on a file we could
            // read the version out of. A file that is not JSON has no version
            // to check, and a schema change is exactly the kind of thing that
            // would make a future file unreadable to this build — so the guard
            // used to be skipped precisely when it mattered most, and the file
            // was overwritten. Recovery from a broken file belongs to `new_in`,
            // which backs it up first; a plain save must not do it silently.
            LoadOutcome::Malformed => {
                return Err(format!(
                    "{} is not valid JSON; refusing to overwrite it. Restart the IME \
                     to have it moved aside and replaced with the defaults.",
                    config_path.display()
                ));
            }
            LoadOutcome::Parsed(_) | LoadOutcome::Missing => {}
        }

        let config_str = serde_json::to_string_pretty(&self.normalized())
            .map_err(|e| format!("failed to serialize settings: {e}"))?;
        // write-then-rename rather than a plain write: fs::write truncates
        // first, so a crash or power loss mid-write leaves a half-file that
        // the next start reads as Malformed and replaces with the defaults,
        // taking the .bak with it. Rename on Windows replaces the existing
        // file, and both paths are in the same directory so it stays atomic.
        //
        // The pid in the name is what makes that true across PROCESSES. One
        // shared `settings.json.tmp` was used by the launcher's startup
        // migration, the settings app and every save path; two of them writing
        // at once interleaved into one truncated file, and the rename then
        // published it — manufacturing exactly the corruption the temp file
        // exists to prevent.
        let tmp_path = config_root.join(format!("{SETTINGS_FILENAME}.tmp-{}", std::process::id()));
        std::fs::write(&tmp_path, config_str)
            .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &config_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("failed to replace {}: {e}", config_path.display())
        })
    }

    /// Read the stored settings, falling back to defaults for anything that
    /// cannot be read. Never writes.
    pub fn read() -> Self {
        match config_root_or_error() {
            Ok(root) => Self::read_in(&root),
            Err(reason) => {
                tracing::warn!("{reason}; running with the default settings");
                AppConfig::default()
            }
        }
    }

    /// The same, from a directory the caller names. Public for the same
    /// reason `new_in` and `write_to` exist privately: what happens on a
    /// re-read is worth testing, and the alternative is a test that reads
    /// the machine it runs on.
    pub fn read_in(config_root: &Path) -> Self {
        match Self::load_from(config_root) {
            LoadOutcome::Parsed(config) => *config,
            LoadOutcome::Missing | LoadOutcome::Malformed => AppConfig::default(),
        }
    }

    /// Read for a caller that is going to write the result back — the
    /// settings app does exactly that, one key at a time. A missing file is
    /// still the defaults (first run, and writing them is correct), but an
    /// unreadable or malformed one is an error here rather than the
    /// defaults: handing those back would let the caller save them over
    /// whatever the user actually had.
    pub fn try_read() -> Result<Self, String> {
        Self::try_read_in(&config_root_or_error()?)
    }

    /// The same, from a directory the caller names — see `read_in`.
    pub fn try_read_in(config_root: &Path) -> Result<Self, String> {
        match Self::load_from(config_root) {
            LoadOutcome::Parsed(config) => Ok(*config),
            LoadOutcome::Missing => Ok(AppConfig::default()),
            LoadOutcome::Malformed => Err(format!(
                "{} could not be read; it is missing or not valid JSON",
                config_root.join(SETTINGS_FILENAME).display()
            )),
        }
    }

    /// Two-stage decode: is this JSON at all, and then what can be salvaged
    /// from it.
    ///
    /// Only the first stage can fail. Once the text parses as JSON, every
    /// setting is taken out of it individually and a value this build cannot
    /// use costs nothing but itself — which is the difference between "you
    /// quoted a number" and "your settings are gone".
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
                tracing::warn!("failed to read {}: {e}", config_path.display());
                return LoadOutcome::Malformed;
            }
        };
        // Windows editors readily save UTF-8 with a BOM, and serde_json
        // rejects one — without this a hand-edit through Notepad looks like a
        // corrupt file and costs the user their settings
        let config_str = config_str.strip_prefix('\u{feff}').unwrap_or(&config_str);
        let value: serde_json::Value = match serde_json::from_str(config_str) {
            Ok(value) => value,
            Err(e) => {
                tracing::warn!("invalid {}: {e}", config_path.display());
                return LoadOutcome::Malformed;
            }
        };
        LoadOutcome::Parsed(Box::new(Self::from_json(value.as_object())))
    }

    /// Everything this build understands, taken out of a parsed settings
    /// document key by key. A document that is not an object at all (a bare
    /// array, a number) leaves every setting at its default.
    fn from_json(object: Option<&serde_json::Map<String, serde_json::Value>>) -> Self {
        AppConfig {
            // read straight off the document rather than through the struct:
            // the version is what decides whether this build may write the
            // file back, so it has to survive anything else being unusable
            version: lenient_field(object, "version", "", CONFIG_VERSION.to_string()),
            zenzai: ZenzaiConfig::from_json(lenient_section(object, "zenzai")),
            conversion: ConversionConfig::from_json(lenient_section(object, "conversion")),
            plugins: PluginsConfig::from_json(lenient_section(object, "plugins")),
        }
    }

    /// Load the settings at process start, creating or migrating the file
    /// only when that is provably safe. A file from a *newer* build is used
    /// read-only: writing it back would serialize it through this build's
    /// schema and silently drop the keys we do not know about.
    pub fn new() -> Self {
        match config_root_or_error() {
            Ok(root) => Self::new_in(&root),
            Err(reason) => {
                tracing::warn!("{reason}; running with the default settings");
                AppConfig::default()
            }
        }
    }

    fn new_in(config_root: &Path) -> Self {
        if !config_root.exists()
            && let Err(e) = std::fs::create_dir_all(config_root)
        {
            tracing::warn!("failed to create {}: {e}", config_root.display());
            return AppConfig::default();
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
                if !back_up_unreadable_settings(config_root) {
                    // nothing was backed up, so do not overwrite either
                    return AppConfig::default();
                }
                let config = AppConfig::default();
                config.log_write_failure(config_root);
                config
            }
            LoadOutcome::Parsed(mut config) => {
                if is_newer_than_current(&config.version) {
                    tracing::warn!(
                        "settings.json version {} is newer than this build ({}); \
                         using it read-only",
                        config.version,
                        CONFIG_VERSION
                    );
                } else if config.version != CONFIG_VERSION {
                    // older schema: the decode already filled in the fields it
                    // did not have, so stamping the current version persists
                    // the migration
                    config.version = CONFIG_VERSION.to_string();
                    config.log_write_failure(config_root);
                }
                *config
            }
        }
    }

    /// Write during startup: a failure to persist must not take down the IME,
    /// so it is logged rather than propagated.
    fn log_write_failure(&self, config_root: &Path) {
        if let Err(e) = self.write_to(config_root) {
            tracing::warn!("{e}");
        }
    }
}

/// Moves an unreadable `settings.json` aside. True when it is safe to write a
/// fresh one, i.e. when the old bytes are somewhere.
///
/// The first backup keeps the plain `.bak` name and is never overwritten;
/// later ones take `-2`, `-3`, … up to [`MAX_SETTINGS_BACKUPS`]. When the
/// slots are full it is the LAST one that is reused, which is the opposite of
/// what "rotate" usually means and is the entire point: the oldest backup was
/// taken from a file that had been working, and each one after it is a copy of
/// something already broken. Dropping the oldest would be a slower version of
/// the bug this replaced — one fixed name, overwritten by the second
/// corruption, taking the user's real settings with it.
fn back_up_unreadable_settings(config_root: &Path) -> bool {
    let from = config_root.join(SETTINGS_FILENAME);

    let mut to = config_root.join(SETTINGS_BACKUP_FILENAME);
    for slot in 2..=MAX_SETTINGS_BACKUPS {
        if !to.exists() {
            break;
        }
        to = config_root.join(format!("{SETTINGS_BACKUP_FILENAME}-{slot}"));
    }
    if to.exists() {
        tracing::warn!(
            "every settings backup slot is taken; replacing the newest ({}) and \
             keeping the older ones",
            to.display()
        );
    }

    if let Err(e) = std::fs::rename(&from, &to) {
        tracing::warn!(
            "failed to back up {} to {}: {e}; leaving it untouched and \
             running with defaults",
            from.display(),
            to.display()
        );
        return false;
    }
    tracing::info!("backed up unreadable settings to {}", to.display());
    true
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

    /// The host and the client both link against this now, so a build.rs
    /// that stopped compiling plugin.proto would fail loudly on its own.
    /// The test stays because it fails FIRST and in one line, rather than
    /// as a hundred missing-type errors in two other crates.
    #[test]
    fn the_plugin_api_surface_is_generated() {
        use crate::proto::plugin_host_service_client::PluginHostServiceClient;
        use crate::proto::plugin_host_service_server::PluginHostServiceServer;

        let request = crate::proto::ProcessCandidatesRequest {
            api_version: 1,
            reading: "きょう".to_string(),
            candidates: vec![crate::proto::PluginCandidate {
                text: "今日".to_string(),
                subtext: String::new(),
                corresponding_count: 4,
                surface_count: 3,
            }],
        };

        assert_eq!(request.candidates[0].surface_count, 3);
        // the client and server halves both exist; `_` because naming the
        // types is the whole assertion
        let _: Option<PluginHostServiceClient<tonic::transport::Channel>> = None;
        let _ = std::any::type_name::<PluginHostServiceServer<()>>();
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

    /// The zenzai keys added after the first release have to degrade the same
    /// way `backend` did, and to the values the engine used while they were
    /// still hardcoded — an upgrade must not change how conversion behaves
    /// until the user asks it to.
    #[test]
    fn zenzai_keys_added_later_default_to_the_previous_behaviour() {
        let root = TempConfigRoot::new();
        root.write_settings(
            r#"{"version":"0.1.0","zenzai":{"enable":true,"profile":"p","backend":"cuda"}}"#,
        );

        let config = AppConfig::new_in(root.path());

        assert_eq!(
            config.zenzai.inference_limit, 1,
            "what the engine hardcoded"
        );
        assert_eq!(config.zenzai.topic, "");
        assert_eq!(config.zenzai.style, "");
        assert_eq!(config.zenzai.preference, "");
        assert!(config.zenzai.enable, "the old keys still survive");
        assert_eq!(config.zenzai.profile, "p");
    }

    /// The engine decodes this file itself, by field name, so the names have
    /// to survive a round trip through disk exactly as spelled.
    #[test]
    fn the_zenzai_context_keys_round_trip() {
        let root = TempConfigRoot::new();
        let mut config = AppConfig::default();
        config.zenzai.inference_limit = 5;
        config.zenzai.topic = "ソフトウェア開発".to_string();
        config.zenzai.style = "ですます調".to_string();
        config.zenzai.preference = "漢字は控えめに".to_string();

        config.write_to(root.path()).expect("write");

        let stored = root.read_settings();
        for key in [
            "inference_limit",
            "context_size",
            "topic",
            "style",
            "preference",
        ] {
            assert!(
                stored.contains(&format!("\"{key}\"")),
                "the Swift decoder looks for {key} by name"
            );
        }
        let reread = AppConfig::new_in(root.path());
        assert_eq!(reread.zenzai.inference_limit, 5);
        assert_eq!(reread.zenzai.topic, "ソフトウェア開発");
        assert_eq!(reread.zenzai.style, "ですます調");
        assert_eq!(reread.zenzai.preference, "漢字は控えめに");
    }

    /// The settings app re-reads before every save, so what a failed read
    /// returns decides whether a broken file gets overwritten with defaults
    /// or reported.
    #[test]
    fn try_read_reports_a_broken_file_but_not_a_missing_one() {
        let root = TempConfigRoot::new();

        let missing = AppConfig::try_read_in(root.path()).expect("no file yet is not an error");
        assert_eq!(missing.version, CONFIG_VERSION);

        root.write_settings(r#"{"version":"#);
        assert!(
            AppConfig::try_read_in(root.path()).is_err(),
            "defaults here would be saved over the user's file"
        );

        root.write_settings(r#"{"version":"0.1.0","zenzai":{"profile":"p"}}"#);
        assert_eq!(
            AppConfig::try_read_in(root.path())
                .expect("a valid file reads")
                .zenzai
                .profile,
            "p"
        );
    }

    /// The bug this whole two-stage decode exists for: one quoted number used
    /// to fail the entire parse, which moved the file aside and wrote the
    /// defaults. settings.json is documented as hand-editable, so "you typed
    /// `"5"` instead of `5`" must cost that one setting and nothing else.
    #[test]
    fn a_wrongly_typed_value_costs_only_its_own_field() {
        let root = TempConfigRoot::new();
        root.write_settings(
            r#"{
                 "version": "0.1.0",
                 "zenzai": {
                   "enable": "true",
                   "profile": "keep me",
                   "backend": "cuda",
                   "inference_limit": "5",
                   "topic": 42
                 },
                 "conversion": {
                   "half_width_kana": "yes",
                   "full_width_roman": true,
                   "typo_correction": "disabled"
                 },
                 "plugins": { "enable": true, "entries": [] }
               }"#,
        );

        let config = AppConfig::new_in(root.path());

        // the unusable values fell back, one by one
        assert!(!config.zenzai.enable, "a quoted bool is not a bool");
        assert_eq!(config.zenzai.inference_limit, 1, "a quoted number either");
        assert_eq!(config.zenzai.topic, "", "nor is a number a string");
        assert!(
            !config.conversion.half_width_kana,
            "and the same rule holds one section over"
        );
        // ...and everything around them survived, which is the point
        assert_eq!(config.zenzai.profile, "keep me");
        assert_eq!(config.zenzai.backend, "cuda");
        assert!(config.conversion.full_width_roman);
        assert_eq!(config.conversion.typo_correction, "disabled");
        assert!(
            !config.conversion.typography,
            "a key the file never mentions keeps its default"
        );
        assert!(config.plugins.enable);
        assert!(
            !root.path().join(SETTINGS_BACKUP_FILENAME).exists(),
            "a usable file must not be treated as corrupt"
        );
    }

    /// A section that is not an object, and a document that is not an object,
    /// are the same class of mistake: they cost the settings inside them and
    /// nothing else.
    #[test]
    fn an_unusable_section_does_not_take_the_file_with_it() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.1.0","zenzai":"on","plugins":{"enable":true}}"#);

        let config = AppConfig::new_in(root.path());

        assert!(!config.zenzai.enable, "the whole section fell back");
        assert_eq!(config.zenzai.backend, "cpu");
        assert!(config.plugins.enable, "the other section was untouched");
        assert_eq!(config.version, CONFIG_VERSION);
    }

    /// Only text that is not JSON at all is Malformed now. The distinction
    /// matters because Malformed is the path that moves the user's file aside.
    #[test]
    fn only_unparsable_text_counts_as_malformed() {
        let root = TempConfigRoot::new();

        root.write_settings("{not json at all");
        assert!(matches!(
            AppConfig::load_from(root.path()),
            LoadOutcome::Malformed
        ));

        // valid JSON, wrong shape from top to bottom: still readable
        root.write_settings("[1, 2, 3]");
        assert!(matches!(
            AppConfig::load_from(root.path()),
            LoadOutcome::Parsed(_)
        ));
    }

    /// A plugins list with one bad element keeps the good ones. Nothing reads
    /// `entries` in this build, which is exactly why it is worth pinning: the
    /// day something does, a hand-edit should not silently empty the list.
    #[test]
    fn one_unusable_plugin_entry_does_not_empty_the_list() {
        let root = TempConfigRoot::new();
        root.write_settings(
            r#"{"plugins":{"entries":[{"id":"dates","enabled":true},{"id":5},{"id":"emoji","enabled":false}]}}"#,
        );

        let config = AppConfig::new_in(root.path());

        let ids: Vec<&str> = config
            .plugins
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["dates", "emoji"]);
    }

    /// The newer-version guard could only ever fire on a file whose version
    /// this build could read. A schema change is the thing most likely to make
    /// a future file unreadable here — so the guard was skipped in precisely
    /// the case it exists for, and the file was overwritten.
    #[test]
    fn writing_over_an_unparsable_file_is_refused() {
        let root = TempConfigRoot::new();
        let original = "{this is not json";
        root.write_settings(original);

        let result = AppConfig::default().write_to(root.path());

        assert!(
            result.is_err(),
            "an unreadable file must not be overwritten"
        );
        assert_eq!(
            root.read_settings(),
            original,
            "and its bytes must still be there"
        );
    }

    /// Corrupt twice and the FIRST backup — the one taken from a file that had
    /// been working — has to survive. It used to be overwritten by the second
    /// corrupt file, which is how the only copy of the real settings was lost.
    #[test]
    fn a_second_corruption_keeps_the_first_backup() {
        let root = TempConfigRoot::new();
        root.write_settings("{the real settings, corrupted");
        AppConfig::new_in(root.path());
        assert_eq!(
            std::fs::read_to_string(root.path().join(SETTINGS_BACKUP_FILENAME))
                .expect("the first backup exists"),
            "{the real settings, corrupted"
        );

        root.write_settings("{corrupted again, and worthless");
        AppConfig::new_in(root.path());

        assert_eq!(
            std::fs::read_to_string(root.path().join(SETTINGS_BACKUP_FILENAME))
                .expect("the first backup is still there"),
            "{the real settings, corrupted",
            "the oldest backup is the valuable one and must not be replaced"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(format!("{SETTINGS_BACKUP_FILENAME}-2")))
                .expect("the second backup went to its own slot"),
            "{corrupted again, and worthless"
        );
    }

    /// The slots are bounded, and when they are full the oldest still wins.
    #[test]
    fn the_backup_slots_are_capped_and_the_oldest_survives() {
        let root = TempConfigRoot::new();

        for i in 0..MAX_SETTINGS_BACKUPS + 3 {
            root.write_settings(&format!("{{corruption number {i}"));
            AppConfig::new_in(root.path());
        }

        let backups = std::fs::read_dir(root.path())
            .expect("read the config root")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(SETTINGS_BACKUP_FILENAME)
            })
            .count();
        assert_eq!(backups, MAX_SETTINGS_BACKUPS, "the slots are bounded");
        assert_eq!(
            std::fs::read_to_string(root.path().join(SETTINGS_BACKUP_FILENAME))
                .expect("the first backup survived every later corruption"),
            "{corruption number 0"
        );
    }

    /// Two processes saving at once used to interleave into one shared
    /// `settings.json.tmp`, and the rename then published the mixture — the
    /// temp file manufactured the corruption it exists to prevent.
    #[test]
    fn the_staging_file_is_private_to_this_process() {
        let root = TempConfigRoot::new();

        AppConfig::default().write_to(root.path()).expect("write");

        assert!(
            !root
                .path()
                .join(format!("{SETTINGS_FILENAME}.tmp"))
                .exists(),
            "the shared name must not be used at all"
        );
        let staged = std::fs::read_dir(root.path())
            .expect("read the config root")
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{SETTINGS_FILENAME}.tmp"))
            });
        assert!(!staged, "and the rename must leave nothing behind");
    }

    /// Only the reader bounded this, and the reader is on the other side of an
    /// FFI boundary: until it was clamped here, any u32 at all was persisted.
    #[test]
    fn an_out_of_range_inference_limit_is_clamped_before_it_is_stored() {
        let root = TempConfigRoot::new();
        let mut config = AppConfig::default();

        config.zenzai.inference_limit = u32::MAX;
        config.write_to(root.path()).expect("write");
        assert_eq!(
            AppConfig::read_in(root.path()).zenzai.inference_limit,
            *ZENZAI_INFERENCE_LIMIT.end()
        );

        config.zenzai.inference_limit = 0;
        config.write_to(root.path()).expect("write");
        assert_eq!(
            AppConfig::read_in(root.path()).zenzai.inference_limit,
            *ZENZAI_INFERENCE_LIMIT.start()
        );
    }

    /// Adding a settings section must not change how anything converts. Every
    /// default in `conversion` is what the engine did before these were
    /// settable, so an existing settings.json that has never heard of the
    /// section behaves exactly as it did.
    #[test]
    fn the_conversion_defaults_are_the_engines_previous_behaviour() {
        let root = TempConfigRoot::new();
        root.write_settings(
            r#"{"version":"0.1.0","zenzai":{"enable":true},"plugins":{"enable":true}}"#,
        );

        let config = AppConfig::new_in(root.path());

        assert!(!config.conversion.half_width_kana);
        assert!(!config.conversion.full_width_roman);
        assert!(!config.conversion.english_in_roman_input);
        assert!(!config.conversion.typography);
        assert_eq!(
            config.conversion.typo_correction, "automatic",
            "the converter's own default, so the engine keeps deciding"
        );
        assert!(config.zenzai.enable, "the sections it does have still load");
    }

    /// Same reasoning as the inference limit, with a sharper edge: this number
    /// becomes llama.cpp's `n_ctx` and `n_batch`, so an unbounded one is an
    /// allocation that can land in VRAM.
    #[test]
    fn an_out_of_range_context_size_is_clamped_before_it_is_stored() {
        let root = TempConfigRoot::new();
        let mut config = AppConfig::default();

        config.zenzai.context_size = u32::MAX;
        config.write_to(root.path()).expect("write");
        assert_eq!(
            AppConfig::read_in(root.path()).zenzai.context_size,
            *ZENZAI_CONTEXT_SIZE.end()
        );

        config.zenzai.context_size = 1;
        config.write_to(root.path()).expect("write");
        assert_eq!(
            AppConfig::read_in(root.path()).zenzai.context_size,
            *ZENZAI_CONTEXT_SIZE.start()
        );
    }

    /// The defaults are Rust's to define, and the Swift engine has its own
    /// copy of every one of them. Nothing compared the two, so "the default
    /// inference limit is 1" could stop being true on one side only — and the
    /// symptom would be conversion behaving differently from what the settings
    /// app shows, with no test failing anywhere.
    ///
    /// Both suites read this file: here it is compared against what this build
    /// serializes, and in `config_tests.swift` it is applied to a fresh
    /// `EngineConfig` and expected to change nothing.
    #[test]
    fn the_defaults_match_the_shared_fixture() {
        let fixture = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/default-settings.json"),
        )
        .expect("read fixtures/default-settings.json");

        let expected: serde_json::Value =
            serde_json::from_str(&fixture).expect("the fixture is JSON");
        let actual: serde_json::Value =
            serde_json::to_value(AppConfig::default()).expect("serialize the defaults");

        assert_eq!(
            actual, expected,
            "the defaults changed; update fixtures/default-settings.json and check that the \
             Swift engine's EngineConfig still agrees with it"
        );
    }

    /// The public twin of `local_data_root_in`, which had tests while this one
    /// had none — and this is the one the settings file hangs off.
    #[test]
    fn config_root_is_the_azookey_folder_under_the_given_base() {
        assert_eq!(
            config_root_in(Some(r"C:\Users\someone\AppData\Roaming".into()))
                .expect("a set base yields a root"),
            Path::new(r"C:\Users\someone\AppData\Roaming\Azookey")
        );
        assert_eq!(
            config_root_in(None),
            None,
            "no base must be reported as none, not as a relative path"
        );
        assert_eq!(
            config_root_in(Some("".into())),
            None,
            "an empty base is the same as no base"
        );
    }

    #[test]
    fn current_version_is_not_rewritten() {
        let root = TempConfigRoot::new();
        let original = serde_json::to_string_pretty(&AppConfig::default()).expect("serialize");
        root.write_settings(&original);

        AppConfig::new_in(root.path());

        assert_eq!(root.read_settings(), original, "file must be left alone");
    }

    /// The unknown keys here are the whole point: serde drops what it does
    /// not know on the way back out, so the ONLY thing protecting a future
    /// build's settings — a key inside `plugins` as much as a top-level one
    /// — is that this build refuses to write the file at all.
    #[test]
    fn newer_version_is_read_only() {
        let root = TempConfigRoot::new();
        let original = r#"{"version":"99.0.0","zenzai":{"enable":true,"profile":"p","backend":"cuda"},"plugins":{"enable":true,"entries":[{"id":"future","enabled":true,"future_key":"x"}]},"future_field":123}"#;
        root.write_settings(original);

        let config = AppConfig::new_in(root.path());

        assert!(config.zenzai.enable, "known fields are still readable");
        assert_eq!(config.zenzai.backend, "cuda");
        assert!(config.plugins.enable);
        assert_eq!(config.plugins.entries.len(), 1);
        assert_eq!(config.plugins.entries[0].id, "future");
        assert_eq!(config.version, "99.0.0", "version must not be stamped down");
        assert_eq!(
            root.read_settings(),
            original,
            "a newer file must never be rewritten"
        );
    }

    /// The write goes through a temp file so a crash cannot leave a
    /// truncated settings.json behind. The temp must not survive the write —
    /// a leftover would be mistaken for a stale profile by anyone inspecting
    /// the config directory.
    #[test]
    fn write_leaves_no_temp_file_and_replaces_the_old_content() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.0.1","zenzai":{"enable":false,"profile":"old"}}"#);

        let mut config = AppConfig::default();
        config.zenzai.profile = "new".to_string();
        config.write_to(root.path()).expect("write");

        let stored = root.read_settings();
        let parsed: AppConfig = serde_json::from_str(&stored).expect("the final file must parse");
        assert_eq!(parsed.zenzai.profile, "new");
        assert_eq!(parsed.version, CONFIG_VERSION);
        assert!(
            !root
                .path()
                .join(format!("{SETTINGS_FILENAME}.tmp"))
                .exists(),
            "the temp file must be renamed away, not left behind"
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
            "\u{feff}{\"version\":\"0.1.0\",\"zenzai\":{\"enable\":true,\"profile\":\"p\",\"backend\":\"cpu\"},\"plugins\":{\"enable\":true,\"entries\":[{\"id\":\"dates\",\"enabled\":true}]}}",
        );

        let config = AppConfig::new_in(root.path());

        assert!(config.zenzai.enable, "a BOM is not corruption");
        assert!(config.plugins.enable, "the BOM strip covers the whole file");
        assert_eq!(config.plugins.entries[0].id, "dates");
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

    /// `#[serde(default)]` is per FIELD, not per struct: a settings.json that
    /// predates one zenzai key (or was hand-edited down to the keys the user
    /// cared about) must keep every key it does have. `backend` is covered by
    /// `older_version_is_migrated_and_stamped`; these are the other two.
    #[test]
    fn a_missing_enable_defaults_without_losing_the_other_keys() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.0.1","zenzai":{"profile":"p","backend":"cuda"}}"#);

        let config = AppConfig::new_in(root.path());

        assert!(!config.zenzai.enable, "missing field gets a default");
        assert_eq!(config.zenzai.profile, "p");
        assert_eq!(config.zenzai.backend, "cuda");
    }

    #[test]
    fn a_missing_profile_defaults_without_losing_the_other_keys() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.0.1","zenzai":{"enable":true,"backend":"cuda"}}"#);

        let config = AppConfig::new_in(root.path());

        assert_eq!(config.zenzai.profile, "", "missing field gets a default");
        assert!(config.zenzai.enable);
        assert_eq!(config.zenzai.backend, "cuda");
    }

    /// What the server does on every UpdateConfig: read the file again and
    /// take the new answer. Both directions, because a switch that only
    /// worked one way would look like it worked.
    ///
    /// `read` rather than `new_in`: the re-read must not create, migrate
    /// or stamp anything, and a test that used the startup path would not
    /// notice if it started doing so.
    #[test]
    fn a_re_read_sees_the_setting_change_in_both_directions() {
        let root = TempConfigRoot::new();

        root.write_settings(r#"{"version":"0.1.0","plugins":{"enable":true}}"#);
        assert!(AppConfig::read_in(root.path()).plugins.enable);

        root.write_settings(r#"{"version":"0.1.0","plugins":{"enable":false}}"#);
        assert!(!AppConfig::read_in(root.path()).plugins.enable);

        root.write_settings(r#"{"version":"0.1.0","plugins":{"enable":true}}"#);
        assert!(AppConfig::read_in(root.path()).plugins.enable);
    }

    /// A re-read of a broken file must hand back the defaults rather than
    /// whatever was there before — and must not repair the file, which is
    /// the startup path's job and only on startup.
    #[test]
    fn a_re_read_of_a_broken_file_is_the_defaults_and_changes_nothing() {
        let root = TempConfigRoot::new();
        let broken = r#"{"version":"#;
        root.write_settings(broken);

        let config = AppConfig::read_in(root.path());

        assert!(!config.plugins.enable);
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(root.read_settings(), broken, "a re-read must not write");
        assert!(
            !root.path().join(SETTINGS_BACKUP_FILENAME).exists(),
            "and must not back anything up either"
        );
    }

    /// The degenerate end of the same rule: an empty (or absent) zenzai
    /// object is a valid file, not a malformed one — no backup, no reset of
    /// anything else.
    #[test]
    fn an_empty_zenzai_object_yields_the_defaults() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.0.1","zenzai":{}}"#);

        let config = AppConfig::new_in(root.path());

        let defaults = ZenzaiConfig::default();
        assert_eq!(config.zenzai.enable, defaults.enable);
        assert_eq!(config.zenzai.profile, defaults.profile);
        assert_eq!(config.zenzai.backend, defaults.backend);
        assert!(
            !root.path().join(SETTINGS_BACKUP_FILENAME).exists(),
            "a partial file is not corruption"
        );
    }

    /// Every settings.json that exists today predates the plugins section,
    /// so "the section is absent" is the normal case, not an edge one: it
    /// must read as the feature being off rather than as a broken file.
    #[test]
    fn an_absent_plugins_section_reads_as_off() {
        let root = TempConfigRoot::new();
        root.write_settings(
            r#"{"version":"0.0.1","zenzai":{"enable":true,"profile":"p","backend":"cuda"}}"#,
        );

        let config = AppConfig::new_in(root.path());

        assert!(!config.plugins.enable, "the opt-in defaults to off");
        assert!(config.plugins.entries.is_empty());
        assert!(config.zenzai.enable, "the existing section is untouched");
        assert_eq!(config.zenzai.backend, "cuda");
        assert!(
            !root.path().join(SETTINGS_BACKUP_FILENAME).exists(),
            "a file from before the section is not corruption"
        );
    }

    /// The migration write must not be able to lose what it just read: a
    /// file stamped up to the current version is rewritten in full, so the
    /// plugins it carried have to come back out of the serializer.
    #[test]
    fn plugins_survive_the_migration_rewrite() {
        let root = TempConfigRoot::new();
        root.write_settings(
            r#"{"version":"0.0.1","zenzai":{},"plugins":{"enable":true,"entries":[{"id":"a","enabled":true},{"id":"b","enabled":false}]}}"#,
        );

        let config = AppConfig::new_in(root.path());
        assert_eq!(config.version, CONFIG_VERSION, "this file is rewritten");

        let reloaded = AppConfig::new_in(root.path());
        assert!(reloaded.plugins.enable);
        assert_eq!(
            reloaded.plugins.entries,
            vec![
                PluginEntry {
                    id: "a".to_string(),
                    enabled: true
                },
                PluginEntry {
                    id: "b".to_string(),
                    enabled: false
                },
            ],
            "order and per-entry state both survive the round trip"
        );
    }

    #[test]
    fn plugin_entries_survive_an_explicit_write() {
        let root = TempConfigRoot::new();
        let config = AppConfig {
            plugins: PluginsConfig {
                enable: true,
                entries: vec![PluginEntry {
                    id: "dates".to_string(),
                    enabled: true,
                }],
            },
            ..AppConfig::default()
        };

        config.write_to(root.path()).expect("write");

        let reloaded = AppConfig::new_in(root.path());
        assert_eq!(reloaded.plugins, config.plugins);
    }

    /// Same per-field rule as zenzai, one level deeper: a hand-written
    /// entry that only names an id is a valid entry, and the master switch
    /// it sits under is independent of it.
    #[test]
    fn a_partial_plugins_object_keeps_the_keys_it_has() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.0.1","plugins":{"entries":[{"id":"dates"}]}}"#);

        let config = AppConfig::new_in(root.path());

        assert!(!config.plugins.enable, "missing field gets a default");
        assert_eq!(config.plugins.entries.len(), 1);
        assert_eq!(config.plugins.entries[0].id, "dates");
        assert!(
            !config.plugins.entries[0].enabled,
            "an entry defaults to not running"
        );
    }

    /// The degenerate end, mirroring `an_empty_zenzai_object_yields_the_defaults`.
    #[test]
    fn an_empty_plugins_object_yields_the_defaults() {
        let root = TempConfigRoot::new();
        root.write_settings(r#"{"version":"0.0.1","plugins":{}}"#);

        let config = AppConfig::new_in(root.path());

        assert_eq!(config.plugins, PluginsConfig::default());
        assert!(
            !root.path().join(SETTINGS_BACKUP_FILENAME).exists(),
            "a partial file is not corruption"
        );
    }
}
