import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

import { invokeCommand, patchConfig, resetLearning } from "@/lib/config";

/// The one failure mode the mock in `use-config.test.tsx` cannot express.
///
/// That file replaces `invoke` with a spy that always resolves, so every test
/// written against it describes an app whose commands answer. The bug that
/// shipped was the opposite: a panic inside a `#[tauri::command]` kills the
/// task rather than returning, so `invoke` neither resolves NOR rejects. The
/// UI does not show an error, because there is no error — it just waits, with
/// whatever control triggered the call disabled, forever.
///
/// A promise that never settles cannot be waited for, so these drive the
/// clock instead.
describe("a command that never answers", () => {
    beforeEach(() => {
        invoke.mockReset();
        vi.useFakeTimers();
    });

    afterEach(() => {
        vi.useRealTimers();
    });

    const never = () => new Promise(() => {});

    it("rejects rather than hanging", async () => {
        invoke.mockImplementation(never);

        const call = invokeCommand("get_config");
        const settled = expect(call).rejects.toThrow(/応答しませんでした/);
        await vi.advanceTimersByTimeAsync(20_000);

        await settled;
    });

    it("names the command, so a log has something to search for", async () => {
        invoke.mockImplementation(never);

        const call = invokeCommand("reset_learning");
        const settled = expect(call).rejects.toThrow(/reset_learning/);
        await vi.advanceTimersByTimeAsync(20_000);

        await settled;
    });

    /// A save reports it the way it reports every other refusal, so the
    /// existing UI shows it with no further work.
    it("is reported as a failed save", async () => {
        invoke.mockImplementation(never);

        const call = patchConfig("learning.enable", false);
        await vi.advanceTimersByTimeAsync(20_000);
        const outcome = await call;

        expect(outcome.saved).toBe(false);
        expect(outcome.error).toMatch(/応答しませんでした/);
    });

    /// The reset deliberately throws instead, and the page turns that into
    /// the line under the button.
    it("is thrown by the learning reset", async () => {
        invoke.mockImplementation(never);

        const call = resetLearning();
        const settled = expect(call).rejects.toThrow(/応答しませんでした/);
        await vi.advanceTimersByTimeAsync(20_000);

        await settled;
    });
});

describe("a command that answers", () => {
    beforeEach(() => {
        invoke.mockReset();
    });

    it("passes the value straight through", async () => {
        invoke.mockResolvedValue({ saved: true, notified: true });

        await expect(invokeCommand("patch_config", { keyPath: "learning.enable" })).resolves.toEqual(
            { saved: true, notified: true }
        );
        expect(invoke).toHaveBeenCalledWith("patch_config", { keyPath: "learning.enable" });
    });

    /// The timeout must not swallow a real rejection, which is how the Rust
    /// side reports "the IME is not running".
    it("passes a rejection straight through", async () => {
        invoke.mockRejectedValue("the IME could not be reached; nothing was reset");

        await expect(invokeCommand("reset_learning")).rejects.toBe(
            "the IME could not be reached; nothing was reset"
        );
    });
});
