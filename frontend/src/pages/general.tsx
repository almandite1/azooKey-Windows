import { useEffect, useState } from "react";
import { Switch } from "@/components/ui/switch";
import { Puzzle } from "lucide-react";
import { readConfig, updateConfig } from "@/lib/config";

export const General = () => {
    const [pluginsEnabled, setPluginsEnabled] = useState(false);

    useEffect(() => {
        readConfig().then((data) => {
            if (data) {
                setPluginsEnabled(data.plugins.enable);
            }
        });
    }, []);

    // the server re-reads this on every UpdateConfig and resets its circuit
    // breaker with it, so the switch takes effect on the next keystroke
    const handlePluginsChange = async () => {
        const data = await updateConfig((config) => {
            config.plugins.enable = !pluginsEnabled;
        });

        if (data) {
            setPluginsEnabled(data.plugins.enable);
        }
    };

    return (
        <div className="space-y-8">
            <h1 className="text-lg font-bold text-foreground">全般</h1>
            <section className="space-y-2" aria-labelledby="plugins-heading">
                <h2 id="plugins-heading" className="text-sm font-bold text-foreground">プラグイン</h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <Puzzle aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <label htmlFor="plugins-enable" className="text-sm font-medium leading-none">
                            プラグインを有効化
                        </label>
                        <p id="plugins-enable-description" className="text-xs text-muted-foreground">
                            プラグインが変換候補を追加できるようになります
                        </p>
                    </div>
                    <Switch id="plugins-enable" aria-describedby="plugins-enable-description" checked={pluginsEnabled} onCheckedChange={handlePluginsChange} />
                </div>
            </section>
        </div>
    )
}
