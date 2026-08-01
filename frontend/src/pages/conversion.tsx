import { Switch } from "@/components/ui/switch";
import { CaseSensitive, SpellCheck, Sparkles, Type } from "lucide-react";
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from "@/components/ui/select"
import { useConfigKey } from "@/hooks/use-config";
import type { ConfigKey } from "@/lib/config";

// The engine maps anything it does not recognise to "automatic", so a value
// hand-written into settings.json costs nothing; these are the three it knows.
const typoCorrectionModes = [
    { value: "automatic", name: "自動（エンジンに任せる）" },
    { value: "enabled", name: "常に有効" },
    { value: "disabled", name: "無効" },
];

/// One switch over a boolean conversion setting. All five of these differ only
/// in wording and in which key they write, which is where writing them out
/// five times puts a copy-paste mistake.
const CandidateSwitch = ({
    id,
    configKey,
    icon,
    title,
    description,
}: {
    id: string;
    configKey: ConfigKey;
    icon: React.ReactNode;
    title: string;
    description: string;
}) => {
    const field = useConfigKey<boolean>(configKey, false);
    return (
        <div className="flex items-center space-x-4 rounded-md border p-4">
            {icon}
            <div className="flex-1 space-y-1">
                <label htmlFor={id} className="text-sm font-medium leading-none">
                    {title}
                </label>
                <p id={`${id}-description`} className="text-xs text-muted-foreground">
                    {description}
                </p>
            </div>
            <Switch
                id={id}
                aria-describedby={`${id}-description`}
                checked={field.value}
                onCheckedChange={(checked) => void field.commit(checked)}
            />
        </div>
    )
}

export const Conversion = () => {
    const typoCorrection = useConfigKey<string>("conversion.typo_correction", "automatic");

    // Same reason as the other pages: a value that is not one of the presets
    // still has to be visible, or choosing anything would throw it away
    // without ever having shown it.
    const typoOptions = typoCorrectionModes.some((m) => m.value === typoCorrection.value)
        ? typoCorrectionModes
        : [...typoCorrectionModes, { value: typoCorrection.value, name: typoCorrection.value }];

    return (
        <div className="space-y-8">
            <h1 className="text-lg font-bold text-foreground">変換</h1>
            <section className="space-y-2" aria-labelledby="candidates-heading">
                <h2 id="candidates-heading" className="text-sm font-bold text-foreground">候補に含めるもの</h2>
                <CandidateSwitch
                    id="conversion-half-width-kana"
                    configKey="conversion.half_width_kana"
                    icon={<Type aria-hidden="true" />}
                    title="半角カナ"
                    description="ｱｲｳ のような半角カタカナを候補に加えます"
                />
                <CandidateSwitch
                    id="conversion-full-width-roman"
                    configKey="conversion.full_width_roman"
                    icon={<CaseSensitive aria-hidden="true" />}
                    title="全角英数"
                    description="ＡＢＣ のような全角の英数字を候補に加えます"
                />
                <CandidateSwitch
                    id="conversion-typography"
                    configKey="conversion.typography"
                    icon={<Sparkles aria-hidden="true" />}
                    title="装飾文字"
                    description="𝐁𝐎𝐋𝐃 や 𝒜𝓁𝓅𝒽𝒶 のような装飾文字を候補に加えます（英数字のみを入力しているときだけ）"
                />
            </section>
            <section className="space-y-2" aria-labelledby="typo-heading">
                <h2 id="typo-heading" className="text-sm font-bold text-foreground">誤字訂正</h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <SpellCheck aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        {/* SelectTrigger is a <button role="combobox">, so it takes
                            aria-labelledby rather than a <label for> */}
                        <p id="conversion-typo-label" className="text-sm font-medium leading-none">
                            打ち間違いの訂正
                        </p>
                        <p id="conversion-typo-description" className="text-xs text-muted-foreground">
                            入力の打ち間違いを推測して候補を出すかどうかを選びます
                        </p>
                    </div>
                    <Select
                        value={typoCorrection.value}
                        onValueChange={(value) => void typoCorrection.commit(value)}
                    >
                        <SelectTrigger id="conversion-typo" className="w-56" aria-labelledby="conversion-typo-label conversion-typo" aria-describedby="conversion-typo-description">
                            <SelectValue placeholder="動作を選択" />
                        </SelectTrigger>
                        <SelectContent>
                            {typoOptions.map((mode) => (
                                <SelectItem key={mode.value} value={mode.value}>{mode.name}</SelectItem>
                            ))}
                        </SelectContent>
                    </Select>
                </div>
            </section>
        </div>
    )
}
