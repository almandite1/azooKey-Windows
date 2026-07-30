import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";

/// The settings file is the source of truth and every page edits a
/// different corner of it, so both of these go to disk rather than to a
/// snapshot: two pages open at once (or a hand edit) must not revert each
/// other.

/// Read the whole config, or null when it could not be read -- callers keep
/// their defaults in that case rather than showing values that are not what
/// is stored.
export const readConfig = async (): Promise<any | null> => {
    try {
        return await invoke<any>("get_config");
    } catch {
        return null;
    }
};

/// Save one change: re-read, apply, write the whole config back. Returns
/// what was written, or null if the save failed -- the caller uses that to
/// decide whether to move its own state.
export const updateConfig = async (
    updater: (config: any) => void
): Promise<any | null> => {
    try {
        const data = await invoke<any>("get_config");
        updater(data);
        await invoke("update_config", { newConfig: data });
        return data;
    } catch (error) {
        // the reason matters here: a settings.json written by a newer
        // version is refused rather than overwritten
        toast(`設定の更新に失敗しました: ${error}`);
        return null;
    }
};
