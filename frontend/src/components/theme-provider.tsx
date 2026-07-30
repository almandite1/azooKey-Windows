import { createContext, useContext, useLayoutEffect, useState } from "react"

type Theme = "dark" | "light" | "system"

type ThemeProviderProps = {
  children: React.ReactNode
  defaultTheme?: Theme
  storageKey?: string
}

type ThemeProviderState = {
  theme: Theme
  setTheme: (theme: Theme) => void
}

// no default value: that is what makes the guard in useTheme reachable
const ThemeProviderContext = createContext<ThemeProviderState | undefined>(
  undefined
)

export function ThemeProvider({
  children,
  defaultTheme = "system",
  storageKey = "vite-ui-theme",
  ...props
}: ThemeProviderProps) {
  const [theme, setTheme] = useState<Theme>(
    () => (localStorage.getItem(storageKey) as Theme) || defaultTheme
  )

  useLayoutEffect(() => {
    const root = window.document.documentElement
    const query = window.matchMedia("(prefers-color-scheme: dark)")

    // the class list is also written by the inline script in index.html, which
    // runs before paint to avoid a flash of the wrong theme on startup
    const apply = () => {
      root.classList.remove("light", "dark")
      root.classList.add(
        theme === "system" ? (query.matches ? "dark" : "light") : theme
      )
    }

    apply()

    if (theme !== "system") return

    // follow the OS switching its colour mode while the app is running
    query.addEventListener("change", apply)
    return () => query.removeEventListener("change", apply)
  }, [theme])

  const value = {
    theme,
    setTheme: (theme: Theme) => {
      localStorage.setItem(storageKey, theme)
      setTheme(theme)
    },
  }

  return (
    <ThemeProviderContext.Provider {...props} value={value}>
      {children}
    </ThemeProviderContext.Provider>
  )
}

export const useTheme = () => {
  const context = useContext(ThemeProviderContext)

  if (context === undefined)
    throw new Error("useTheme must be used within a ThemeProvider")

  return context
}
