import { Bot, Settings, Megaphone } from "lucide-react"
import { Link, useLocation } from "react-router"

import {
    Sidebar,
    SidebarContent,
    SidebarFooter,
    SidebarGroup,
    SidebarGroupContent,
    SidebarGroupLabel,
    SidebarMenu,
    SidebarMenuButton,
    SidebarMenuItem,
} from "@/components/ui/sidebar"

// Menu items.
const contents = [
    {
        title: "全般",
        url: "/",
        icon: Settings,
    },
    // {
    //     title: "外観",
    //     url: "/appearance",
    //     icon: Palette,
    // },
    {
        title: "Zenzai",
        url: "/zenzai",
        icon: Bot,
    },
]

// Footer items.
const footer = [
    {
        title: "Azookeyについて",
        url: "/about",
        icon: Megaphone,
    },
]

export function AppSidebar() {
    const { pathname } = useLocation();

    return (
        <Sidebar>
            {/* client-side <Link>, not <a href>: a real navigation would ask the
                embedded asset protocol for /zenzai, which has no SPA fallback */}
            <SidebarContent>
                <nav aria-label="設定メニュー">
                    <SidebarGroup>
                        <SidebarGroupLabel>設定</SidebarGroupLabel>
                        <SidebarGroupContent>
                            <SidebarMenu>
                                {contents.map((item) => (
                                    <SidebarMenuItem key={item.title}>
                                        <SidebarMenuButton asChild isActive={pathname === item.url}>
                                            <Link to={item.url} aria-current={pathname === item.url ? "page" : undefined}>
                                                <item.icon aria-hidden="true" />
                                                <span>{item.title}</span>
                                            </Link>
                                        </SidebarMenuButton>
                                    </SidebarMenuItem>
                                ))}
                            </SidebarMenu>
                        </SidebarGroupContent>
                    </SidebarGroup>
                </nav>
            </SidebarContent>
            <SidebarFooter>
                <nav aria-label="このアプリについて">
                    <SidebarGroup>
                        <SidebarGroupContent>
                            <SidebarMenu>
                                {footer.map((item) => (
                                    <SidebarMenuItem key={item.title}>
                                        <SidebarMenuButton asChild isActive={pathname === item.url}>
                                            <Link to={item.url} aria-current={pathname === item.url ? "page" : undefined}>
                                                <item.icon aria-hidden="true" />
                                                <span>{item.title}</span>
                                            </Link>
                                        </SidebarMenuButton>
                                    </SidebarMenuItem>
                                ))}
                            </SidebarMenu>
                        </SidebarGroupContent>
                    </SidebarGroup>
                </nav>
            </SidebarFooter>
        </Sidebar>
    )
}
