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

const THEMES: readonly Theme[] = ["light", "dark", "system"]

/// localStorage holds whatever anyone put there, and the value was cast to
/// `Theme` and used unchecked. That is not only a wrong theme: the pre-paint
/// script in index.html passes it to `classList.add`, which THROWS on a value
/// containing a space — taking the whole inline script with it, so the flash
/// it exists to prevent comes back and the failure is invisible.
///
/// The same allow-list is spelled out in index.html, which cannot import this.
export function readStoredTheme(storageKey: string, fallback: Theme): Theme {
  let stored: string | null = null
  try {
    stored = localStorage.getItem(storageKey)
  } catch {
    // storage can be unavailable (a locked-down webview); the default is fine
    return fallback
  }
  return THEMES.includes(stored as Theme) ? (stored as Theme) : fallback
}

export function ThemeProvider({
  children,
  defaultTheme = "system",
  storageKey = "vite-ui-theme",
  ...props
}: ThemeProviderProps) {
  const [theme, setTheme] = useState<Theme>(() => readStoredTheme(storageKey, defaultTheme))

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
