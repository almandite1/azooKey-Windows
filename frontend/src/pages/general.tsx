import { Switch } from "@/components/ui/switch";
import { Puzzle } from "lucide-react";
import { useConfigKey } from "@/hooks/use-config";

export const General = () => {
    // the switch reports the value it is moving TO; computing it from the
    // previous React state instead is what lost one of two fast clicks, and
    // what made the switch disagree with a file changed elsewhere
    const plugins = useConfigKey<boolean>("plugins.enable", false);

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
                    <Switch
                        id="plugins-enable"
                        aria-describedby="plugins-enable-description"
                        checked={plugins.value}
                        onCheckedChange={(checked) => void plugins.commit(checked)}
                    />
                </div>
            </section>
        </div>
    )
}
