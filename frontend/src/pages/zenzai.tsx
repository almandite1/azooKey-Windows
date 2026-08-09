import { Textarea } from "@/components/ui/textarea";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Bot, User, Cpu, Gauge, MessageSquare, Type, Heart, Ruler } from "lucide-react";
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from "@/components/ui/select"
import { useEffect, useState } from "react";
import { toast } from "sonner"
import { invokeCommand } from "@/lib/config";
import { useConfigKey } from "@/hooks/use-config";
import type { ConfigKey, SaveOutcome } from "@/lib/config";

// the reason a backend is unavailable used to live in a tooltip, but that put
// a <button> around the role="option" -- which cost the item its accessible
// name and broke arrow-key navigation. Say it inline instead.
const BackendSelectItem = ({
    name,
    value,
    disabled,
    reason
}: {
    name: string;
    value: string;
    disabled: boolean;
    reason: string;
}) => {
    return (
        <SelectItem value={value} disabled={disabled}>
            {name}
            {disabled && reason && (
                <span className="ml-2 text-xs text-muted-foreground">{reason}</span>
            )}
        </SelectItem>
    )
}

/// The presets, plus the stored value when it is not one of them.
///
/// A stored value outside the presets must still be visible. The engine accepts
/// any of 1..10 and settings.json is hand-editable, so a saved 7 used to render
/// as an empty combobox -- and picking anything then threw the 7 away without
/// ever having shown it. Returns the preset array itself when nothing has to be
/// added, so the common case allocates nothing and the array stays untouched.
const withStoredValue = (presets: number[], stored: number): number[] =>
    presets.includes(stored) ? presets : [...presets, stored].sort((a, b) => a - b);

/// Saves, and says so when the change will not be felt until a restart.
///
/// Both settings this is used for are read once, while the engine starts, so
/// silence after changing one looks exactly like a setting that does nothing.
const commitWithRestartToast = <T,>(
    commit: (value: T) => Promise<SaveOutcome>,
    title: string
) => (value: T) => {
    void commit(value).then((outcome) => {
        if (outcome.saved) {
            toast(title, {
                description: "変更を適用するには、PCを再起動してください",
                duration: 10000,
            });
        }
    });
};

// the engine accepts 1..10; these are the round numbers inside that range,
// not a separate policy
const inferenceLimits = [1, 3, 5, 10];

// The engine accepts 512..4096, but it then clamps to what the model was
// trained on, and the zenz weights that ship with this build stop at 1024.
// Offering more would be offering a number that silently becomes 1024.
const contextSizes = [512, 1024];

const backends = [
    { value: "cpu", name: "CPU (非推奨)", reason: "" },
    { value: "cuda", name: "CUDA (NVIDIA GPU)", reason: "CUDA Toolkit 12をインストールする必要があります" },
    { value: "vulkan", name: "Vulkan", reason: "お使いのPCはVulkanに対応していません" },
];

/// One of the three v3 context strings. They differ only in wording, and the
/// engine treats them identically: a short hint, or an empty string for
/// "say nothing".
const ContextInput = ({
    id,
    configKey,
    icon,
    title,
    description,
    placeholder,
    disabled,
}: {
    id: string;
    configKey: ConfigKey;
    icon: React.ReactNode;
    title: string;
    description: string;
    placeholder: string;
    disabled: boolean;
}) => {
    // typing is debounced and flushed on blur: it used to write the file on
    // every keystroke, and each write is an RPC to a single-threaded engine
    const field = useConfigKey<string>(configKey, "");
    return (
        <div className="space-y-4 rounded-md border p-4">
            <div className="flex items-center space-x-4">
                {icon}
                <div className="flex-1 space-y-1">
                    <label htmlFor={id} className="text-sm font-medium leading-none">
                        {title}
                    </label>
                    <p id={`${id}-description`} className="text-xs text-muted-foreground">
                        {description}
                    </p>
                </div>
            </div>
            <Input
                id={id}
                aria-describedby={`${id}-description`}
                placeholder={placeholder}
                value={field.value}
                disabled={disabled}
                onChange={(event) => field.onType(event.target.value)}
                onBlur={field.flush}
            />
        </div>
    )
}

