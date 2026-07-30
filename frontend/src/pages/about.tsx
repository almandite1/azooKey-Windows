import { ExternalLink } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Button } from "@/components/ui/button";

export const About = () => {
    return (
        <div className="space-y-8">
            {/* the one heading on this page doubles as the page title and the
                section's accessible name */}
            <section className="space-y-2" aria-labelledby="about-heading">
                <h1 id="about-heading" className="text-lg font-bold text-foreground">このソフトについて</h1>
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
