import React from "react";
import ReactDOM from "react-dom/client";
import "./index.css";
import { SidebarProvider } from "@/components/ui/sidebar"
import { ThemeProvider } from "@/components/theme-provider"
// HashRouter, not BrowserRouter: a real reload on /zenzai asks the embedded
// asset protocol for that path, and it has no SPA fallback to answer with.
// The path lives in the fragment now, which never reaches it.
import { HashRouter, Routes, Route, Navigate } from "react-router";
import { AppSidebar } from "@/components/app-sidebar"
import { ConfigProvider } from "@/hooks/use-config"
import { ConfigGate } from "@/components/config-gate"

import { General } from "@/pages/general"
import { Conversion } from "@/pages/conversion"
import { Zenzai } from "@/pages/zenzai"
import { About } from "@/pages/about"
import { Toaster } from "@/components/ui/sonner"

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    {/* the provider has to sit above the sidebar too, or useTheme() there
        silently reads the context default instead of the real theme */}
    <ThemeProvider defaultTheme="system" storageKey="vite-ui-theme">
      <ConfigProvider>
        <SidebarProvider>
          <HashRouter>
            <AppSidebar />
            <main className="w-full p-6">
              <ConfigGate>
                <Routes>
                  <Route path="/" element={<General />} />
                  <Route path="/conversion" element={<Conversion />} />
                  <Route path="/zenzai" element={<Zenzai />} />
                  <Route path="/about" element={<About />} />
                  <Route path="*" element={<Navigate to="/" replace />} />
                </Routes>
              </ConfigGate>
            </main>
          </HashRouter>
        </SidebarProvider>
      </ConfigProvider>
      <Toaster />
    </ThemeProvider>
  </React.StrictMode>,
);
