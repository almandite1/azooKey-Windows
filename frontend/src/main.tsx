import React from "react";
import ReactDOM from "react-dom/client";
import "./index.css";
import { SidebarProvider } from "@/components/ui/sidebar"
import { ThemeProvider } from "@/components/theme-provider"
import { BrowserRouter, Routes, Route, Navigate } from "react-router";
import { AppSidebar } from "@/components/app-sidebar"

import { General } from "@/pages/general"
import { Appearance } from "@/pages/appearance"
import { Zenzai } from "@/pages/zenzai"
import { About } from "@/pages/about"
import { Toaster } from "@/components/ui/sonner"

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    {/* the provider has to sit above the sidebar too, or useTheme() there
        silently reads the context default instead of the real theme */}
    <ThemeProvider defaultTheme="system" storageKey="vite-ui-theme">
      <SidebarProvider>
        <BrowserRouter>
          <AppSidebar />
          <main className="w-full p-6">
            <Routes>
              <Route path="/" element={<General />} />
              <Route path="/appearance" element={<Appearance />} />
              <Route path="/zenzai" element={<Zenzai />} />
              <Route path="/about" element={<About />} />
              <Route path="*" element={<Navigate to="/" replace />} />
            </Routes>
          </main>
        </BrowserRouter>
      </SidebarProvider>
      <Toaster />
    </ThemeProvider>
  </React.StrictMode>,
);
