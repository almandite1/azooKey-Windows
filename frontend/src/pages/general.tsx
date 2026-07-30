import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Button } from "@/components/ui/button";
import { RefreshCcw, ExternalLink } from "lucide-react";

export const General = () => {
    // single-sourced from the workspace version via the Tauri app version
    const [version, setVersion] = useState("");
    useEffect(() => {
        getVersion().then(setVersion).catch(() => setVersion("?"));
    }, []);

    return (
        <div className="space-y-8">
            <h1 className="text-lg font-bold text-foreground">全般</h1>
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
        </div>
    )
}
