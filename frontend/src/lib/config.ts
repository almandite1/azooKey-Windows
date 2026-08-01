import { invoke } from "@tauri-apps/api/core";

/// The settings document, mirroring `shared::AppConfig` on the Rust side.
/// Hand-written, and held to the Rust struct by a key-name test — this was
/// `any` everywhere, so a typo in a key path was a runtime no-op that looked
/// exactly like a setting that does not work.

export interface ZenzaiConfig {
    enable: boolean;
    profile: string;
    backend: string;
    inference_limit: number;
    context_size: number;
    topic: string;
    style: string;
    preference: string;
}

export interface ConversionConfig {
    half_width_kana: boolean;
    full_width_roman: boolean;
    /// "automatic" | "enabled" | "disabled"; anything else means automatic
    typo_correction: string;
    typography: boolean;
}

export interface PluginsConfig {
    enable: boolean;
    entries: { id: string; enabled: boolean }[];
}

export interface AppConfig {
    version: string;
    zenzai: ZenzaiConfig;
    conversion: ConversionConfig;
    plugins: PluginsConfig;
}

/// Every settable key, spelled the way `patch_config` expects. A union rather
/// than a string: the Rust side rejects a path it does not recognise, and
/// finding that out at compile time is better than as a toast.
export type ConfigKey =
    | `zenzai.${keyof ZenzaiConfig}`
    | `conversion.${keyof ConversionConfig}`
    | "plugins.enable";

/// What a save achieved. The two halves are separate because they fail
/// separately: writing the file and telling the running IME about it.
export interface SaveOutcome {
    saved: boolean;
    notified: boolean;
    error?: string;
}

/// Reads the whole document. Throws when it cannot be read, which is a state
/// the UI has to show rather than paper over — see the recovery banner.
export const readConfig = async (): Promise<AppConfig> =>
    await invoke<AppConfig>("get_config");

/// Changes one key. The read-modify-write happens on the Rust side under a
/// lock, so two of these cannot lose each other's change the way the old
/// "read the whole document, edit it, send it back" round trip could.
export const patchConfig = async (
    key: ConfigKey,
    value: unknown
): Promise<SaveOutcome> => {
    try {
        return await invoke<SaveOutcome>("patch_config", { keyPath: key, value });
    } catch (error) {
        // the command itself refused: nothing was written
        return { saved: false, notified: false, error: String(error) };
    }
};

/// Moves an unreadable settings.json aside and starts from the defaults.
/// Destructive, so it is only ever called from the recovery banner, after the
/// user has been told what is wrong and has chosen this.
export const resetConfig = async (): Promise<SaveOutcome> => {
    try {
        return await invoke<SaveOutcome>("reset_config");
    } catch (error) {
        return { saved: false, notified: false, error: String(error) };
    }
};
