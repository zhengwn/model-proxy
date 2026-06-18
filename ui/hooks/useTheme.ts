import { useState, useEffect, useCallback } from "react";

export type ThemeMode = "dark" | "light" | "system";

const STORAGE_KEY = "model-proxy-theme";

function getSystemTheme(): "dark" | "light" {
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function resolveTheme(mode: ThemeMode): "dark" | "light" {
  return mode === "system" ? getSystemTheme() : mode;
}

function applyTheme(theme: "dark" | "light") {
  document.documentElement.setAttribute("data-theme", theme);
}

function loadMode(): ThemeMode {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "dark" || stored === "light" || stored === "system") return stored;
  return "dark";
}

export function useTheme() {
  // Resolve once at construction — applyTheme runs before first paint, avoiding FOUC
  const [modeRaw, setModeRaw] = useState<ThemeMode>(() => {
    const m = loadMode();
    applyTheme(resolveTheme(m));
    return m;
  });
  const [resolved, setResolved] = useState<"dark" | "light">(() => resolveTheme(modeRaw));

  const setMode = useCallback((newMode: ThemeMode) => {
    setModeRaw(newMode);
    localStorage.setItem(STORAGE_KEY, newMode);
    const r = resolveTheme(newMode);
    applyTheme(r);
    setResolved(r);
  }, []);

  // Re-apply when system theme changes and mode is "system"
  useEffect(() => {
    if (modeRaw !== "system") return;

    const mql = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) => {
      const t = e.matches ? "dark" : "light";
      applyTheme(t);
      setResolved(t);
    };
    mql.addEventListener("change", handler);
    return () => mql.removeEventListener("change", handler);
  }, [modeRaw]);

  return { mode: modeRaw, resolved, setMode };
}
