import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { ExternalLink, RefreshCcw } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Button } from "@/components/ui/button";

export const About = () => {
    // single-sourced from the workspace version via the Tauri app version
    const [version, setVersion] = useState("");
    useEffect(() => {
        getVersion().then(setVersion).catch(() => setVersion("?"));
    }, []);

    return (
        <div className="space-y-8">
            <h1 className="text-lg font-bold text-foreground">このソフトについて</h1>
            <section className="space-y-2" aria-labelledby="version-heading">
                <h2 id="version-heading" className="text-sm font-bold text-foreground">バージョンと更新プログラム</h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <RefreshCcw aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <p className="text-sm font-medium leading-none">
                            v{version}
                        </p>
                    </div>
                    {/* openUrl, not target="_blank": WebView2 drops the new-window
                        request, so the link does nothing in a release build */}
                    <Button variant="secondary" onClick={() => void openUrl("https://github.com/almandite1/azooKey-Windows/releases")}>
                        <ExternalLink aria-hidden="true" />
                        更新を確認する
                    </Button>
                </div>
            </section>
            <section className="space-y-2" aria-labelledby="community-heading">
                <h2 id="community-heading" className="text-sm font-bold text-foreground">コミュニティ</h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <ExternalLink aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <p className="text-sm font-medium leading-none">
                            Discord
                        </p>
                        <p className="text-xs text-muted-foreground">
                            Azookey公式Discordサーバーに参加して、最新情報を入手する
                        </p>
                    </div>
                    <Button variant="secondary" onClick={() => void openUrl("https://discord.com/invite/dY9gHuyZN5")}>
                        参加する
                    </Button>
                </div>
            </section>
        </div>
    )
}
