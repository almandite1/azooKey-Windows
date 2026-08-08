import { invoke } from "@tauri-apps/api/core";

/// Longest a command may leave the UI waiting.
///
/// Deliberately far above any ceiling the Rust side enforces (the server RPCs
/// give up after 3s), because this is not a second opinion on how slow the
/// IME is. It is the guard against a promise that will never settle AT ALL —
/// which is not hypothetical: a panic inside a `#[tauri::command]` kills the
/// task instead of returning, and `invoke` then neither resolves nor
/// rejects. Every control waiting on it stays disabled, with no error to
/// show, forever. That shipped, and the only symptom was a button that did
/// nothing.
const COMMAND_TIMEOUT_MS = 15_000;

/// `invoke`, but it always settles.
///
/// Every call into Rust goes through here. A timeout turns the worst case
/// from "the app is silently broken" into an error message, which the callers
/// below already know how to show.
export const invokeCommand = async <T>(
    command: string,
    args?: Record<string, unknown>
): Promise<T> => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
        return await Promise.race([
            // not `invoke(command, args)` with an undefined second argument:
            // that is a different call, and Tauri's own overloads say so
            args === undefined ? invoke<T>(command) : invoke<T>(command, args),
            new Promise<never>((_, reject) => {
                timer = setTimeout(
                    () =>
                        reject(
                            new Error(
                                `設定アプリの内部処理 (${command}) が応答しませんでした。` +
                                    `IME を再起動しても直らない場合はログ ` +
                                    `(%LOCALAPPDATA%\\Azookey\\logs) を確認してください。`
                            )
                        ),
                    COMMAND_TIMEOUT_MS
                );
            }),
        ]);
    } finally {
        // the loser of the race is abandoned, not cancelled; without this a
        // pending timer keeps the process awake for its full duration
        clearTimeout(timer);
    }
};

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

export interface LearningConfig {
    enable: boolean;
}

export interface PluginsConfig {
    enable: boolean;
    entries: { id: string; enabled: boolean }[];
}

export interface AppConfig {
    version: string;
    zenzai: ZenzaiConfig;
    conversion: ConversionConfig;
    learning: LearningConfig;
    plugins: PluginsConfig;
}

/// Every settable key, spelled the way `patch_config` expects. A union rather
/// than a string: the Rust side rejects a path it does not recognise, and
/// finding that out at compile time is better than as a toast.
export type ConfigKey =
    | `zenzai.${keyof ZenzaiConfig}`
    | `conversion.${keyof ConversionConfig}`
    | "learning.enable"
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
    await invokeCommand<AppConfig>("get_config");

/// Changes one key. The read-modify-write happens on the Rust side under a
/// lock, so two of these cannot lose each other's change the way the old
/// "read the whole document, edit it, send it back" round trip could.
export const patchConfig = async (
    key: ConfigKey,
    value: unknown
): Promise<SaveOutcome> => {
    try {
        return await invokeCommand<SaveOutcome>("patch_config", { keyPath: key, value });
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
        return await invokeCommand<SaveOutcome>("reset_config");
    } catch (error) {
        return { saved: false, notified: false, error: String(error) };
    }
};

/// Forgets everything the engine has learned from confirmed conversions.
///
/// Deliberately not a `SaveOutcome`, and deliberately allowed to throw:
/// nothing is written to disk, so there is no half-success to describe.
/// Either the history was reset or it was not — including the case where the
/// IME is not running, which the caller has to show rather than swallow.
export const resetLearning = async (): Promise<void> =>
    await invokeCommand<void>("reset_learning");
