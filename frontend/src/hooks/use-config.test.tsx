import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
const toast = vi.fn();
vi.mock("sonner", () => ({ toast: (...args: unknown[]) => toast(...args) }));

import { ConfigProvider } from "@/hooks/use-config";
import { General } from "@/pages/general";
import { Conversion } from "@/pages/conversion";
import { Zenzai } from "@/pages/zenzai";

const config = (overrides: Record<string, unknown> = {}) => ({
    version: "0.1.0",
    zenzai: {
        enable: true,
        profile: "",
        backend: "cpu",
        inference_limit: 1,
        context_size: 1024,
        topic: "",
        style: "",
        preference: "",
        ...overrides,
    },
    conversion: {
        half_width_kana: false,
        full_width_roman: false,
        typo_correction: "automatic",
        typography: false,
    },
    plugins: { enable: false, entries: [] },
});

/// Answers `get_config` with `document` and records every `patch_config`.
const withBackend = (
    document: ReturnType<typeof config>,
    patch: (key: string, value: unknown) => unknown = () => ({
        saved: true,
        notified: true,
    })
) => {
    const patches: { key: string; value: unknown }[] = [];
    invoke.mockImplementation((command: string, args: Record<string, unknown>) => {
        if (command === "get_config") return Promise.resolve(document);
        if (command === "check_capability")
            return Promise.resolve({ cpu: true, cuda: false, vulkan: false });
        if (command === "patch_config") {
            patches.push({ key: args.keyPath as string, value: args.value });
            return Promise.resolve(patch(args.keyPath as string, args.value));
        }
        return Promise.resolve(undefined);
    });
    return patches;
};

beforeEach(() => {
    invoke.mockReset();
    toast.mockReset();
});

const renderPage = async (page: React.ReactNode) => {
    render(<ConfigProvider>{page}</ConfigProvider>);
    // the provider reads the config on mount
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_config"));
};

describe("saving a setting", () => {
    /// The regression this whole structured outcome exists for. The file was
    /// written and only the IME could not be told, which used to come back as
    /// a plain error: the switch went back to its old position while the file
    /// said the opposite, and the next restart applied it.
    it("keeps the new value when the file saved but the IME could not be told", async () => {
        withBackend(config(), () => ({
            saved: true,
            notified: false,
            error: "IME に通知できませんでした",
        }));
        await renderPage(<General />);

        const toggle = screen.getByRole("switch", { name: /プラグインを有効化/ });
        await userEvent.click(toggle);

        await waitFor(() => expect(toggle).toBeChecked());
        expect(toast).toHaveBeenCalled();
        // ...and it is a warning about the IME, not a failure to save
        const calls = toast.mock.calls;
        const [, options] = calls[calls.length - 1] as [string, { description?: string }];
        expect(options?.description).toMatch(/IME/);
    });

    /// A save that did NOT reach the file has to put the control back, and the
    /// value it goes back to comes from disk rather than from memory.
    it("returns the control to the stored value when nothing was written", async () => {
        withBackend(config(), () => ({
            saved: false,
            notified: false,
            error: "settings.json は読めません",
        }));
        await renderPage(<General />);

        const toggle = screen.getByRole("switch", { name: /プラグインを有効化/ });
        await userEvent.click(toggle);

        await waitFor(() => expect(toggle).not.toBeChecked());
    });

    /// Two fast clicks used to lose one: the handler computed the new value
    /// from React state that the first click had not updated yet. The switch
    /// reports the value it is moving to, and that is what gets sent.
    it("sends both values when a switch is clicked twice quickly", async () => {
        const patches = withBackend(config());
        await renderPage(<General />);

        const toggle = screen.getByRole("switch", { name: /プラグインを有効化/ });
        await userEvent.click(toggle);
        await userEvent.click(toggle);

        await waitFor(() => expect(patches).toHaveLength(2));
        expect(patches.map((p) => p.value)).toEqual([true, false]);
        expect(patches.every((p) => p.key === "plugins.enable")).toBe(true);
    });

    /// A patch names one key. The whole document used to be sent back, which
    /// carried every other setting along with it as it had been read some
    /// time earlier.
    it("sends only the key that changed", async () => {
        const patches = withBackend(config());
        await renderPage(<General />);

        await userEvent.click(screen.getByRole("switch", { name: /プラグインを有効化/ }));

        await waitFor(() => expect(patches).toHaveLength(1));
        expect(patches[0]).toEqual({ key: "plugins.enable", value: true });
    });
});

