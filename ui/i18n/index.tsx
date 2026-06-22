import { createContext, useContext, useState, useCallback, useMemo, type ReactNode } from "react";
import { messages, antdLocales, type Locale, type Messages } from "./locales";

const STORAGE_KEY = "model-proxy-locale";

function loadLocale(): Locale {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "en" || stored === "zh") return stored;
  // Auto-detect from browser
  return navigator.language.startsWith("zh") ? "zh" : "en";
}

// Interpolate {key} placeholders
function interpolate(template: string, params?: Record<string, string | number>): string {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (_, key: string) =>
    params[key] !== undefined ? String(params[key]) : `{${key}}`
  );
}

interface LocaleContextValue {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  t: (key: keyof Messages, params?: Record<string, string | number>) => string;
  antdLocale: typeof antdLocales.zh;
}

const LocaleContext = createContext<LocaleContextValue | null>(null);

export function LocaleProvider({ children }: { children: ReactNode }) {
  const [locale, setLocaleRaw] = useState<Locale>(loadLocale);

  const setLocale = useCallback((newLocale: Locale) => {
    setLocaleRaw(newLocale);
    localStorage.setItem(STORAGE_KEY, newLocale);
  }, []);

  const t = useCallback(
    (key: keyof Messages, params?: Record<string, string | number>): string => {
      const template = messages[locale][key] ?? key;
      return interpolate(template, params);
    },
    [locale]
  );

  const value = useMemo<LocaleContextValue>(
    () => ({ locale, setLocale, t, antdLocale: antdLocales[locale] }),
    [locale, setLocale, t]
  );

  return <LocaleContext.Provider value={value}>{children}</LocaleContext.Provider>;
}

export function useLocale() {
  const ctx = useContext(LocaleContext);
  if (!ctx) throw new Error("useLocale must be used within LocaleProvider");
  return ctx;
}