export const Zenzai = () => {
    const enable = useConfigKey<boolean>("zenzai.enable", false);
    const profile = useConfigKey<string>("zenzai.profile", "");
    const backend = useConfigKey<string>("zenzai.backend", "cpu");
    const inferenceLimit = useConfigKey<number>("zenzai.inference_limit", 1);
    const contextSize = useConfigKey<number>("zenzai.context_size", 1024);

    const [capability, setCapability] = useState({
        cpu: true,
        cuda: false,
        vulkan: false,
    });

    useEffect(() => {
        invokeCommand<{ cpu: boolean; cuda: boolean; vulkan: boolean }>("check_capability")
            .then(setCapability)
            // without this the rejection was unhandled and the list silently
            // stayed CPU-only, which looks identical to "your machine has no GPU"
            .catch(() => {
                setCapability({ cpu: true, cuda: false, vulkan: false });
                toast("利用可能なバックエンドを判定できませんでした", {
                    description: "CPUのみ選択できます",
                });
            });
    }, []);

    const capabilityOf = (value: string) =>
        value === "cuda" ? capability.cuda : value === "vulkan" ? capability.vulkan : capability.cpu;

    const limitOptions = withStoredValue(inferenceLimits, inferenceLimit.value);
    const contextSizeOptions = withStoredValue(contextSizes, contextSize.value);
    // Same idea, kept separate: these are objects that need a label and an
    // availability reason built for them, not numbers to be sorted.
    const backendOptions = backends.some((b) => b.value === backend.value)
        ? backends
        : [...backends, { value: backend.value, name: backend.value, reason: "" }];

    const disabled = !enable.value;

    return (
        <div className="space-y-8">
            <h1 className="text-lg font-bold text-foreground">Zenzai</h1>
            <section className="space-y-2" aria-labelledby="zenzai-basic-heading">
                <h2 id="zenzai-basic-heading" className="text-sm font-bold text-foreground">基本設定</h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <Bot aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <label htmlFor="zenzai-enable" className="text-sm font-medium leading-none">
                            Zenzaiを有効化
                        </label>
                        <p id="zenzai-enable-description" className="text-xs text-muted-foreground">
                            Zenzaiを有効にして、変換精度を向上させます
                        </p>
                    </div>
                    <Switch
                        id="zenzai-enable"
                        aria-describedby="zenzai-enable-description"
                        checked={enable.value}
                        onCheckedChange={(checked) => void enable.commit(checked)}
                    />
                </div>
                <div className="space-y-4 rounded-md border p-4">
                    <div className="flex items-center space-x-4 ">
                        <User aria-hidden="true" />
                        <div className="flex-1 space-y-1">
                            <label htmlFor="zenzai-profile" className="text-sm font-medium leading-none">
                                変換プロファイル
                            </label>
                            <p id="zenzai-profile-description" className="text-xs text-muted-foreground">
                                Zenzaiで利用されるユーザー情報を設定します
                            </p>
                        </div>
                    </div>
                    <Textarea
                        id="zenzai-profile"
                        aria-describedby="zenzai-profile-description"
                        placeholder="例）山田太郎、数学科の学生。"
                        value={profile.value}
                        disabled={disabled}
                        onChange={(event) => profile.onType(event.target.value)}
                        onBlur={profile.flush}
                    />
                </div>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <Cpu aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        {/* SelectTrigger is a <button role="combobox">, so it takes
                            aria-labelledby rather than a <label for> */}
                        <p id="zenzai-backend-label" className="text-sm font-medium leading-none">
                            バックエンド
                        </p>
                        <p id="zenzai-backend-description" className="text-xs text-muted-foreground">
                            Zenzaiを利用するバックエンドを選択します
                        </p>
                    </div>
                    <Select
                        disabled={disabled}
                        value={backend.value}
                        onValueChange={commitWithRestartToast(
                            backend.commit,
                            "バックエンドが変更されました"
                        )}
                    >
                        <SelectTrigger id="zenzai-backend" className="w-48" aria-labelledby="zenzai-backend-label zenzai-backend" aria-describedby="zenzai-backend-description">
                            <SelectValue placeholder="バックエンドを選択" />
                        </SelectTrigger>
                        <SelectContent>
                            {backendOptions.map((option) => (
                                <BackendSelectItem
                                    key={option.value}
                                    name={option.name}
                                    value={option.value}
                                    disabled={!capabilityOf(option.value)}
                                    reason={option.reason}
                                />
                            ))}
                        </SelectContent>
                    </Select>
                </div>
            </section>
            <section className="space-y-2" aria-labelledby="zenzai-advanced-heading">
                <h2 id="zenzai-advanced-heading" className="text-sm font-bold text-foreground">詳細設定</h2>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <Gauge aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <p id="zenzai-inference-limit-label" className="text-sm font-medium leading-none">
                            推論上限
                        </p>
                        <p id="zenzai-inference-limit-description" className="text-xs text-muted-foreground">
                            大きいほど変換品質が上がり、変換が遅くなります（既定: 1）
                        </p>
                    </div>
                    <Select
                        disabled={disabled}
                        value={String(inferenceLimit.value)}
                        onValueChange={(value) => void inferenceLimit.commit(Number(value))}
                    >
                        <SelectTrigger id="zenzai-inference-limit" className="w-48" aria-labelledby="zenzai-inference-limit-label zenzai-inference-limit" aria-describedby="zenzai-inference-limit-description">
                            <SelectValue placeholder="推論上限を選択" />
                        </SelectTrigger>
                        <SelectContent>
                            {limitOptions.map((limit) => (
                                <SelectItem key={limit} value={String(limit)}>{limit}</SelectItem>
                            ))}
                        </SelectContent>
                    </Select>
                </div>
                <div className="flex items-center space-x-4 rounded-md border p-4">
                    <Ruler aria-hidden="true" />
                    <div className="flex-1 space-y-1">
                        <p id="zenzai-context-size-label" className="text-sm font-medium leading-none">
                            コンテキスト長
                        </p>
                        <p id="zenzai-context-size-description" className="text-xs text-muted-foreground">
                            一度に扱えるトークン数です。これを超える長さの入力はZenzaiを使わずに変換されます。モデルの上限を超える値は自動的に切り詰められます（既定: 1024）
                        </p>
                    </div>
                    <Select
                        disabled={disabled}
                        value={String(contextSize.value)}
                        onValueChange={(value) =>
                            commitWithRestartToast(
                                contextSize.commit,
                                "コンテキスト長が変更されました"
                            )(Number(value))
                        }
                    >
                        <SelectTrigger id="zenzai-context-size" className="w-48" aria-labelledby="zenzai-context-size-label zenzai-context-size" aria-describedby="zenzai-context-size-description">
                            <SelectValue placeholder="コンテキスト長を選択" />
                        </SelectTrigger>
                        <SelectContent>
                            {contextSizeOptions.map((size) => (
                                <SelectItem key={size} value={String(size)}>{size}</SelectItem>
                            ))}
                        </SelectContent>
                    </Select>
                </div>
                <ContextInput
                    id="zenzai-topic"
                    configKey="zenzai.topic"
                    icon={<MessageSquare aria-hidden="true" />}
                    title="話題"
                    description="いま書いている内容の話題を10〜20文字程度で設定します"
                    placeholder="例）ソフトウェア開発"
                    disabled={disabled}
                />
                <ContextInput
                    id="zenzai-style"
                    configKey="zenzai.style"
                    icon={<Type aria-hidden="true" />}
                    title="文体"
                    description="文章のスタイルを10〜20文字程度で設定します"
                    placeholder="例）ですます調"
                    disabled={disabled}
                />
                <ContextInput
                    id="zenzai-preference"
                    configKey="zenzai.preference"
                    icon={<Heart aria-hidden="true" />}
                    title="好み"
                    description="変換の好みを10〜20文字程度で設定します"
                    placeholder="例）漢字は控えめに"
                    disabled={disabled}
                />
            </section>
        </div>
    )
}