describe("text fields", () => {
    /// One save per keystroke meant one RPC per keystroke to a single-threaded
    /// engine, each with a three-second ceiling.
    it("collapse a burst of typing into a single save", async () => {
        vi.useFakeTimers({ shouldAdvanceTime: true });
        const patches = withBackend(config());
        const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });

        render(
            <ConfigProvider>
                <Zenzai />
            </ConfigProvider>
        );
        await waitFor(() => expect(invoke).toHaveBeenCalledWith("get_config"));

        await user.type(screen.getByLabelText("話題"), "開発");
        expect(patches).toHaveLength(0);

        await act(async () => {
            vi.advanceTimersByTime(600);
        });

        await waitFor(() => expect(patches).toHaveLength(1));
        expect(patches[0]).toEqual({ key: "zenzai.topic", value: "開発" });
    });

    /// Leaving the field must not lose what is in it, whatever the timer is
    /// doing.
    it("save immediately on blur", async () => {
        const patches = withBackend(config());
        await renderPage(<Zenzai />);

        const topic = screen.getByLabelText("話題");
        await userEvent.type(topic, "開発");
        await userEvent.tab();

        await waitFor(() => expect(patches[patches.length - 1]?.key).toBe("zenzai.topic"));
        expect(patches[patches.length - 1]?.value).toBe("開発");
    });
});

describe("values outside the presets", () => {
    /// The engine accepts any of 1..10 and settings.json is hand-editable, so
    /// a stored 7 used to render as an empty combobox — and choosing anything
    /// then threw the 7 away without ever having shown it.
    it("shows a stored inference limit that is not one of the presets", async () => {
        withBackend(config({ inference_limit: 7 }));
        await renderPage(<Zenzai />);

        await waitFor(() =>
            expect(screen.getByRole("combobox", { name: /推論上限/ })).toHaveTextContent("7")
        );
    });

    it("shows a stored backend the app does not know", async () => {
        withBackend(config({ backend: "rocm" }));
        await renderPage(<Zenzai />);

        await waitFor(() =>
            expect(screen.getByRole("combobox", { name: /バックエンド/ })).toHaveTextContent("rocm")
        );
    });
});

/// `config()` spreads its overrides into the zenzai section, which is the one
/// every other test needs; these need the section next to it.
const withConversion = (overrides: Record<string, unknown> = {}) => {
    const document = config();
    return { ...document, conversion: { ...document.conversion, ...overrides } };
};

describe("the conversion settings", () => {
    /// The key path is the whole risk here. A setting that writes
    /// "zenzai.half_width_kana" is rejected by the Rust side and looks exactly
    /// like a control that does nothing, so pin the path the switch sends.
    it("writes the key it names", async () => {
        const patches = withBackend(withConversion());
        await renderPage(<Conversion />);

        await userEvent.click(screen.getByRole("switch", { name: /半角カナ/ }));

        await waitFor(() =>
            expect(patches).toContainEqual({ key: "conversion.half_width_kana", value: true })
        );
    });

    it("shows the stored state of each switch", async () => {
        withBackend(withConversion({ full_width_roman: true, typography: true }));
        await renderPage(<Conversion />);

        await waitFor(() => expect(screen.getByRole("switch", { name: /全角英数/ })).toBeChecked());
        expect(screen.getByRole("switch", { name: /装飾文字/ })).toBeChecked();
        expect(screen.getByRole("switch", { name: /半角カナ/ })).not.toBeChecked();
    });

    /// settings.json is hand-editable and the engine accepts anything (it
    /// treats what it does not know as "automatic"), so a value outside the
    /// three presets must still be shown rather than silently replaced.
    it("shows a stored typo correction mode that is not one of the presets", async () => {
        withBackend(withConversion({ typo_correction: "aggressive" }));
        await renderPage(<Conversion />);

        await waitFor(() =>
            expect(screen.getByRole("combobox", { name: /打ち間違いの訂正/ })).toHaveTextContent(
                "aggressive"
            )
        );
    });
});

describe("the advanced controls", () => {
    /// They all follow the master switch; a control that stayed live with
    /// Zenzai off would write settings that do nothing.
    it("are disabled while Zenzai is off", async () => {
        withBackend(config({ enable: false }));
        await renderPage(<Zenzai />);

        for (const label of ["変換プロファイル", "話題", "文体", "好み"]) {
            expect(screen.getByLabelText(label)).toBeDisabled();
        }
        for (const name of [/バックエンド/, /推論上限/]) {
            expect(screen.getByRole("combobox", { name })).toBeDisabled();
        }
    });
});
