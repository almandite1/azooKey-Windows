import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
    type AppConfig,
    type ConfigKey,
    type SaveOutcome,
    patchConfig,
    readConfig,
} from "@/lib/config";

/// How long a text field waits after the last keystroke before saving.
///
/// It used to save on every one. Each save is an RPC to a single-threaded
/// engine that can legitimately be busy converting, so a slow answer froze
/// the field per character — and the failure path threw the channel away, so
/// the next character rebuilt a whole tokio runtime as well.
const TEXT_DEBOUNCE_MS = 500;

type ConfigState =
    | { status: "loading" }
    | { status: "ready"; config: AppConfig }
    | { status: "unreadable"; error: string };

interface ConfigContextValue {
    state: ConfigState;
    /// Re-reads from disk. The file is the source of truth and this app is not
    /// its only writer.
    refresh: () => void;
    /// Saves one key, moving local state to the new value when the FILE took
    /// it — whether or not the running IME could be told.
    setKey: (key: ConfigKey, value: unknown) => Promise<SaveOutcome>;
}

const ConfigContext = createContext<ConfigContextValue | undefined>(undefined);

/// Reports a save in terms of what actually happened.
///
/// The old code returned one error for both halves, and the UI reverted the
/// control on any of them. So flipping a switch while the IME was stopped put
/// the switch back and said "failed" — while the file said the opposite, and
/// the next restart applied it. Saved-but-not-notified is a warning, not a
/// failure.
const reportSave = (outcome: SaveOutcome) => {
    if (!outcome.saved) {
        toast(`設定を保存できませんでした: ${outcome.error ?? "原因不明"}`);
        return;
    }
    if (!outcome.notified) {
        toast("設定を保存しました", {
            description:
                outcome.error ??
                "IME に通知できなかったため、次回の起動時に反映されます",
            duration: 8000,
        });
    }
};

export const ConfigProvider = ({ children }: { children: React.ReactNode }) => {
    const [state, setState] = useState<ConfigState>({ status: "loading" });

    const refresh = useCallback(() => {
        readConfig()
            .then((config) => setState({ status: "ready", config }))
            .catch((error) => setState({ status: "unreadable", error: String(error) }));
    }, []);

    useEffect(() => {
        refresh();
        // The settings app is not the only writer, and it can sit open for a
        // long time. Re-read when the window comes back to the front, so what
        // is shown is what is stored.
        const onFocus = () => refresh();
        window.addEventListener("focus", onFocus);
        return () => window.removeEventListener("focus", onFocus);
    }, [refresh]);

    const setKey = useCallback(async (key: ConfigKey, value: unknown) => {
        // optimistic: the file is written before the IME is told, and the
        // notification is the half allowed to fail
        setState((current) =>
            current.status === "ready"
                ? { status: "ready", config: applyKey(current.config, key, value) }
                : current
        );

        const outcome = await patchConfig(key, value);
        reportSave(outcome);
        if (!outcome.saved) {
            // nothing was written, so the control must go back to the file
            readConfig()
                .then((config) => setState({ status: "ready", config }))
                .catch((error) =>
                    setState({ status: "unreadable", error: String(error) })
                );
        }
        return outcome;
    }, []);

    return (
        <ConfigContext.Provider value={{ state, refresh, setKey }}>
            {children}
        </ConfigContext.Provider>
    );
};

/// Sets one dotted key on a copy of the config, for the optimistic update.
/// The authoritative version of this lives in Rust; this only has to keep the
/// screen in step until the next read.
const applyKey = (config: AppConfig, key: ConfigKey, value: unknown): AppConfig => {
    const [section, leaf] = key.split(".") as [keyof AppConfig, string];
    return {
        ...config,
        [section]: { ...(config[section] as object), [leaf]: value },
    };
};

export const useConfig = () => {
    const context = useContext(ConfigContext);
    if (context === undefined) {
        throw new Error("useConfig must be used within a ConfigProvider");
    }
    return context;
};

/// One setting, ready to bind to a control.
///
/// `value` follows the file; `commit` saves immediately (switches, selects).
/// `onType`/`flush` are for text: they hold the keystrokes for
/// `TEXT_DEBOUNCE_MS` and write once, and `flush` is what a blur calls so
/// leaving the field never loses the last thing typed.
export const useConfigKey = <T,>(key: ConfigKey, fallback: T) => {
    const { state, setKey } = useConfig();
    const stored = state.status === "ready" ? readKey<T>(state.config, key) : fallback;

    const [draft, setDraft] = useState<T | null>(null);
    const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
    const pending = useRef<T | null>(null);

    // the file changed underneath us (another writer, or a refresh): the
    // draft is stale and the stored value wins
    useEffect(() => {
        if (pending.current === null) {
            setDraft(null);
        }
    }, [stored]);

    const commit = useCallback(
        (value: T) => {
            setDraft(null);
            pending.current = null;
            if (timer.current) {
                clearTimeout(timer.current);
                timer.current = null;
            }
            return setKey(key, value);
        },
        [key, setKey]
    );

    const flush = useCallback(() => {
        if (pending.current === null) return;
        const value = pending.current;
        pending.current = null;
        if (timer.current) {
            clearTimeout(timer.current);
            timer.current = null;
        }
        setDraft(null);
        void setKey(key, value);
    }, [key, setKey]);

    const onType = useCallback(
        (value: T) => {
            setDraft(value);
            pending.current = value;
            if (timer.current) clearTimeout(timer.current);
            timer.current = setTimeout(() => {
                timer.current = null;
                flush();
            }, TEXT_DEBOUNCE_MS);
        },
        [flush]
    );

    // a field left mid-edit by unmounting (a page change) still saves
    useEffect(() => () => flush(), [flush]);

    return {
        value: draft ?? stored,
        commit,
        onType,
        flush,
    };
};

const readKey = <T,>(config: AppConfig, key: ConfigKey): T => {
    const [section, leaf] = key.split(".") as [keyof AppConfig, string];
    return (config[section] as unknown as Record<string, unknown>)[leaf] as T;
};
