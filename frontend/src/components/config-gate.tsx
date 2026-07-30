import { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useConfig } from "@/hooks/use-config";
import { resetConfig } from "@/lib/config";

/// Stands between the settings pages and a settings file that could not be
/// read.
///
/// Opening the app used to repair the file on its own: startup called the
/// create-or-migrate path, which moves an unreadable settings.json aside and
/// writes the defaults. The user was told nothing, and by the time any page
/// rendered there was nothing left to tell them about. Resetting is now
/// something they choose, having been shown what is wrong.
export const ConfigGate = ({ children }: { children: React.ReactNode }) => {
    const { state, refresh } = useConfig();
    const [resetting, setResetting] = useState(false);

    if (state.status === "loading") {
        return (
            <p className="text-sm text-muted-foreground" role="status">
                設定を読み込んでいます…
            </p>
        );
    }

    if (state.status === "unreadable") {
        return (
            <div
                className="space-y-4 rounded-md border border-destructive/50 p-6"
                role="alert"
            >
                <div className="flex items-center space-x-3">
                    <AlertTriangle aria-hidden="true" className="text-destructive" />
                    <h1 className="text-lg font-bold text-foreground">
                        設定ファイルを読み込めません
                    </h1>
                </div>
                <p className="text-sm text-muted-foreground">
                    settings.json の内容が壊れているため、設定を表示できません。
                    元のファイルを残したまま初期状態に戻すか、このまま終了して
                    手動で修正してください。
                </p>
                <p className="text-xs text-muted-foreground break-all">{state.error}</p>
                <div className="flex items-center gap-x-3">
                    <Button
                        variant="destructive"
                        disabled={resetting}
                        onClick={() => {
                            setResetting(true);
                            void resetConfig().finally(() => {
                                setResetting(false);
                                refresh();
                            });
                        }}
                    >
                        バックアップして初期化する
                    </Button>
                    <Button variant="secondary" onClick={refresh}>
                        再読み込み
                    </Button>
                </div>
            </div>
        );
    }

    return <>{children}</>;
};
