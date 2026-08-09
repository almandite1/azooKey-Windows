import { useState } from "react";
import { History, Trash2 } from "lucide-react";
import { Switch } from "@/components/ui/switch";
import { Button } from "@/components/ui/button";
import { useConfigKey } from "@/hooks/use-config";
import { resetLearning } from "@/lib/config";

/// The reset, with its confirmation inline rather than in a modal — the same
/// shape the unreadable-settings recovery uses (config-gate.tsx). A dialog
/// here would be the only one in the app.
const ResetPanel = () => {
    const [confirming, setConfirming] = useState(false);
    const [resetting, setResetting] = useState(false);
    const [result, setResult] = useState<string | null>(null);

    if (!confirming) {
        return (
            <div className="space-y-2">
                <Button
                    variant="outline"
                    onClick={() => {
                        setResult(null);
                        setConfirming(true);
                    }}
                >
                    <Trash2 aria-hidden="true" />
                    学習履歴をリセット…
                </Button>
                {result !== null && (
                    <p className="text-xs text-muted-foreground" role="status">
                        {result}
                    </p>
                )}
            </div>
        );
    }

    return (
        <div
            className="space-y-4 rounded-md border border-destructive/50 p-4"
            role="alert"
        >
            <p className="text-sm text-muted-foreground">
                保存された学習履歴をすべて削除します。この操作は取り消せません。
            </p>
            <div className="flex items-center gap-x-3">
                <Button
                    variant="destructive"
                    disabled={resetting}
                    onClick={() => {
                        setResetting(true);
                        resetLearning()
                            .then(() => setResult("学習履歴を削除しました。"))
                            // The message is whatever the command said — it
                            // distinguishes "the IME is not running" from "the
                            // IME refused", and the user can act on each.
                            .catch((error: unknown) => setResult(String(error)))
                            .finally(() => {
                                setResetting(false);
                                setConfirming(false);
                            });
                    }}
                >
                    削除する
                </Button>
                <Button
                    variant="secondary"
                    disabled={resetting}
                    onClick={() => setConfirming(false)}
                >
                    キャンセル
                </Button>
            </div>
        </div>
    );
};

export const Learning = () => {
    const enabled = useConfigKey<boolean>("learning.enable", true);

    return (
        <div className="space-y-8">
            <h1 className="text-lg font-bold text-foreground">学習</h1>
            <section className="space-y-2" aria-labelledby="learning-heading">
                <h2 id="learning-heading" className="text-sm font-bold text-foreground">
                    入力履歴
                </h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <History aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <label
                            htmlFor="learning-enable"
                            className="text-sm font-medium leading-none"
                        >
                            入力履歴からの学習
                        </label>
                        {/* The disk is named on purpose. This is the one
                            setting that is on by default AND writes what the
                            user has typed to a file, so where it goes is part
                            of the setting, not a footnote. */}
                        <p
                            id="learning-enable-description"
                            className="text-xs text-muted-foreground"
                        >
                            確定した変換をこのPCに保存し、次回から候補の順位に反映します。
                            履歴は設定フォルダー（%APPDATA%\Azookey）内に保存されます。
                        </p>
                    </div>
                    <Switch
                        id="learning-enable"
                        aria-describedby="learning-enable-description"
                        checked={enabled.value}
                        onCheckedChange={(checked) => void enabled.commit(checked)}
                    />
                </div>
            </section>
            <section className="space-y-2" aria-labelledby="learning-reset-heading">
                <h2
                    id="learning-reset-heading"
                    className="text-sm font-bold text-foreground"
                >
                    学習履歴の削除
                </h2>
                <ResetPanel />
            </section>
        </div>
    );
};
