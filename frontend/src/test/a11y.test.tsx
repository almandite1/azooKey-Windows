import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, waitFor, screen } from "@testing-library/react";
import { axe } from "jest-axe";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("sonner", () => ({ toast: vi.fn() }));

import { ConfigProvider } from "@/hooks/use-config";
import { General } from "@/pages/general";
import { Conversion } from "@/pages/conversion";
import { Zenzai } from "@/pages/zenzai";
import { About } from "@/pages/about";

/// The accessibility properties that were fixed by hand and have nothing
/// holding them: every control's label association, the decorative icons
/// being hidden, one h1 per page, and buttons that are not submit buttons.
/// They were all wrong at once before, and nothing failed.

beforeEach(() => {
    invoke.mockReset();
    invoke.mockImplementation((command: string) => {
        if (command === "get_config")
            return Promise.resolve({
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
                },
                conversion: {
                    half_width_kana: false,
                    full_width_roman: false,
                    english_in_roman_input: false,
                    typo_correction: "automatic",
                    typography: false,
                },
                plugins: { enable: false, entries: [] },
            });
        if (command === "check_capability")
            return Promise.resolve({ cpu: true, cuda: false, vulkan: false });
        return Promise.resolve(undefined);
    });
});

const pages: [string, React.ReactNode][] = [
    ["全般", <General key="general" />],
    ["変換", <Conversion key="conversion" />],
    ["Zenzai", <Zenzai key="zenzai" />],
    ["このソフトについて", <About key="about" />],
];

describe.each(pages)("%s", (_name, page) => {
    it("has no detectable accessibility violations", async () => {
        const { container } = render(<ConfigProvider>{page}</ConfigProvider>);
        await waitFor(() => expect(invoke).toHaveBeenCalled());

        const results = await axe(container);
        expect(results.violations).toEqual([]);
    });

    it("has exactly one top-level heading", async () => {
        render(<ConfigProvider>{page}</ConfigProvider>);
        await waitFor(() => expect(invoke).toHaveBeenCalled());

        expect(screen.getAllByRole("heading", { level: 1 })).toHaveLength(1);
    });

    /// A bare <button> defaults to type="submit", which is a trap for the
    /// first <form> anyone adds.
    it("has no implicit submit buttons", async () => {
        const { container } = render(<ConfigProvider>{page}</ConfigProvider>);
        await waitFor(() => expect(invoke).toHaveBeenCalled());

        const untyped = [...container.querySelectorAll("button")].filter(
            (button) => !button.getAttribute("type")
        );
        expect(untyped).toEqual([]);
    });

    /// The nesting that cost a Select item its accessible name and broke
    /// arrow-key navigation.
    it("nests no interactive element inside another", async () => {
        const { container } = render(<ConfigProvider>{page}</ConfigProvider>);
        await waitFor(() => expect(invoke).toHaveBeenCalled());

        expect(
            container.querySelectorAll("button a, button button, a a, a button")
        ).toHaveLength(0);
    });
});
