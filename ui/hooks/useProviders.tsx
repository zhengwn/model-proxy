import React, { createContext, useContext, useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ProviderConfig, ProvidersInfo } from "../types";

interface UseProvidersReturn {
  providers: ProviderConfig[];
  activeProvider: string;
  loading: boolean;
  error: string | null;
  switching: boolean;
  loadProviders: () => Promise<void>;
  switchProvider: (name: string) => Promise<void>;
  addProvider: (config: ProviderConfig) => Promise<void>;
  updateProvider: (config: ProviderConfig, originalName?: string) => Promise<void>;
  deleteProvider: (name: string) => Promise<void>;
}

const ProvidersContext = createContext<UseProvidersReturn | null>(null);

export function ProvidersProvider({ children }: { children: React.ReactNode }) {
  const [providers, setProviders] = useState<ProviderConfig[]>([]);
  const [activeProvider, setActiveProvider] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [switching, setSwitching] = useState(false);

  const loadProviders = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<ProvidersInfo>("get_providers");
      setProviders(result.providers);
      setActiveProvider(result.active_provider);
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      if (errMsg.includes("不存在")) {
        setProviders([]);
        setActiveProvider("");
      } else {
        setError(errMsg);
      }
    } finally {
      setLoading(false);
    }
  }, []);

  const switchProvider = useCallback(async (name: string) => {
    setSwitching(true);
    setError(null);
    try {
      await invoke("switch_provider", { name });
      setActiveProvider(name);
      window.dispatchEvent(new CustomEvent("provider-switched"));
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      setError(errMsg);
      throw e;
    } finally {
      setSwitching(false);
    }
  }, []);

  const addProvider = useCallback(async (config: ProviderConfig) => {
    setError(null);
    try {
      await invoke("add_provider", { provider: config });
      await loadProviders();
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      setError(errMsg);
      throw e;
    }
  }, [loadProviders]);

  const updateProvider = useCallback(async (config: ProviderConfig, originalName?: string) => {
    setError(null);
    try {
      await invoke("update_provider", { provider: config, originalName: originalName || config.name });
      await loadProviders();
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      setError(errMsg);
      throw e;
    }
  }, [loadProviders]);

  const deleteProvider = useCallback(async (name: string) => {
    setError(null);
    try {
      await invoke("delete_provider", { name });
      await loadProviders();
    } catch (e) {
      const errMsg = typeof e === "string" ? e : String(e);
      setError(errMsg);
      throw e;
    }
  }, [loadProviders]);

  useEffect(() => {
    loadProviders();
  }, [loadProviders]);

  return (
    <ProvidersContext.Provider
      value={{
        providers,
        activeProvider,
        loading,
        error,
        switching,
        loadProviders,
        switchProvider,
        addProvider,
        updateProvider,
        deleteProvider,
      }}
    >
      {children}
    </ProvidersContext.Provider>
  );
}

export function useProviders(): UseProvidersReturn {
  const context = useContext(ProvidersContext);
  if (!context) {
    throw new Error("useProviders must be used within a ProvidersProvider");
  }
  return context;
}
