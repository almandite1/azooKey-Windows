import { Textarea } from "@/components/ui/textarea";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Bot, User, Cpu, Gauge, MessageSquare, Type, Heart } from "lucide-react";
import {
    Select,
    SelectContent,
    SelectItem,
    SelectTrigger,
    SelectValue,
} from "@/components/ui/select"
import { useEffect, useState } from "react";
import { toast } from "sonner"
import { invoke } from '@tauri-apps/api/core';
import { readConfig, updateConfig } from "@/lib/config";

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

// the engine clamps to 1..10; these are the round numbers inside that range,
// not a separate policy
const inferenceLimits = [1, 3, 5, 10];

/// One of the three v3 context strings. They differ only in wording, and the
/// engine treats them identically: a short hint, or an empty string for
/// "say nothing".
const ContextInput = ({
    id,
    icon,
    title,
    description,
    placeholder,
    value,
    disabled,
    onChange,
}: {
    id: string;
    icon: React.ReactNode;
    title: string;
    description: string;
    placeholder: string;
    value: string;
    disabled: boolean;
    onChange: (value: string) => void;
}) => {
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
                value={value}
                disabled={disabled}
                onChange={(event) => onChange(event.target.value)}
            />
        </div>
    )
}

export const Zenzai = () => {
    const [value, setValue] = useState({
        enable: false,
        profile: "",
        backend: "",
        inferenceLimit: 1,
        topic: "",
        style: "",
        preference: "",
    });

    const [capability, setCapability] = useState({
        cpu: true,
        cuda: false,
        vulkan: false,
    });

    // Load config on component mount
    useEffect(() => {
        readConfig().then((data) => {
            // Keep default values if config fetch fails
            if (!data) return;
            const zenzai = data.zenzai;
            setValue({
                enable: zenzai.enable,
                profile: zenzai.profile,
                backend: zenzai.backend,
                inferenceLimit: zenzai.inference_limit,
                topic: zenzai.topic,
                style: zenzai.style,
                preference: zenzai.preference,
            });
        });

        invoke("check_capability").then((capability: any) => {
            setCapability({
                cpu: capability["cpu"],
                cuda: capability["cuda"],
                vulkan: capability["vulkan"],
            });
        })
    }, []);

    const handleZenzaiChange = async () => {
        const data = await updateConfig((data) => {
            data.zenzai.enable = !value.enable;
        });

        if (data) {
            setValue((prev) => ({ ...prev, enable: data.zenzai.enable }));
        }
    };

    const handleProfileChange = (event: React.ChangeEvent<HTMLTextAreaElement>) => {
        const newProfile = event.target.value;
        setValue((prev) => ({ ...prev, profile: newProfile }));

        updateConfig((data) => {
            data.zenzai.profile = newProfile;
        });
    };

    const handleBackendChange = async (backend: string) => {
        const data = await updateConfig((data) => {
            data.zenzai.backend = backend;
        });

        if (data) {
            setValue((prev) => ({ ...prev, backend }));
            toast("バックエンドが変更されました", {
                description: "変更を適用するには、PCを再起動してください",
                duration: 10000,
            });
        }
    };

    const handleInferenceLimitChange = async (limit: string) => {
        const parsed = Number(limit);
        const data = await updateConfig((data) => {
            data.zenzai.inference_limit = parsed;
        });

        if (data) {
            setValue((prev) => ({ ...prev, inferenceLimit: parsed }));
        }
    };

    // one handler for all three v3 context strings: the key on the config is
    // the only thing that differs
    const handleContextChange = (
        key: "topic" | "style" | "preference"
    ) => (next: string) => {
        setValue((prev) => ({ ...prev, [key]: next }));

        updateConfig((data) => {
            data.zenzai[key] = next;
        });
    };

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
                    <Switch id="zenzai-enable" aria-describedby="zenzai-enable-description" checked={value.enable} onCheckedChange={handleZenzaiChange} />
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
                    <Textarea id="zenzai-profile" aria-describedby="zenzai-profile-description" placeholder="例）山田太郎、数学科の学生。" value={value.profile} disabled={!value.enable} onChange={handleProfileChange} />
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
                    <Select disabled={!value.enable} value={value.backend} onValueChange={handleBackendChange}>
                        <SelectTrigger id="zenzai-backend" className="w-48" aria-labelledby="zenzai-backend-label zenzai-backend" aria-describedby="zenzai-backend-description">
                            <SelectValue placeholder="バックエンドを選択" />
                        </SelectTrigger>
                        <SelectContent>
                            <BackendSelectItem name="CPU (非推奨)" value="cpu" disabled={!capability.cpu} reason="" />
                            <BackendSelectItem name="CUDA (NVIDIA GPU)" value="cuda" disabled={!capability.cuda} reason="CUDA Toolkit 12をインストールする必要があります" />
                            <BackendSelectItem name="Vulkan" value="vulkan" disabled={!capability.vulkan} reason="お使いのPCはVulkanに対応していません" />
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
                    <Select disabled={!value.enable} value={String(value.inferenceLimit)} onValueChange={handleInferenceLimitChange}>
                        <SelectTrigger id="zenzai-inference-limit" className="w-48" aria-labelledby="zenzai-inference-limit-label zenzai-inference-limit" aria-describedby="zenzai-inference-limit-description">
                            <SelectValue placeholder="推論上限を選択" />
                        </SelectTrigger>
                        <SelectContent>
                            {inferenceLimits.map((limit) => (
                                <SelectItem key={limit} value={String(limit)}>{limit}</SelectItem>
                            ))}
                        </SelectContent>
                    </Select>
                </div>
                <ContextInput
                    id="zenzai-topic"
                    icon={<MessageSquare aria-hidden="true" />}
                    title="話題"
                    description="いま書いている内容の話題を10〜20文字程度で設定します"
                    placeholder="例）ソフトウェア開発"
                    value={value.topic}
                    disabled={!value.enable}
                    onChange={handleContextChange("topic")}
                />
                <ContextInput
                    id="zenzai-style"
                    icon={<Type aria-hidden="true" />}
                    title="文体"
                    description="文章のスタイルを10〜20文字程度で設定します"
                    placeholder="例）ですます調"
                    value={value.style}
                    disabled={!value.enable}
                    onChange={handleContextChange("style")}
                />
                <ContextInput
                    id="zenzai-preference"
                    icon={<Heart aria-hidden="true" />}
                    title="好み"
                    description="変換の好みを10〜20文字程度で設定します"
                    placeholder="例）漢字は控えめに"
                    value={value.preference}
                    disabled={!value.enable}
                    onChange={handleContextChange("preference")}
                />
            </section>
        </div>
    )
}
