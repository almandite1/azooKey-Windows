import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import { Bot, User, Cpu } from "lucide-react";
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

export const Zenzai = () => {
    const [value, setValue] = useState({
        enable: false,
        profile: "",
        backend: "",
    });

    const [capability, setCapability] = useState({
        cpu: true,
        cuda: false,
        vulkan: false,
    });

    // Load config on component mount
    useEffect(() => {
        invoke<any>("get_config")
            .then((data) => {
                const zenzai = data.zenzai;
                setValue({
                    enable: zenzai.enable,
                    profile: zenzai.profile,
                    backend: zenzai.backend,
                });
            })
            .catch(() => {
                // Keep default values if config fetch fails
            });

        invoke("check_capability").then((capability: any) => {
            setCapability({
                cpu: capability["cpu"],
                cuda: capability["cuda"],
                vulkan: capability["vulkan"],
            });
        })
    }, []);

    const updateConfig = async (updater: (config: any) => void) => {
        try {
            const data = await invoke<any>("get_config");
            updater(data);
            await invoke("update_config", { newConfig: data });
            return data;
        } catch (error) {
            // the reason matters here: a settings.json written by a newer
            // version is refused rather than overwritten
            toast(`設定の更新に失敗しました: ${error}`);
            return null;
        }
    };

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

    return (
        <div className="space-y-8">
            {/* the one heading on this page doubles as the page title and the
                section's accessible name */}
            <section className="space-y-2" aria-labelledby="zenzai-heading">
                <h1 id="zenzai-heading" className="text-lg font-bold text-foreground">Zenzai</h1>
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
        </div>
    )
}